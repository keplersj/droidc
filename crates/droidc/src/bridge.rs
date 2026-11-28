//! `droidc bridge`: shows a headless session compositor on the host compositor.
//!
//! For `screencast` sessions (see `session.rs`). The session compositor runs headless with a
//! virtual monitor and exposes Mutter's ScreenCast and RemoteDesktop D-Bus API (gnome-shell does;
//! any compositor implementing the API works). The bridge:
//!
//! 1. creates a RemoteDesktop session and a ScreenCast session linked to it, records the monitor
//!    and waits for its PipeWire node;
//! 2. reads BGRx frames from `gst-launch-1.0 pipewiresrc` (the image's own PipeWire client: a
//!    static binary can't load libpipewire), linked to the stream with `pw-link`;
//! 3. presents them as one fullscreen `xdg_toplevel` through `wl_shm` on the host compositor
//!    (`$WAYLAND_DISPLAY`), and sends the toplevel's pointer, touch and keyboard input back
//!    through RemoteDesktop.
//!
//! `--dump-frame <png>` writes one frame and exits, with no Wayland connection (used by CI).
//!
//! `--scale <s>` sets the monitor's scale (the closest one Mutter supports), unless the user's
//! own display settings (`monitors.xml`) already cover this monitor and size.
//!
//! Which monitor it records:
//!
//! - With `--connector`, or when the session compositor already has a monitor (one started with
//!   `--virtual-monitor WxH`), it records that monitor. Its size is fixed, so when the window
//!   changes size (the device turns) the frames are scaled to fit and input is scaled back.
//! - When the session compositor has no monitor at all (`gnome-shell --headless` alone), it asks
//!   for a virtual monitor (`RecordVirtual`) the size of the window. Mutter sizes that monitor to
//!   the stream, so when the window changes size the bridge restarts the stream at the new size
//!   and Mutter resizes the monitor in place: the session follows the device as it turns, keeps
//!   its scale, and nothing restarts.
//!
//! Mutter stops a monitor stream when the monitor's logical size changes, as it does when the
//! scale changes with fractional scaling on (GNOME Settings > Displays). The bridge then records
//! the monitor again in the same window, so changing the scale does not end the session.

use crate::error::{Error, Result};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface, wl_touch,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const SCREEN_CAST: &str = "org.gnome.Mutter.ScreenCast";
const REMOTE_DESKTOP: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// How often the bridge checks that its stream is still there.
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// Re-recording more often than this many times a minute ends the bridge instead.
const MAX_RESTARTS_PER_MINUTE: usize = 5;

#[derive(Debug, Clone)]
pub struct Opts {
    pub width: u32,
    pub height: u32,
    /// Monitor to record (`Meta-0` is the first `--virtual-monitor` of a headless Mutter). None
    /// records the compositor's first monitor, or a virtual one sized to the window when it has
    /// none.
    pub connector: Option<String>,
    pub dump_frame: Option<PathBuf>,
    /// How long to wait for the session compositor's ScreenCast service.
    pub timeout: Duration,
    /// Monitor scale to set before recording; 0 or less leaves it alone.
    pub scale: f64,
}

fn dbus_err(e: zbus::Error) -> Error {
    Error::internal(format!("D-Bus: {e}"))
}

/// The Mutter sessions behind one recorded monitor.
struct Remote {
    conn: zbus::blocking::Connection,
    rd: OwnedObjectPath,
    stream: OwnedObjectPath,
    node: u32,
    /// The monitor recorded; None for a virtual one until Mutter has made it (see
    /// [`only_connector`]).
    connector: Option<String>,
}

/// What [`Remote::open`] records.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Monitor(String),
    /// A monitor Mutter makes for the stream, sized to what the stream's consumer asks for.
    Virtual,
}

/// The monitor to record: `connector` when given, else the first monitor, else a virtual one.
fn pick_target(connector: Option<&str>, monitors: &[String]) -> Target {
    match (connector, monitors.first()) {
        (Some(c), _) => Target::Monitor(c.to_string()),
        (None, Some(m)) => Target::Monitor(m.clone()),
        (None, None) => Target::Virtual,
    }
}

fn current_state(conn: &zbus::blocking::Connection) -> Result<CurrentState> {
    conn.call_method(
        Some(DISPLAY_CONFIG),
        DISPLAY_CONFIG_PATH,
        Some(DISPLAY_CONFIG),
        "GetCurrentState",
        &(),
    )
    .map_err(dbus_err)?
    .body()
    .deserialize()
    .map_err(dbus_err)
}

/// Connectors of the session compositor's monitors, in Mutter's order.
fn connectors(conn: &zbus::blocking::Connection) -> Result<Vec<String>> {
    Ok(current_state(conn)?
        .1
        .into_iter()
        .map(|(spec, _, _)| spec.0)
        .collect())
}

/// The connector of the compositor's only monitor: the virtual one the bridge asked for.
fn only_connector(conn: &zbus::blocking::Connection) -> Option<String> {
    match connectors(conn).ok()?.as_slice() {
        [c] => Some(c.clone()),
        _ => None,
    }
}

/// Waits up to `timeout` for the session compositor's ScreenCast service.
fn wait_for_screen_cast(conn: &zbus::blocking::Connection, timeout: Duration) -> Result<()> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(conn).map_err(dbus_err)?;
    let deadline = Instant::now() + timeout;
    let name = zbus::names::BusName::try_from(SCREEN_CAST).unwrap();
    while !dbus.name_has_owner(name.clone()).unwrap_or(false) {
        if Instant::now() > deadline {
            return Err(Error::not_found(format!(
                "{SCREEN_CAST} did not appear on the session bus within {timeout:?}"
            )));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

impl Remote {
    fn open(opts: &Opts) -> Result<Remote> {
        let conn = zbus::blocking::Connection::session().map_err(dbus_err)?;
        wait_for_screen_cast(&conn, opts.timeout)?;
        let call = |dest: &str, path: &str, iface: &str, m: &str, body: &dyn DynBody| {
            body.call(&conn, dest, path, iface, m)
        };
        let rd: OwnedObjectPath = call(
            REMOTE_DESKTOP,
            "/org/gnome/Mutter/RemoteDesktop",
            REMOTE_DESKTOP,
            "CreateSession",
            &(),
        )?
        .body()
        .deserialize()
        .map_err(dbus_err)?;
        let id: OwnedValue = call(
            REMOTE_DESKTOP,
            rd.as_str(),
            "org.freedesktop.DBus.Properties",
            "Get",
            &(RD_SESSION, "SessionId"),
        )?
        .body()
        .deserialize()
        .map_err(dbus_err)?;
        let id: String = id
            .try_into()
            .map_err(|e| Error::internal(format!("SessionId: {e}")))?;
        let mut props: HashMap<&str, Value> = HashMap::new();
        props.insert("remote-desktop-session-id", Value::from(id.as_str()));
        let sc: OwnedObjectPath = call(
            SCREEN_CAST,
            "/org/gnome/Mutter/ScreenCast",
            SCREEN_CAST,
            "CreateSession",
            &(props,),
        )?
        .body()
        .deserialize()
        .map_err(dbus_err)?;
        let mut rec: HashMap<&str, Value> = HashMap::new();
        // 1 = embedded: the cursor is drawn into the frames.
        rec.insert("cursor-mode", Value::from(1u32));
        let target = pick_target(opts.connector.as_deref(), &connectors(&conn)?);
        let reply = match &target {
            Target::Monitor(c) => call(
                SCREEN_CAST,
                sc.as_str(),
                SC_SESSION,
                "RecordMonitor",
                &(c.as_str(), rec),
            )?,
            Target::Virtual => {
                // A platform monitor: GNOME Shell puts its panel, dash and windows on it.
                rec.insert("is-platform", Value::from(true));
                call(
                    SCREEN_CAST,
                    sc.as_str(),
                    SC_SESSION,
                    "RecordVirtual",
                    &(rec,),
                )?
            }
        };
        let stream: OwnedObjectPath = reply.body().deserialize().map_err(dbus_err)?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.gnome.Mutter.ScreenCast.Stream")
            .and_then(|b| b.member("PipeWireStreamAdded"))
            .and_then(|b| b.path(stream.as_str()))
            .map_err(dbus_err)?
            .build();
        let mut signals = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(4))
            .map_err(dbus_err)?;
        call(REMOTE_DESKTOP, rd.as_str(), RD_SESSION, "Start", &())?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Some(Ok(m)) = signals.next() {
                let _ = tx.send(m.body().deserialize::<u32>());
            }
        });
        let node = rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| Error::internal("no PipeWireStreamAdded for the recorded monitor"))?
            .map_err(dbus_err)?;
        Ok(Remote {
            conn,
            rd,
            stream,
            node,
            connector: match target {
                Target::Monitor(c) => Some(c),
                Target::Virtual => None,
            },
        })
    }

    fn is_virtual(&self) -> bool {
        self.connector.is_none()
    }

    /// What the log calls the recorded monitor.
    fn label(&self) -> &str {
        self.connector.as_deref().unwrap_or("a virtual monitor")
    }

    /// Fire-and-forget input call on the RemoteDesktop session.
    fn notify<B>(&self, method: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let msg = zbus::message::Message::method_call(self.rd.as_str(), method)
            .and_then(|b| b.destination(REMOTE_DESKTOP))
            .and_then(|b| b.interface(RD_SESSION))
            .map(|b| b.with_flags(zbus::message::Flags::NoReplyExpected))
            .and_then(|b| b?.build(body));
        match msg {
            Ok(m) => {
                if let Err(e) = self.conn.send(&m) {
                    eprintln!("bridge: {method}: {e}");
                }
            }
            Err(e) => eprintln!("bridge: {method}: {e}"),
        }
    }

    /// Whether Mutter still has our stream (it drops it when the monitor's logical size changes).
    fn alive(&self) -> bool {
        self.conn
            .call_method(
                Some(SCREEN_CAST),
                self.stream.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(SC_STREAM, "Parameters"),
            )
            .is_ok()
    }

    fn stop(&self) {
        let _ = ().call(
            &self.conn,
            REMOTE_DESKTOP,
            self.rd.as_str(),
            RD_SESSION,
            "Stop",
        );
    }
}

/// Lets the setup calls above take bodies of different types through one closure.
trait DynBody {
    fn call(
        &self,
        c: &zbus::blocking::Connection,
        d: &str,
        p: &str,
        i: &str,
        m: &str,
    ) -> Result<zbus::Message>;
}

impl<T: serde::Serialize + zbus::zvariant::DynamicType> DynBody for T {
    fn call(
        &self,
        c: &zbus::blocking::Connection,
        d: &str,
        p: &str,
        i: &str,
        m: &str,
    ) -> Result<zbus::Message> {
        c.call_method(Some(d), p, Some(i), m, self)
            .map_err(|e| Error::internal(format!("{i}.{m}: {e}")))
    }
}

/// PipeWire node name of our capture stream.
fn capture_name() -> String {
    format!("droidc-bridge-{}", std::process::id())
}

/// `gst-launch-1.0` reading from a pipewiresrc that waits to be linked (see [`spawn_capture`]),
/// with `src` extra source properties, followed by `tail`.
fn gst_argv(src: &[&str], tail: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = [
        "gst-launch-1.0",
        "-q",
        "pipewiresrc",
        "autoconnect=false",
        "always-copy=true",
    ]
    .map(String::from)
    .to_vec();
    argv.push(format!(
        "stream-properties=props,node.name={}",
        capture_name()
    ));
    argv.extend(src.iter().map(|s| s.to_string()));
    argv.push("!".into());
    argv.extend_from_slice(tail);
    argv
}

/// Whether `pw-link -l` lists a link into our capture stream.
fn capture_linked() -> bool {
    Command::new("pw-link")
        .arg("-l")
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| has_input_link(&String::from_utf8_lossy(&o.stdout), &capture_name()))
}

/// Whether a `pw-link -l` listing shows a link into one of `node`'s ports.
fn has_input_link(listing: &str, node: &str) -> bool {
    let prefix = format!("{node}:");
    let mut lines = listing.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with(&prefix)
            && lines
                .peek()
                .is_some_and(|n| n.trim_start().starts_with("|<-"))
        {
            return true;
        }
    }
    false
}

/// Starts gst and links the session compositor's stream node to it with `pw-link`. Linking by
/// `target-object` would go through WirePlumber's policy, which does not link a stream to another
/// stream (Mutter's ScreenCast node is `Stream/Output/Video`), so the link is made directly.
fn spawn_capture(
    node: u32,
    src: &[&str],
    tail: &[String],
    stdout: Stdio,
) -> Result<std::process::Child> {
    let argv = gst_argv(src, tail);
    let mut gst = Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(stdout)
        .spawn()
        .map_err(|e| Error::not_found(format!("{}: {e}", argv[0])))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(st)) = gst.try_wait() {
            // pw-link can report a failure for a link it did make; a short pipeline
            // (`--dump-frame`) may then be done before the next try.
            if st.success() {
                return Ok(gst);
            }
            return Err(Error::internal(format!(
                "gst-launch-1.0 exited with {st} before it was linked"
            )));
        }
        let out = Command::new("pw-link")
            .arg(node.to_string())
            .arg(capture_name())
            .stdout(Stdio::null())
            .output()
            .map_err(|e| Error::not_found(format!("pw-link: {e}")))?;
        // For a virtual monitor, Mutter makes the monitor while the link negotiates and pw-link
        // can report a failure for the link it made, then fail every retry: check the link itself.
        if out.status.success() || capture_linked() {
            return Ok(gst);
        }
        if Instant::now() > deadline {
            let last = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let _ = gst.kill();
            let _ = gst.wait();
            return Err(Error::internal(format!(
                "pw-link {node} {}: {last}",
                capture_name()
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn dump_frame(node: u32, size: Option<(u32, u32)>, out: &std::path::Path) -> Result<i32> {
    let mut tail: Vec<String> = vec!["videoconvert".into(), "!".into()];
    // A virtual monitor takes the size the consumer asks for.
    if let Some((w, h)) = size {
        tail.extend([format!("video/x-raw,width={w},height={h}"), "!".into()]);
    }
    tail.extend(["pngenc", "snapshot=true", "!", "filesink"].map(String::from));
    tail.push(format!("location={}", out.display()));
    // The third frame: the first ones can still be partially drawn.
    let mut gst = spawn_capture(node, &["num-buffers=3"], &tail, Stdio::inherit())?;
    let st = gst
        .wait()
        .map_err(|e| Error::internal(format!("gst-launch-1.0: {e}")))?;
    Ok(st.code().unwrap_or(1))
}

type Props = HashMap<String, OwnedValue>;
type Spec = (String, String, String, String);
/// id, width, height, refresh, preferred scale, supported scales, properties
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
/// x, y, scale, transform, primary, monitors, properties
type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);
type CurrentState = (u32, Vec<(Spec, Vec<Mode>, Props)>, Vec<Logical>, Props);

/// The supported scale closest to `want` (Mutter only takes scales from this list).
fn closest_scale(supported: &[f64], want: f64) -> Option<f64> {
    supported
        .iter()
        .copied()
        .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
}

/// Whether the user's saved display settings (`monitors.xml`) have a layout for `connector` at
/// `width`x`height`: Mutter applies those itself, and they win over the bridge's default.
fn has_saved_layout(xml: &str, connector: &str, width: i32, height: i32) -> bool {
    let conn = format!("<connector>{connector}</connector>");
    let (w, h) = (
        format!("<width>{width}</width>"),
        format!("<height>{height}</height>"),
    );
    xml.split("<configuration>")
        .skip(1)
        .any(|c| c.contains(&conn) && c.contains(&w) && c.contains(&h))
}

fn monitors_xml() -> Option<String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    std::fs::read_to_string(dir.join("monitors.xml")).ok()
}

/// Sets `connector` to the supported scale closest to `want`, for this run of the session only
/// (Mutter's "temporary" config: nothing is saved and no confirmation is asked). Leaves the scale
/// alone when it is already not 1, when the user saved a layout for this monitor and size, or
/// when there is more than one monitor. Returns the scale it set.
fn set_scale(conn: &zbus::blocking::Connection, connector: &str, want: f64) -> Result<Option<f64>> {
    let (serial, monitors, logical, _) = current_state(conn)?;
    let [lm] = logical.as_slice() else {
        return Ok(None);
    };
    let Some((spec, modes, _)) = monitors.iter().find(|(s, _, _)| s.0 == connector) else {
        return Err(Error::not_found(format!("no monitor {connector}")));
    };
    if !lm.5.iter().any(|s| s.0 == connector) || (lm.2 - 1.0).abs() > 0.01 {
        return Ok(None);
    }
    let current = |m: &&Mode| {
        m.6.get("is-current")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
    };
    let Some(mode) = modes.iter().find(current) else {
        return Ok(None);
    };
    if monitors_xml().is_some_and(|x| has_saved_layout(&x, connector, mode.1, mode.2)) {
        return Ok(None);
    }
    let Some(scale) = closest_scale(&mode.5, want) else {
        return Ok(None);
    };
    if (scale - lm.2).abs() < 0.01 {
        return Ok(None);
    }
    let mons: Vec<(&str, &str, HashMap<&str, Value>)> =
        vec![(spec.0.as_str(), mode.0.as_str(), HashMap::new())];
    // 1 = temporary: applied now, not written to monitors.xml.
    let body = (
        serial,
        1u32,
        vec![(lm.0, lm.1, scale, lm.3, lm.4, mons)],
        HashMap::<&str, Value>::new(),
    );
    conn.call_method(
        Some(DISPLAY_CONFIG),
        DISPLAY_CONFIG_PATH,
        Some(DISPLAY_CONFIG),
        "ApplyMonitorsConfig",
        &body,
    )
    .map_err(dbus_err)?;
    Ok(Some(scale))
}

/// [`set_scale`], logging the outcome.
fn apply_scale(conn: &zbus::blocking::Connection, connector: &str, want: f64) {
    match set_scale(conn, connector, want) {
        Ok(Some(s)) => eprintln!("bridge: set {connector} to scale {s}"),
        Ok(None) => {}
        Err(e) => eprintln!("bridge: could not set the scale: {e}"),
    }
}

pub fn run(opts: &Opts) -> Result<i32> {
    if opts.width == 0 || opts.height == 0 {
        return Err(Error::invalid(
            "bridge needs a non-zero --width and --height",
        ));
    }
    if opts.scale > 0.0 {
        let conn = zbus::blocking::Connection::session().map_err(dbus_err)?;
        wait_for_screen_cast(&conn, opts.timeout)?;
        // A virtual monitor only exists once its stream is linked: `present` scales it then.
        if let Target::Monitor(c) = pick_target(opts.connector.as_deref(), &connectors(&conn)?) {
            apply_scale(&conn, &c, opts.scale);
        }
    }
    let remote = Arc::new(Remote::open(opts)?);
    eprintln!(
        "bridge: recording {} as PipeWire node {}",
        remote.label(),
        remote.node
    );
    let code = match &opts.dump_frame {
        Some(out) => dump_frame(
            remote.node,
            remote.is_virtual().then_some((opts.width, opts.height)),
            out,
        ),
        None => present(opts, remote.clone()),
    };
    remote.stop();
    code
}

/// Latest complete frame from the capture thread, plus whether capture has ended.
#[derive(Default)]
struct Frames {
    latest: Option<Vec<u8>>,
    ended: bool,
}

/// One gst capture of the recorded monitor, at the window size.
struct Capture {
    gst: std::process::Child,
    frames: Arc<Mutex<Frames>>,
}

/// The gst elements after pipewiresrc that turn the stream into `w`x`h` BGRx frames on stdout.
/// A monitor's frames are scaled to fit; a virtual monitor gets the size asked for upstream,
/// which is what makes Mutter size it to the window.
fn capture_tail(w: u32, h: u32, virt: bool) -> Vec<String> {
    let mut tail: Vec<String> = vec!["videoconvert".into(), "!".into()];
    if !virt {
        tail.extend(["videoscale", "!"].map(String::from));
    }
    tail.push(format!("video/x-raw,format=BGRx,width={w},height={h}"));
    tail.extend(["!", "fdsink", "fd=1"].map(String::from));
    tail
}

impl Capture {
    fn start(remote: &Remote, w: u32, h: u32, wake: Arc<OwnedFd>) -> Result<Capture> {
        let frame_len = (w * h * 4) as usize;
        let tail = capture_tail(w, h, remote.is_virtual());
        let mut gst = spawn_capture(remote.node, &[], &tail, Stdio::piped())?;
        let mut stdout = gst.stdout.take().unwrap();
        let frames = Arc::new(Mutex::new(Frames::default()));
        {
            let frames = frames.clone();
            std::thread::spawn(move || loop {
                let mut buf = vec![0u8; frame_len];
                let ok = stdout.read_exact(&mut buf).is_ok();
                {
                    let mut f = frames.lock().unwrap();
                    if ok {
                        f.latest = Some(buf);
                    } else {
                        f.ended = true;
                    }
                }
                let _ = rustix::io::write(&*wake, &[1]);
                if !ok {
                    break;
                }
            });
        }
        Ok(Capture { gst, frames })
    }

    fn stop(&mut self) {
        let _ = self.gst.kill();
        let _ = self.gst.wait();
    }
}

/// Wakes the present loop whenever Mutter's monitor configuration changes, so a stream it
/// dropped is noticed at once. Best effort: the periodic check covers a failure here.
fn watch_monitors(changed: Arc<AtomicBool>, wake: Arc<OwnedFd>) {
    std::thread::spawn(move || {
        let Ok(conn) = zbus::blocking::Connection::session() else {
            return;
        };
        let Ok(rule) = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(DISPLAY_CONFIG)
            .and_then(|b| b.member("MonitorsChanged"))
            .map(|b| b.build())
        else {
            return;
        };
        let Ok(signals) = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(8))
        else {
            return;
        };
        for _ in signals.flatten() {
            changed.store(true, Ordering::SeqCst);
            let _ = rustix::io::write(&*wake, &[1]);
        }
    });
}

fn present(opts: &Opts, remote: Arc<Remote>) -> Result<i32> {
    let (w, h) = (opts.width, opts.height);
    let (wake_r, wake_w) =
        rustix::pipe::pipe().map_err(|e| Error::internal(format!("pipe: {e}")))?;
    let wake_w = Arc::new(wake_w);
    let mut cap = Capture::start(&remote, w, h, wake_w.clone())?;
    // A virtual monitor is scaled once Mutter has made it, which is when its first frame comes.
    let mut scale_pending = opts.scale > 0.0 && remote.is_virtual();
    let changed = Arc::new(AtomicBool::new(false));
    watch_monitors(changed.clone(), wake_w.clone());

    let conn = Connection::connect_to_env()
        .map_err(|e| Error::not_found(format!("host compositor (WAYLAND_DISPLAY): {e}")))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State::new(remote, w, h);
    queue.roundtrip(&mut st).map_err(wl_err)?;
    st.create_window(&qh)?;
    queue.roundtrip(&mut st).map_err(wl_err)?;

    let mut next_check = Instant::now() + CHECK_EVERY;
    // Quick re-checks after a monitor change: Mutter may drop the stream a moment later.
    let mut quick_checks = 0u32;
    let mut restarts: Vec<Instant> = Vec::new();
    let code = loop {
        queue.dispatch_pending(&mut st).map_err(wl_err)?;
        if st.closed {
            break 0;
        }
        if changed.swap(false, Ordering::SeqCst) {
            quick_checks = 4;
            next_check = Instant::now() + Duration::from_millis(250);
        }
        if let Some((nw, nh)) = st.resize.take() {
            eprintln!(
                "bridge: window is now {nw}x{nh}; {}",
                if st.remote.is_virtual() {
                    "resizing the virtual monitor"
                } else {
                    "scaling the monitor to fit"
                }
            );
            st.resize_buffers(&qh, nw, nh)?;
            cap.stop();
            cap = Capture::start(&st.remote, nw, nh, wake_w.clone())?;
            // Mutter keeps the scale of a monitor it resizes; this only covers one still at 1.
            scale_pending = opts.scale > 0.0 && st.remote.is_virtual();
            continue;
        }
        let (frame, ended) = {
            let mut f = cap.frames.lock().unwrap();
            (f.latest.take(), f.ended)
        };
        let lost = if ended {
            let s = cap.gst.wait().ok().and_then(|s| s.code()).unwrap_or(1);
            eprintln!("bridge: capture ended (gst-launch-1.0 exited with {s})");
            Some(if s == 0 { 1 } else { s })
        } else if Instant::now() >= next_check {
            if quick_checks > 0 {
                quick_checks -= 1;
                next_check = Instant::now() + Duration::from_millis(500);
            } else {
                next_check = Instant::now() + CHECK_EVERY;
            }
            (!st.remote.alive()).then(|| {
                eprintln!("bridge: Mutter dropped the stream of {}", st.remote.label());
                1
            })
        } else {
            None
        };
        if let Some(code) = lost {
            restarts.retain(|t| t.elapsed() < Duration::from_secs(60));
            if restarts.len() >= MAX_RESTARTS_PER_MINUTE {
                eprintln!("bridge: the stream ended {MAX_RESTARTS_PER_MINUTE} times within a minute; giving up");
                break code;
            }
            restarts.push(Instant::now());
            cap.stop();
            st.remote.stop();
            let remote = match Remote::open(opts) {
                Ok(r) => Arc::new(r),
                Err(e) => {
                    eprintln!("bridge: could not record {} again: {e}", st.remote.label());
                    break code;
                }
            };
            eprintln!(
                "bridge: recording {} again as PipeWire node {}",
                remote.label(),
                remote.node
            );
            scale_pending = opts.scale > 0.0 && remote.is_virtual();
            cap = match Capture::start(&remote, st.width, st.height, wake_w.clone()) {
                Ok(c) => c,
                Err(e) => {
                    remote.stop();
                    return Err(e);
                }
            };
            st.remote = remote;
            st.touches.clear();
            next_check = Instant::now() + CHECK_EVERY;
            continue;
        }
        if frame.is_some() && scale_pending {
            scale_pending = false;
            match only_connector(&st.remote.conn) {
                Some(c) => apply_scale(&st.remote.conn, &c, opts.scale),
                None => eprintln!("bridge: could not find the virtual monitor to scale it"),
            }
        }
        if let Some(frame) = frame {
            if !st.present(&qh, &frame) {
                // No free buffer or the last frame isn't shown yet: keep it for the next wakeup.
                cap.frames.lock().unwrap().latest.get_or_insert(frame);
            }
        }
        conn.flush()
            .map_err(|e| Error::internal(format!("wayland: {e}")))?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let wl_fd = guard.connection_fd();
        let mut fds = [
            rustix::event::PollFd::new(&wl_fd, rustix::event::PollFlags::IN),
            rustix::event::PollFd::new(&wake_r, rustix::event::PollFlags::IN),
        ];
        let wait = next_check.saturating_duration_since(Instant::now());
        let timeout = rustix::event::Timespec {
            tv_sec: wait.as_secs() as _,
            tv_nsec: wait.subsec_nanos() as _,
        };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(Error::internal(format!("poll: {e}"))),
        }
        let (wl_ready, wake_ready) = (!fds[0].revents().is_empty(), !fds[1].revents().is_empty());
        if wl_ready {
            if let Err(e) = guard.read() {
                return Err(Error::internal(format!("wayland: {e}")));
            }
        } else {
            drop(guard);
        }
        if wake_ready {
            let mut b = [0u8; 64];
            let _ = rustix::io::read(&wake_r, &mut b);
        }
    };
    cap.stop();
    // `run` stops the session it started; stop the one we hold if it is a later one.
    st.remote.stop();
    Ok(code)
}

fn wl_err(e: impl std::fmt::Display) -> Error {
    Error::internal(format!("wayland: {e}"))
}

struct Buf {
    buffer: wl_buffer::WlBuffer,
    offset: usize,
    busy: bool,
}

/// How much to scale input from window coordinates to stream coordinates. A monitor keeps the
/// size the bridge started at, so input on a window of another size is scaled to it; a virtual
/// monitor is always the window's size.
fn input_scale(start: (u32, u32), now: (u32, u32), virt: bool) -> (f64, f64) {
    if virt || now.0 == 0 || now.1 == 0 {
        return (1.0, 1.0);
    }
    (start.0 as f64 / now.0 as f64, start.1 as f64 / now.1 as f64)
}

/// The size a configure asks for, if it is one to resize to: xdg_toplevel sends 0x0 when the
/// client may choose, and the same size again on other changes (focus, states).
fn configured_size(asked: (i32, i32), now: (u32, u32)) -> Option<(u32, u32)> {
    let (w, h) = asked;
    if w <= 0 || h <= 0 {
        return None;
    }
    let size = (w as u32, h as u32);
    (size != now).then_some(size)
}

struct State {
    remote: Arc<Remote>,
    /// The window's current size, which is also the size of the frames and buffers.
    width: u32,
    height: u32,
    /// The size the bridge started at (`--width`, `--height`).
    start: (u32, u32),
    /// Size from the last xdg_toplevel configure, applied on the xdg_surface configure after it.
    asked: (i32, i32),
    /// A new window size for the present loop to switch to.
    resize: Option<(u32, u32)>,
    /// Counts buffer sets, so a release of a buffer from an older set is ignored.
    generation: u32,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seats: Vec<wl_seat::WlSeat>,
    surface: Option<wl_surface::WlSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    configured: bool,
    frame_pending: bool,
    bufs: Vec<Buf>,
    mem: Option<(OwnedFd, *mut u8, usize)>,
    pointer: (f64, f64),
    touches: HashMap<i32, (f64, f64)>,
    closed: bool,
}

impl State {
    fn new(remote: Arc<Remote>, width: u32, height: u32) -> Self {
        State {
            remote,
            width,
            height,
            start: (width, height),
            asked: (0, 0),
            resize: None,
            generation: 0,
            compositor: None,
            shm: None,
            wm_base: None,
            seats: Vec::new(),
            surface: None,
            toplevel: None,
            configured: false,
            frame_pending: false,
            bufs: Vec::new(),
            mem: None,
            pointer: (0.0, 0.0),
            touches: HashMap::new(),
            closed: false,
        }
    }

    fn create_window(&mut self, qh: &QueueHandle<State>) -> Result<()> {
        let missing = |what: &str| Error::not_found(format!("host compositor has no {what}"));
        let compositor = self
            .compositor
            .clone()
            .ok_or_else(|| missing("wl_compositor"))?;
        let wm_base = self.wm_base.clone().ok_or_else(|| missing("xdg_wm_base"))?;
        self.alloc_buffers(qh)?;
        let surface = compositor.create_surface(qh, ());
        let xs = wm_base.get_xdg_surface(&surface, qh, ());
        let top = xs.get_toplevel(qh, ());
        top.set_title("droidc session".into());
        top.set_app_id("droidc-bridge".into());
        top.set_fullscreen(None);
        surface.commit();
        self.surface = Some(surface);
        self.toplevel = Some(top);
        Ok(())
    }

    /// Switches to `w`x`h` buffers. The old ones are destroyed; what the host compositor shows
    /// stays until the next frame is committed.
    fn resize_buffers(&mut self, qh: &QueueHandle<State>, w: u32, h: u32) -> Result<()> {
        for b in self.bufs.drain(..) {
            b.buffer.destroy();
        }
        if let Some((_, ptr, len)) = self.mem.take() {
            // SAFETY: our own mapping from `alloc_buffers`; no buffer points into it any more.
            let _ = unsafe { rustix::mm::munmap(ptr.cast(), len) };
        }
        self.width = w;
        self.height = h;
        self.frame_pending = false;
        self.alloc_buffers(qh)
    }

    /// Two XRGB8888 buffers of the window's size in one shared-memory pool.
    fn alloc_buffers(&mut self, qh: &QueueHandle<State>) -> Result<()> {
        let shm = self
            .shm
            .clone()
            .ok_or_else(|| Error::not_found("host compositor has no wl_shm"))?;
        self.generation += 1;
        let stride = self.width as usize * 4;
        let size = stride * self.height as usize;
        let fd = rustix::fs::memfd_create("droidc-bridge", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| Error::internal(format!("memfd_create: {e}")))?;
        rustix::fs::ftruncate(&fd, (size * 2) as u64)
            .map_err(|e| Error::internal(format!("ftruncate: {e}")))?;
        // SAFETY: a fresh shared mapping of our own memfd, unmapped only by `resize_buffers`.
        let ptr = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                size * 2,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
                &fd,
                0,
            )
        }
        .map_err(|e| Error::internal(format!("mmap: {e}")))? as *mut u8;
        let pool = shm.create_pool(fd.as_fd(), (size * 2) as i32, qh, ());
        for i in 0..2 {
            let buffer = pool.create_buffer(
                (i * size) as i32,
                self.width as i32,
                self.height as i32,
                stride as i32,
                wl_shm::Format::Xrgb8888,
                qh,
                (self.generation, i),
            );
            self.bufs.push(Buf {
                buffer,
                offset: i * size,
                busy: false,
            });
        }
        pool.destroy();
        self.mem = Some((fd, ptr, size * 2));
        Ok(())
    }

    /// Copies `frame` into a free buffer and commits it; false if it has to wait.
    fn present(&mut self, qh: &QueueHandle<State>, frame: &[u8]) -> bool {
        if !self.configured || self.frame_pending {
            return false;
        }
        if frame.len() != self.width as usize * self.height as usize * 4 {
            // From a capture at the size before a resize: drop it.
            return true;
        }
        let Some(i) = self.bufs.iter().position(|b| !b.busy) else {
            return false;
        };
        let (_, base, _) = self.mem.as_ref().unwrap();
        // SAFETY: the buffer region is inside our mapping and not in use by the compositor (!busy).
        unsafe {
            std::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                base.add(self.bufs[i].offset),
                frame.len(),
            );
        }
        let s = self.surface.as_ref().unwrap();
        s.attach(Some(&self.bufs[i].buffer), 0, 0);
        s.damage_buffer(0, 0, self.width as i32, self.height as i32);
        s.frame(qh, ());
        s.commit();
        self.bufs[i].busy = true;
        self.frame_pending = true;
        true
    }

    fn stream(&self) -> &str {
        self.remote.stream.as_str()
    }

    /// Window coordinates to stream coordinates.
    fn to_stream(&self, x: f64, y: f64) -> (f64, f64) {
        let (sx, sy) = input_scale(
            self.start,
            (self.width, self.height),
            self.remote.is_virtual(),
        );
        (x * sx, y * sy)
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => st.wm_base = Some(reg.bind(name, version.min(2), qh, ())),
                "wl_seat" => st.seats.push(reg.bind(name, version.min(5), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        b: &xdg_wm_base::XdgWmBase,
        ev: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            b.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        st: &mut Self,
        s: &xdg_surface::XdgSurface,
        ev: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = ev {
            s.ack_configure(serial);
            st.configured = true;
            if let Some(size) = configured_size(st.asked, (st.width, st.height)) {
                st.resize = Some(size);
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        st: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        ev: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            xdg_toplevel::Event::Close => st.closed = true,
            xdg_toplevel::Event::Configure { width, height, .. } => st.asked = (width, height),
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (u32, usize)> for State {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        ev: wl_buffer::Event,
        (generation, i): &(u32, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = ev {
            if *generation == st.generation {
                st.bufs[*i].busy = false;
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_callback::WlCallback,
        ev: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = ev {
            st.frame_pending = false;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        ev: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(c),
        } = ev
        {
            if c.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
            if c.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if c.contains(wl_seat::Capability::Touch) {
                seat.get_touch(qh, ());
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_pointer::WlPointer,
        ev: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let r = st.remote.clone();
        match ev {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                st.pointer = (surface_x, surface_y);
                let (x, y) = st.to_stream(surface_x, surface_y);
                r.notify("NotifyPointerMotionAbsolute", &(st.stream(), x, y));
            }
            wl_pointer::Event::Button { button, state, .. } => {
                let pressed = matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                r.notify("NotifyPointerButton", &(button as i32, pressed));
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let (dx, dy) = match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => (value, 0.0),
                    _ => (0.0, value),
                };
                r.notify("NotifyPointerAxis", &(dx, dy, 0u32));
            }
            wl_pointer::Event::AxisStop { .. } => {
                // 1 = finish: ends a kinetic scroll sequence.
                r.notify("NotifyPointerAxis", &(0.0f64, 0.0f64, 1u32));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // wl_keyboard key codes are evdev codes, which is what NotifyKeyboardKeycode takes.
        if let wl_keyboard::Event::Key { key, state, .. } = ev {
            let pressed = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
            st.remote.notify("NotifyKeyboardKeycode", &(key, pressed));
        }
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_touch::WlTouch,
        ev: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let r = st.remote.clone();
        match ev {
            wl_touch::Event::Down { id, x, y, .. } => {
                st.touches.insert(id, (x, y));
                let (x, y) = st.to_stream(x, y);
                r.notify("NotifyTouchDown", &(st.stream(), id as u32, x, y));
            }
            wl_touch::Event::Motion { id, x, y, .. } => {
                st.touches.insert(id, (x, y));
                let (x, y) = st.to_stream(x, y);
                r.notify("NotifyTouchMotion", &(st.stream(), id as u32, x, y));
            }
            wl_touch::Event::Up { id, .. } => {
                st.touches.remove(&id);
                r.notify("NotifyTouchUp", &(id as u32,));
            }
            wl_touch::Event::Cancel => {
                for id in std::mem::take(&mut st.touches).into_keys() {
                    r.notify("NotifyTouchUp", &(id as u32,));
                }
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_supported_scale() {
        let tablet = [
            1.0,
            1.25,
            1.502347469329834,
            1.748633861541748,
            2.0,
            2.253521203994751,
        ];
        assert_eq!(closest_scale(&tablet, 2.0), Some(2.0));
        assert_eq!(closest_scale(&tablet, 1.5), Some(1.502347469329834));
        assert_eq!(closest_scale(&tablet, 2.125), Some(2.0));
        assert_eq!(closest_scale(&[1.0, 2.0, 3.0], 2.625), Some(3.0));
        assert_eq!(closest_scale(&[1.0, 2.0], 1.4), Some(1.0));
        assert_eq!(closest_scale(&[], 2.0), None);
    }

    #[test]
    fn target_follows_the_compositor() {
        let none: Vec<String> = Vec::new();
        let one = vec!["Meta-0".to_string()];
        assert_eq!(pick_target(None, &none), Target::Virtual);
        assert_eq!(pick_target(None, &one), Target::Monitor("Meta-0".into()));
        assert_eq!(
            pick_target(Some("HDMI-1"), &none),
            Target::Monitor("HDMI-1".into())
        );
        assert_eq!(
            pick_target(Some("Meta-1"), &one),
            Target::Monitor("Meta-1".into())
        );
    }

    #[test]
    fn virtual_capture_asks_for_the_window_size() {
        let virt = capture_tail(1600, 2560, true);
        assert!(!virt.iter().any(|e| e == "videoscale"));
        assert!(virt.contains(&"video/x-raw,format=BGRx,width=1600,height=2560".to_string()));
        let mon = capture_tail(2560, 1600, false);
        assert!(mon.iter().any(|e| e == "videoscale"));
        assert_eq!(mon.last().map(String::as_str), Some("fd=1"));
    }

    #[test]
    fn resize_only_on_a_new_size() {
        assert_eq!(configured_size((0, 0), (2560, 1600)), None);
        assert_eq!(configured_size((2560, 1600), (2560, 1600)), None);
        assert_eq!(
            configured_size((1600, 2560), (2560, 1600)),
            Some((1600, 2560))
        );
        assert_eq!(configured_size((-1, 2560), (2560, 1600)), None);
    }

    #[test]
    fn input_follows_the_monitor() {
        assert_eq!(input_scale((2560, 1600), (1600, 2560), true), (1.0, 1.0));
        assert_eq!(input_scale((2560, 1600), (2560, 1600), false), (1.0, 1.0));
        assert_eq!(input_scale((2560, 1600), (1280, 800), false), (2.0, 2.0));
    }

    #[test]
    fn finds_a_link_into_the_capture() {
        let listing = "mutter:output_1\n  |-> python3.12:input_1\ngnome-shell:output_1\n  |-> droidc-bridge-7:input_1\ndroidc-bridge-7:input_1\n  |<- gnome-shell:output_1\npython3.12:input_1\n  |<- mutter:output_1\n";
        assert!(has_input_link(listing, "droidc-bridge-7"));
        assert!(!has_input_link(listing, "droidc-bridge-8"));
        assert!(!has_input_link(
            "droidc-bridge-7:input_1\n",
            "droidc-bridge-7"
        ));
        // droidc-bridge-70 is another bridge, not this one.
        assert!(!has_input_link(
            "droidc-bridge-70:input_1\n  |<- x:output_1\n",
            "droidc-bridge-7"
        ));
    }

    #[test]
    fn saved_layout() {
        let xml = r#"<monitors version="2">
  <configuration>
    <layoutmode>logical</layoutmode>
    <logicalmonitor>
      <x>0</x><y>0</y><scale>1.5023474693298340</scale><primary>yes</primary>
      <monitor>
        <monitorspec>
          <connector>Meta-0</connector><vendor>MetaVendor</vendor>
          <product>MetaVirtualMonitor</product><serial>0x00</serial>
        </monitorspec>
        <mode><width>2560</width><height>1600</height><rate>60.000</rate></mode>
      </monitor>
    </logicalmonitor>
  </configuration>
</monitors>"#;
        assert!(has_saved_layout(xml, "Meta-0", 2560, 1600));
        assert!(!has_saved_layout(xml, "Meta-0", 1600, 2560));
        assert!(!has_saved_layout(xml, "Meta-1", 2560, 1600));
        assert!(!has_saved_layout(
            "<monitors version=\"2\"/>",
            "Meta-0",
            2560,
            1600
        ));
    }
}
