//! `droidc host-link`: one channel between the guest's desktop and Android, for the integration points
//! in `docs/android-integration.md` that are neither sound (`audio-out`) nor media (`media`).
//!
//! Runs inside the guest under `droidc supervise` (so it joins the container's session bus), started by
//! the host (Selkie) with the desktop. Both directions are JSON lines, one object per line, each with a
//! `kind`:
//!
//! - **stdin, host to guest:** Android state the guest should follow. `appearance` sets GNOME's color
//!   scheme, accent color, text size, animations and contrast from Android's.
//! - **stdout, guest to host:** `ready` once the link is up, then requests the host acts on (later
//!   kinds: links to open, notifications, files to pick).
//!
//! Commands in the guest reach the host through the link too: it listens on a Unix socket in the
//! container's `/run` ([`SOCKET`], or `DROIDC_HOST_LINK_SOCKET`), and passes a client's one request line on
//! with an `id`. The host answers with `progress` lines (shown to the user while it waits on them) and one
//! `reply`, each carrying that `id`, which go back to that client. Only the kinds in [`CLIENT_KINDS`] are
//! passed on. `droidc adb` is the first client ([`crate::adb`]).
//!
//! A line that doesn't parse, or a kind this droidc doesn't know, is reported on stderr and skipped, so
//! a newer host can talk to an older droidc. Status and errors go to stderr. Closing stdin ends the link.
//!
//! Settings are written with the image's `gsettings` (a static droidc can't load GIO), or the program
//! named by `DROIDC_GSETTINGS` (tests). Only values that changed since the last line are written, and a
//! key the image's GNOME doesn't have (`accent-color` before GNOME 47) is tried once, then left alone.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Where the link listens for commands in the guest: `/run` is the container's own, so only this app's
/// processes can reach it.
pub const SOCKET: &str = "/run/droidc-host-link.sock";
/// Overrides [`SOCKET`] (tests).
pub const SOCKET_ENV: &str = "DROIDC_HOST_LINK_SOCKET";
/// Requests a guest command may send the host. Anything else is refused at the socket.
pub const CLIENT_KINDS: [&str; 2] = ["adb-pair", "adb-port"];
/// The longest request line a client may send.
const MAX_REQUEST: u64 = 4096;

/// The socket path this guest uses.
pub fn socket_path() -> String {
    std::env::var(SOCKET_ENV).unwrap_or_else(|_| SOCKET.into())
}

const INTERFACE: &str = "org.gnome.desktop.interface";
const A11Y: &str = "org.gnome.desktop.a11y.interface";

/// Android's look, as the host reads it. Every field is optional: the host sends what it knows.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Appearance {
    /// Night mode is on.
    pub dark: Option<bool>,
    /// The system accent (Material You's `system_accent1_500`), as `#rrggbb`.
    pub accent: Option<String>,
    /// Android's font scale (`Configuration.fontScale`): 1.0 is the default size.
    pub text_scale: Option<f64>,
    /// False when the animator duration scale is 0 ("Remove animations").
    pub animations: Option<bool>,
    /// The contrast level is high (`UiModeManager.getContrast()` at or above 0.5).
    pub high_contrast: Option<bool>,
}

/// One line from the host.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HostMsg {
    Appearance(Appearance),
}

/// One line to the host.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GuestMsg {
    /// The link is up: the host can send state.
    Ready { version: &'static str },
}

/// GNOME's accent colors (libadwaita 1.6 / GNOME 47), in `accent-color`'s spelling, with their
/// sRGB values.
const ACCENTS: [(&str, [u8; 3]); 9] = [
    ("blue", [0x35, 0x84, 0xe4]),
    ("teal", [0x21, 0x90, 0xa4]),
    ("green", [0x3a, 0x94, 0x4a]),
    ("yellow", [0xc8, 0x88, 0x00]),
    ("orange", [0xed, 0x5b, 0x00]),
    ("red", [0xe6, 0x2d, 0x42]),
    ("pink", [0xd5, 0x61, 0x99]),
    ("purple", [0x91, 0x41, 0xac]),
    ("slate", [0x6f, 0x83, 0x96]),
];

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#').unwrap_or(s);
    // #aarrggbb as Android's Color.toHexString-style helpers give it: drop the alpha.
    let h = if h.len() == 8 { &h[2..] } else { h };
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([b(0)?, b(2)?, b(4)?])
}

/// Hue in degrees and saturation (0..1, HSL) of an sRGB color.
fn hue_sat(c: [u8; 3]) -> (f64, f64) {
    let [r, g, b] = c.map(|v| f64::from(v) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let l = (max + min) / 2.0;
    if d < 1e-9 {
        return (0.0, 0.0);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-9);
    let h = if max == r {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, s)
}

/// The GNOME accent nearest to an Android accent: by hue, or `slate` for a greyish one (Material You
/// picks a muted accent from a grey wallpaper).
pub fn nearest_accent(hex: &str) -> Option<&'static str> {
    let c = parse_hex(hex)?;
    let (h, s) = hue_sat(c);
    if s < 0.2 {
        return Some("slate");
    }
    ACCENTS
        .iter()
        .filter(|(n, _)| *n != "slate")
        .map(|(n, v)| {
            let (vh, _) = hue_sat(*v);
            let d = (h - vh).abs();
            (n, d.min(360.0 - d))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(n, _)| *n)
}

/// The settings one [`Appearance`] stands for, as `(schema, key, value)` in GVariant text form.
/// Fields the host didn't send set nothing.
pub fn settings_for(a: &Appearance) -> Vec<(&'static str, &'static str, String)> {
    let mut out = Vec::new();
    if let Some(dark) = a.dark {
        let v = if dark { "'prefer-dark'" } else { "'default'" };
        out.push((INTERFACE, "color-scheme", v.to_string()));
    }
    if let Some(name) = a.accent.as_deref().and_then(nearest_accent) {
        out.push((INTERFACE, "accent-color", format!("'{name}'")));
    }
    if let Some(s) = a.text_scale.filter(|s| s.is_finite()) {
        // GNOME's own range for the key; Android's largest font scale (2.0) fits.
        let s = s.clamp(0.5, 3.0);
        out.push((INTERFACE, "text-scaling-factor", format!("{s:.2}")));
    }
    if let Some(on) = a.animations {
        out.push((INTERFACE, "enable-animations", on.to_string()));
    }
    if let Some(on) = a.high_contrast {
        out.push((A11Y, "high-contrast", on.to_string()));
    }
    out
}

/// Writes GSettings keys through a `gsettings`-compatible program, skipping what is already set.
pub struct Settings {
    program: String,
    applied: HashMap<(&'static str, &'static str), String>,
    missing: HashSet<(&'static str, &'static str)>,
}

impl Settings {
    pub fn new(program: impl Into<String>) -> Self {
        Settings {
            program: program.into(),
            applied: HashMap::new(),
            missing: HashSet::new(),
        }
    }

    /// The program from `DROIDC_GSETTINGS`, else `gsettings` on PATH.
    pub fn from_env() -> Self {
        Self::new(std::env::var("DROIDC_GSETTINGS").unwrap_or_else(|_| "gsettings".into()))
    }

    /// Sets each key whose value changed. Returns how many were written; failures are reported on
    /// stderr and don't stop the others.
    pub fn apply(&mut self, wanted: &[(&'static str, &'static str, String)]) -> usize {
        let mut n = 0;
        for (schema, key, value) in wanted {
            let k = (*schema, *key);
            if self.missing.contains(&k) || self.applied.get(&k) == Some(value) {
                continue;
            }
            match self.set(schema, key, value) {
                Ok(()) => {
                    self.applied.insert(k, value.clone());
                    n += 1;
                }
                Err(e) => {
                    eprintln!("host-link: warn: {schema} {key}: {e}");
                    // A schema or key this GNOME lacks won't appear later: stop trying it.
                    if e.contains("No such key") || e.contains("No such schema") {
                        self.missing.insert(k);
                    }
                }
            }
        }
        n
    }

    fn set(&self, schema: &str, key: &str, value: &str) -> std::result::Result<(), String> {
        let out = Command::new(&self.program)
            .args(["set", schema, key, value])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", self.program))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// Guest commands waiting on the host, by request id, and the host's stdout they write to.
pub struct Clients {
    out: Mutex<Box<dyn Write + Send>>,
    pending: Mutex<HashMap<u64, UnixStream>>,
    next: AtomicU64,
}

impl Clients {
    pub fn new(out: Box<dyn Write + Send>) -> Arc<Self> {
        Arc::new(Clients {
            out: Mutex::new(out),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    /// Writes one line to the host.
    pub fn emit_value(&self, v: &serde_json::Value) -> Result<()> {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(out, "{v}")
            .and_then(|_| out.flush())
            .map_err(|e| Error::internal(format!("stdout: {e}")))
    }

    /// Reads one request from a guest command and passes it to the host, or tells the command why not.
    pub fn accept(self: &Arc<Self>, stream: UnixStream) {
        let mut line = String::new();
        let read = BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line);
        let refuse = |mut s: &UnixStream, why: String| {
            let v = serde_json::json!({"kind": "reply", "ok": false, "error": why});
            let _ = writeln!(s, "{v}");
        };
        if let Err(e) = read {
            return refuse(&stream, format!("reading the request: {e}"));
        }
        let mut obj = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(serde_json::Value::Object(o)) => o,
            _ => return refuse(&stream, "the request isn't a JSON object".into()),
        };
        let kind = obj
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("")
            .to_string();
        if !CLIENT_KINDS.contains(&kind.as_str()) {
            return refuse(&stream, format!("host-link doesn't pass on '{kind}'"));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        obj.insert("id".into(), id.into());
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, stream);
        if let Err(e) = self.emit_value(&serde_json::Value::Object(obj)) {
            if let Some(s) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
            {
                refuse(&s, e.to_string());
            }
        }
        eprintln!("host-link: {kind} request {id} passed to the host");
    }

    /// Hands a `progress` or `reply` line from the host to the command waiting on its `id`. A `reply`
    /// ends that request. Returns false if no command waits on it (it gave up, say).
    pub fn route(&self, v: &serde_json::Value) -> bool {
        let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
            return false;
        };
        let last = v.get("kind").and_then(|k| k.as_str()) == Some("reply");
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let ok = match pending.get(&id) {
            Some(mut s) => writeln!(s, "{v}").is_ok(),
            None => return false,
        };
        if last || !ok {
            pending.remove(&id);
        }
        ok
    }

    /// How many guest commands are waiting.
    pub fn waiting(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Listens for guest commands at `path` until the process ends, one thread per command. A stale socket
/// file from an earlier link is replaced.
pub fn listen(path: &str, clients: Arc<Clients>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| Error::internal(format!("{path}: {e}")))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let c = Arc::clone(&clients);
            std::thread::spawn(move || c.accept(stream));
        }
    });
    Ok(())
}

/// The link's state between lines.
pub struct Link {
    pub settings: Settings,
    /// Guest commands waiting on the host; none when the link has no socket (tests).
    pub clients: Option<Arc<Clients>>,
}

impl Link {
    pub fn new(settings: Settings) -> Self {
        Link {
            settings,
            clients: None,
        }
    }

    /// Acts on one line from the host. Unknown or malformed lines are reported, not fatal.
    pub fn handle_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if matches!(
                v.get("kind").and_then(|k| k.as_str()),
                Some("reply" | "progress")
            ) {
                let routed = self.clients.as_ref().is_some_and(|c| c.route(&v));
                if !routed {
                    eprintln!("host-link: warn: an answer for a request nobody waits on");
                }
                return;
            }
        }
        match serde_json::from_str::<HostMsg>(line) {
            Ok(HostMsg::Appearance(a)) => {
                let n = self.settings.apply(&settings_for(&a));
                if n > 0 {
                    eprintln!("host-link: appearance: {n} setting(s) changed");
                }
            }
            Err(e) => eprintln!("host-link: warn: skipped a line from the host: {e}"),
        }
    }
}

/// Runs the link until stdin closes. Returns the exit code.
pub fn run() -> Result<i32> {
    // A closed stdout must surface as a write error, not kill us.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let mut link = Link::new(Settings::from_env());
    let clients = Clients::new(Box::new(std::io::stdout()));
    let path = socket_path();
    match listen(&path, Arc::clone(&clients)) {
        Ok(()) => link.clients = Some(Arc::clone(&clients)),
        // Appearance still works; guest commands say the link isn't there.
        Err(e) => eprintln!("host-link: warn: no socket for guest commands: {e}"),
    }
    let ready = serde_json::to_value(GuestMsg::Ready {
        version: crate::VERSION,
    })
    .map_err(|e| Error::internal(e.to_string()))?;
    clients.emit_value(&ready)?;
    eprintln!("host-link: ready");
    for line in std::io::stdin().lock().lines() {
        match line {
            Ok(l) => link.handle_line(&l),
            Err(e) => return Err(Error::internal(format!("stdin: {e}"))),
        }
    }
    eprintln!("host-link: the host closed the link");
    if link.clients.is_some() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_map_to_the_nearest_gnome_accent() {
        // GNOME's own colors map to themselves.
        for (name, c) in ACCENTS {
            let hex = format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
            assert_eq!(nearest_accent(&hex), Some(name), "{hex}");
        }
        // Material You samples.
        assert_eq!(nearest_accent("#6750A4"), Some("purple"));
        assert_eq!(nearest_accent("#006A6A"), Some("teal"));
        assert_eq!(nearest_accent("#B3261E"), Some("red"));
        assert_eq!(nearest_accent("#5B6B7A"), Some("slate"));
        assert_eq!(nearest_accent("#ff6750a4"), Some("purple"));
        assert_eq!(nearest_accent("not a color"), None);
    }

    #[test]
    fn appearance_becomes_gsettings() {
        let a: HostMsg = serde_json::from_str(
            r##"{"kind":"appearance","dark":true,"accent":"#3584e4","text_scale":1.15,"animations":false,"high_contrast":false}"##,
        )
        .unwrap();
        let HostMsg::Appearance(a) = a;
        let s = settings_for(&a);
        assert_eq!(
            s,
            vec![
                (INTERFACE, "color-scheme", "'prefer-dark'".into()),
                (INTERFACE, "accent-color", "'blue'".into()),
                (INTERFACE, "text-scaling-factor", "1.15".into()),
                (INTERFACE, "enable-animations", "false".into()),
                (A11Y, "high-contrast", "false".into()),
            ]
        );
        // Only what was sent.
        let s = settings_for(&Appearance {
            dark: Some(false),
            text_scale: Some(9.0),
            ..Default::default()
        });
        assert_eq!(
            s,
            vec![
                (INTERFACE, "color-scheme", "'default'".into()),
                (INTERFACE, "text-scaling-factor", "3.00".into()),
            ]
        );
    }

    #[test]
    fn unknown_kinds_do_not_parse_as_appearance() {
        assert!(serde_json::from_str::<HostMsg>(r#"{"kind":"from-the-future"}"#).is_err());
        assert!(serde_json::from_str::<HostMsg>("not json").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn settings_write_only_changes_and_give_up_on_missing_keys() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let fake = dir.path().join("gsettings");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$*\" >> {}\ncase \"$3\" in accent-color) echo 'No such key “accent-color”' >&2; exit 1;; esac\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut s = Settings::new(fake.to_str().unwrap());
        let want = settings_for(&Appearance {
            dark: Some(true),
            accent: Some("#3584e4".into()),
            ..Default::default()
        });
        assert_eq!(s.apply(&want), 1);
        assert_eq!(s.apply(&want), 0);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            vec![
                "set org.gnome.desktop.interface color-scheme 'prefer-dark'",
                "set org.gnome.desktop.interface accent-color 'blue'",
            ]
        );
    }
}
