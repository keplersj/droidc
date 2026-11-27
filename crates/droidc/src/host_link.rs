//! `droidc host-link`: one channel between the guest's desktop and Android, for the integration points
//! in `docs/android-integration.md` that are neither sound (`audio-out`) nor media (`media`).
//!
//! Runs inside the guest under `droidc supervise` (so it joins the container's session bus), started by
//! the host (Selkie) with the desktop. Both directions are JSON lines, one object per line, each with a
//! `kind`:
//!
//! - **stdin, host to guest:** Android state the guest should follow. `appearance` sets GNOME's color
//!   scheme, accent color, text size, animations and contrast from Android's.
//! - **stdout, guest to host:** `ready` once the link is up, then requests the host acts on:
//!   `open-uri` for a link the desktop opened (served on the session bus as `dev.droidc.HostLink1`,
//!   see [`crate::open`]), and `keep-awake` while a desktop app holds an idle inhibitor
//!   (see [`crate::awake`]). From the host, `screen-lock` leaves locking and blanking to Android.
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
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

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
    /// Android locks and blanks the device: turn GNOME's own lock screen and idle blank off
    /// (see [`crate::awake`]). `by_android: false` leaves GNOME's settings as they are.
    ScreenLock {
        by_android: bool,
    },
}

/// One line to the host.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GuestMsg {
    /// The link is up: the host can send state.
    Ready { version: &'static str },
    /// Open this link on Android (see [`crate::open`]).
    OpenUri { uri: String },
    /// Keep the screen on (a desktop app holds an idle inhibitor), or let it sleep again.
    KeepAwake { on: bool },
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

/// The link's state between lines.
pub struct Link {
    pub settings: Settings,
}

impl Link {
    pub fn new(settings: Settings) -> Self {
        Link { settings }
    }

    /// Acts on one line from the host. Unknown or malformed lines are reported, not fatal.
    pub fn handle_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match serde_json::from_str::<HostMsg>(line) {
            Ok(HostMsg::Appearance(a)) => {
                let n = self.settings.apply(&settings_for(&a));
                if n > 0 {
                    eprintln!("host-link: appearance: {n} setting(s) changed");
                }
            }
            Ok(HostMsg::ScreenLock { by_android: true }) => {
                let n = self.settings.apply(&crate::awake::android_locks_settings());
                if n > 0 {
                    eprintln!("host-link: screen lock: left to Android ({n} setting(s) changed)");
                }
            }
            Ok(HostMsg::ScreenLock { by_android: false }) => {}
            Err(e) => eprintln!("host-link: warn: skipped a line from the host: {e}"),
        }
    }
}

fn emit(out: &mut impl Write, msg: &GuestMsg) -> Result<()> {
    let line = serde_json::to_string(msg).map_err(|e| Error::internal(e.to_string()))?;
    writeln!(out, "{line}")
        .and_then(|_| out.flush())
        .map_err(|e| Error::internal(format!("stdout: {e}")))
}

/// Writes one line to the host, from any thread.
type SendLine = Arc<dyn Fn(&GuestMsg) -> Result<()> + Send + Sync>;

/// Runs the link until stdin closes. Returns the exit code.
pub fn run() -> Result<i32> {
    // A closed stdout must surface as a write error, not kill us.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let out: crate::open::Out = Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let mut link = Link::new(Settings::from_env());

    // Links: the bus method writes `open-uri` lines through the same stdout.
    match crate::open::install_handler() {
        Ok(took) if !took.is_empty() => {
            eprintln!("host-link: links: now the default for {}", took.join(", "))
        }
        Ok(_) => {}
        Err(e) => eprintln!("host-link: warn: links: no handler: {e}"),
    }
    let send: SendLine = {
        let out = out.clone();
        Arc::new(move |m: &GuestMsg| {
            let mut o = out
                .lock()
                .map_err(|_| Error::internal("stdout lock poisoned"))?;
            emit(&mut *o, m)
        })
    };
    let sink: crate::open::Sink = {
        let send = send.clone();
        Arc::new(move |uri: &str| {
            send(&GuestMsg::OpenUri {
                uri: uri.to_string(),
            })
        })
    };
    let bus = match crate::open::serve(sink) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("host-link: warn: links: {e}");
            None
        }
    };

    send(&GuestMsg::Ready {
        version: crate::VERSION,
    })?;
    // Keep-awake: follows gnome-session's idle inhibitors, on the same connection. After `ready`, so
    // that line always comes first.
    if let Some(conn) = bus.clone() {
        let send = send.clone();
        let watched = crate::awake::watch(conn, move |on| {
            if let Err(e) = send(&GuestMsg::KeepAwake { on }) {
                eprintln!("host-link: warn: keep-awake: {e}");
            }
        });
        if let Err(e) = watched {
            eprintln!("host-link: warn: keep-awake: {e}");
        }
    }
    eprintln!("host-link: ready");
    for line in std::io::stdin().lock().lines() {
        match line {
            Ok(l) => link.handle_line(&l),
            Err(e) => return Err(Error::internal(format!("stdin: {e}"))),
        }
    }
    eprintln!("host-link: the host closed the link");
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
        let HostMsg::Appearance(a) = a else {
            panic!("{a:?}")
        };
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
