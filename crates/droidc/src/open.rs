//! Links from the guest's desktop, opened on Android.
//!
//! `droidc host-link` serves `dev.droidc.HostLink1.OpenUri` on the session bus and hands each URI to
//! the host as an `open-uri` line, which the host opens with an `ACTION_VIEW` intent (the Android
//! browser for web links, the mail, phone or maps app for the others). `droidc open <uri>` is the
//! client, and the desktop entry [`install_handler`] writes makes it the default handler for those
//! schemes, so `xdg-open`, `gio open`, GNOME apps and the OpenURI portal all end up there. A scheme the
//! user already has a default for is left alone, and GNOME's Default Apps settings can change it back.
//!
//! Only the schemes in [`SCHEMES`] go through: `file:` paths are in the guest and mean nothing to
//! Android, and anything else could start an arbitrary app.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

pub const BUS_NAME: &str = "dev.droidc.HostLink";
pub const OBJECT_PATH: &str = "/dev/droidc/HostLink";
pub const INTERFACE: &str = "dev.droidc.HostLink1";
/// The desktop entry's id, under `$XDG_DATA_HOME/applications`.
pub const DESKTOP_ID: &str = "dev.droidc.AndroidOpen.desktop";

/// URI schemes handed to Android.
pub const SCHEMES: [&str; 6] = ["http", "https", "mailto", "tel", "sms", "geo"];

/// The URI's scheme if Android should open it.
pub fn allowed(uri: &str) -> Option<&'static str> {
    let (scheme, rest) = uri.split_once(':')?;
    if rest.is_empty() || uri.len() > 8192 || uri.chars().any(|c| c.is_control()) {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    SCHEMES.iter().copied().find(|s| *s == scheme)
}

/// Receives URIs from the bus; the host link writes them to the host.
pub type Sink = std::sync::Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

struct Service {
    sink: Sink,
}

#[zbus::interface(name = "dev.droidc.HostLink1")]
impl Service {
    /// Opens `uri` on Android. Fails for a scheme Android isn't given, or with the host gone.
    fn open_uri(&self, uri: &str) -> zbus::fdo::Result<()> {
        if allowed(uri).is_none() {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "not opened on Android: only {} links are",
                SCHEMES.join(", ")
            )));
        }
        (self.sink)(uri).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

/// Serves [`INTERFACE`] on the session bus. The connection serves while it is kept.
pub fn serve(sink: Sink) -> Result<zbus::blocking::Connection> {
    let ctx = |e: zbus::Error| Error::internal(format!("session bus: {e}"));
    zbus::blocking::connection::Builder::session()
        .map_err(ctx)?
        .serve_at(OBJECT_PATH, Service { sink })
        .map_err(ctx)?
        .name(BUS_NAME)
        .map_err(ctx)?
        .replace_existing_names(true)
        .build()
        .map_err(ctx)
}

/// `droidc open <uri>`: asks the host link to open `uri` on Android.
pub fn open(uri: &str) -> Result<()> {
    if allowed(uri).is_none() {
        return Err(Error::invalid(format!(
            "{uri}: only {} links open on Android",
            SCHEMES.join(", ")
        )));
    }
    let conn = zbus::blocking::Connection::session()
        .map_err(|e| Error::internal(format!("session bus: {e}")))?;
    conn.call_method(
        Some(BUS_NAME),
        OBJECT_PATH,
        Some(INTERFACE),
        "OpenUri",
        &(uri,),
    )
    .map(|_| ())
    .map_err(|e| match e {
        zbus::Error::MethodError(name, msg, _) if name.as_str().ends_with("ServiceUnknown") => {
            Error::not_found(format!(
                "the host link isn't running (Selkie starts it with the desktop): {}",
                msg.unwrap_or_default()
            ))
        }
        e => Error::internal(format!("OpenUri: {e}")),
    })
}

/// `$XDG_DATA_HOME` and `$XDG_CONFIG_HOME`, with their defaults under `$HOME`.
fn xdg_dirs() -> Option<(PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let dir = |var: &str, dflt: &str| {
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(dflt)))
    };
    Some((
        dir("XDG_DATA_HOME", ".local/share")?,
        dir("XDG_CONFIG_HOME", ".config")?,
    ))
}

/// The desktop entry for `droidc open`.
pub fn desktop_entry(droidc: &str) -> String {
    let mimes: String = SCHEMES
        .iter()
        .map(|s| format!("x-scheme-handler/{s};"))
        .collect();
    format!(
        "[Desktop Entry]\nType=Application\nName=Android\nComment=Open on Android\n\
         Exec={droidc} open %u\nNoDisplay=true\nTerminal=false\nMimeType={mimes}\n\
         X-Droidc-Managed=true\n"
    )
}

/// `mimeapps` with [`DESKTOP_ID`] as the default for each scheme that has no default yet. Returns the
/// new text and the schemes it took, or `None` when nothing changes.
pub fn add_defaults(mimeapps: &str) -> Option<(String, Vec<&'static str>)> {
    let mut lines: Vec<String> = mimeapps.lines().map(str::to_string).collect();
    // Keys already set under [Default Applications].
    let mut section_at = None;
    let mut end = lines.len();
    let mut set = std::collections::HashSet::new();
    let mut cur = String::new();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with('[') {
            if section_at.is_some() && end == lines.len() && cur == "[Default Applications]" {
                end = i;
            }
            cur = t.to_string();
            if t == "[Default Applications]" && section_at.is_none() {
                section_at = Some(i);
            }
            continue;
        }
        if cur == "[Default Applications]" {
            if let Some((k, v)) = t.split_once('=') {
                if !v.trim().is_empty() {
                    set.insert(k.trim().to_string());
                }
            }
        }
    }
    let missing: Vec<&'static str> = SCHEMES
        .iter()
        .copied()
        .filter(|s| !set.contains(&format!("x-scheme-handler/{s}")))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let new: Vec<String> = missing
        .iter()
        .map(|s| format!("x-scheme-handler/{s}={DESKTOP_ID};"))
        .collect();
    match section_at {
        Some(_) => {
            // After the section's last non-blank line.
            let mut at = end;
            while at > 0 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.splice(at..at, new);
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[Default Applications]".into());
            lines.extend(new);
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    Some((text, missing))
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("droidc-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Writes the desktop entry and adds it as the default for schemes without one. Returns the schemes
/// it became the default for.
pub fn install_handler() -> Result<Vec<&'static str>> {
    let (data, config) = xdg_dirs()
        .ok_or_else(|| Error::invalid("HOME is not set; no place for the link handler"))?;
    // The image's droidc; otherwise this one (tests), when its path is safe to put in Exec.
    let droidc = if Path::new("/usr/bin/droidc").exists() {
        "/usr/bin/droidc".to_string()
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .filter(|p| !p.contains(|c: char| c.is_whitespace() || "\"'\\$`%".contains(c)))
            .unwrap_or_else(|| "/usr/bin/droidc".into())
    };
    let entry = data.join("applications").join(DESKTOP_ID);
    let want = desktop_entry(&droidc);
    if std::fs::read_to_string(&entry).ok().as_deref() != Some(want.as_str()) {
        write_atomic(&entry, &want)
            .map_err(|e| Error::storage(format!("{}: {e}", entry.display())))?;
    }
    let mimeapps = config.join("mimeapps.list");
    let cur = std::fs::read_to_string(&mimeapps).unwrap_or_default();
    match add_defaults(&cur) {
        Some((text, took)) => {
            write_atomic(&mimeapps, &text)
                .map_err(|e| Error::storage(format!("{}: {e}", mimeapps.display())))?;
            Ok(took)
        }
        None => Ok(Vec::new()),
    }
}

/// Guards the stdout of the host link, shared with the bus thread.
pub type Out = std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_schemes_are_allowed() {
        assert_eq!(allowed("https://example.org/a?b"), Some("https"));
        assert_eq!(allowed("HTTP://example.org"), Some("http"));
        assert_eq!(allowed("mailto:a@example.org"), Some("mailto"));
        assert_eq!(allowed("tel:+15551234"), Some("tel"));
        assert_eq!(allowed("geo:40.7,-111.9"), Some("geo"));
        assert_eq!(allowed("file:///etc/passwd"), None);
        assert_eq!(allowed("intent://x#Intent;end"), None);
        assert_eq!(allowed("https:"), None);
        assert_eq!(allowed("https://a\nb"), None);
        assert_eq!(allowed("no scheme"), None);
    }

    #[test]
    fn defaults_are_added_only_where_missing() {
        // Empty file: a new section with all of them.
        let (t, took) = add_defaults("").unwrap();
        assert_eq!(took, SCHEMES.to_vec());
        assert!(t.starts_with(
            "[Default Applications]\nx-scheme-handler/http=dev.droidc.AndroidOpen.desktop;\n"
        ));

        // The user's own choices stay; other sections stay.
        let cur = "[Added Associations]\ntext/plain=org.gnome.TextEditor.desktop;\n\n\
                   [Default Applications]\nx-scheme-handler/https=org.mozilla.firefox.desktop\n\
                   text/html=org.mozilla.firefox.desktop\n\n[Removed Associations]\n";
        let (t, took) = add_defaults(cur).unwrap();
        assert!(!took.contains(&"https"));
        assert!(took.contains(&"http"));
        assert!(t.contains("x-scheme-handler/https=org.mozilla.firefox.desktop\n"));
        assert!(t.contains("text/plain=org.gnome.TextEditor.desktop;"));
        // Inserted inside [Default Applications], before [Removed Associations].
        let da = t.find("[Default Applications]").unwrap();
        let ours = t.find("x-scheme-handler/http=dev.droidc").unwrap();
        let ra = t.find("[Removed Associations]").unwrap();
        assert!(da < ours && ours < ra, "{t}");

        // Run again: nothing to do.
        assert!(add_defaults(&t).is_none());
    }

    #[test]
    fn the_entry_runs_droidc_open() {
        let e = desktop_entry("/usr/bin/droidc");
        assert!(e.contains("Exec=/usr/bin/droidc open %u\n"));
        assert!(e.contains("MimeType=x-scheme-handler/http;x-scheme-handler/https;"));
        assert!(e.contains("NoDisplay=true"));
    }
}
