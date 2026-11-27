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
//! `--scale <s>` sets the monitor's scale before recording (the closest one Mutter supports),
//! unless the user's own display settings (`monitors.xml`) already cover this monitor and size.
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
    /// Monitor to record; `Meta-0` is the first virtual monitor of a headless Mutter.
    pub connector: String,
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
        let stream: OwnedObjectPath = call(
            SCREEN_CAST,
            sc.as_str(),
            SC_SESSION,
            "RecordMonitor",
            &(opts.connector.as_str(), rec),
        )?
        .body()
        .deserialize()
        .map_err(dbus_err)?;
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
        })
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
        if out.status.success() {
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

fn dump_frame(node: u32, out: &std::path::Path) -> Result<i32> {
    let mut tail: Vec<String> = [
        "videoconvert",
        "!",
        "pngenc",
        "snapshot=true",
        "!",
        "filesink",
    ]
    .map(String::from)
    .to_vec();
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

/// Sets `opts.connector` to the supported scale closest to `opts.scale`, for this run of the
/// session only (Mutter's "temporary" config: nothing is saved and no confirmation is asked).
/// Leaves the scale alone when it is already not 1, when the user saved a layout for this monitor
/// and size, or when there is more than one monitor. Returns the scale it set.
fn set_scale(conn: &zbus::blocking::Connection, opts: &Opts) -> Result<Option<f64>> {
    let reply = conn
        .call_method(
            Some(DISPLAY_CONFIG),
            DISPLAY_CONFIG_PATH,
            Some(DISPLAY_CONFIG),
            "GetCurrentState",
            &(),
        )
        .map_err(dbus_err)?;
    let (serial, monitors, logical, _): CurrentState =
        reply.body().deserialize().map_err(dbus_err)?;
    let [lm] = logical.as_slice() else {
        return Ok(None);
    };
    let Some((spec, modes, _)) = monitors.iter().find(|(s, _, _)| s.0 == opts.connector) else {
        return Err(Error::not_found(format!("no monitor {}", opts.connector)));
    };
    if !lm.5.iter().any(|s| s.0 == opts.connector) || (lm.2 - 1.0).abs() > 0.01 {
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
    if monitors_xml().is_some_and(|x| has_saved_layout(&x, &opts.connector, mode.1, mode.2)) {
        return Ok(None);
    }
    let Some(scale) = closest_scale(&mode.5, opts.scale) else {
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

pub fn run(opts: &Opts) -> Result<i32> {
    if opts.width == 0 || opts.height == 0 {
        return Err(Error::invalid(
            "bridge needs a non-zero --width and --height",
        ));
    }
    if opts.scale > 0.0 {
        let conn = zbus::blocking::Connection::session().map_err(dbus_err)?;
        wait_for_screen_cast(&conn, opts.timeout)?;
        match set_scale(&conn, opts) {
            Ok(Some(s)) => eprintln!("bridge: set {} to scale {s}", opts.connector),
            Ok(None) => {}
            Err(e) => eprintln!("bridge: could not set the scale: {e}"),
        }
    }
    let remote = Arc::new(Remote::open(opts)?);
    eprintln!(
        "bridge: recording {} as PipeWire node {}",
        opts.connector, remote.node
    );
    let code = match &opts.dump_frame {
        Some(out) => dump_frame(remote.node, out),
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

/// One gst capture of the recorded monitor, scaled to the window size.
struct Capture {
    gst: std::process::Child,
    frames: Arc<Mutex<Frames>>,
}

impl Capture {
    fn start(node: u32, w: u32, h: u32, wake: Arc<OwnedFd>) -> Result<Capture> {
        let frame_len = (w * h * 4) as usize;
        let mut tail: Vec<String> = ["videoconvert", "!", "videoscale", "!"]
            .map(String::from)
            .to_vec();
        tail.push(format!("video/x-raw,format=BGRx,width={w},height={h}"));
        tail.extend(["!", "fdsink", "fd=1"].map(String::from));
        let mut gst = spawn_capture(node, &[], &tail, Stdio::piped())?;
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
    let mut cap = Capture::start(remote.node, w, h, wake_w.clone())?;
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
                eprintln!("bridge: Mutter dropped the stream of {}", opts.connector);
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
                    eprintln!("bridge: could not record {} again: {e}", opts.connector);
                    break code;
                }
            };
            eprintln!(
                "bridge: recording {} again as PipeWire node {}",
                opts.connector, remote.node
            );
            cap = match Capture::start(remote.node, w, h, wake_w.clone()) {
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

struct State {
    remote: Arc<Remote>,
    width: u32,
    height: u32,
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
        let shm = self.shm.clone().ok_or_else(|| missing("wl_shm"))?;
        let wm_base = self.wm_base.clone().ok_or_else(|| missing("xdg_wm_base"))?;
        let stride = self.width as usize * 4;
        let size = stride * self.height as usize;
        let fd = rustix::fs::memfd_create("droidc-bridge", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| Error::internal(format!("memfd_create: {e}")))?;
        rustix::fs::ftruncate(&fd, (size * 2) as u64)
            .map_err(|e| Error::internal(format!("ftruncate: {e}")))?;
        // SAFETY: a fresh shared mapping of our own memfd, unmapped never (lives as long as the process).
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
                i,
            );
            self.bufs.push(Buf {
                buffer,
                offset: i * size,
                busy: false,
            });
        }
        pool.destroy();
        self.mem = Some((fd, ptr, size * 2));
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

    /// Copies `frame` into a free buffer and commits it; false if it has to wait.
    fn present(&mut self, qh: &QueueHandle<State>, frame: &[u8]) -> bool {
        if !self.configured || self.frame_pending {
            return false;
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
        if let xdg_toplevel::Event::Close = ev {
            st.closed = true;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for State {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        ev: wl_buffer::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = ev {
            st.bufs[*i].busy = false;
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
                r.notify(
                    "NotifyPointerMotionAbsolute",
                    &(st.stream(), surface_x, surface_y),
                );
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
                r.notify("NotifyTouchDown", &(st.stream(), id as u32, x, y));
            }
            wl_touch::Event::Motion { id, x, y, .. } => {
                st.touches.insert(id, (x, y));
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
