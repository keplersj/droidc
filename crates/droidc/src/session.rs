//! How a deployment's graphical session is started and rendered, see docs/design-mvp.md.
//!
//! The image declares it with labels (`containers.droidc.session.renderer`, `.exec`, `.services`);
//! `<root>/session.json` (written by `droidc session set`) overrides them for images that don't.
//! droidc keeps no table of desktops: whatever the image (or the user) names is what runs.

use crate::error::{Context, Error, Result};
use crate::layout::Root;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const NAME_LABEL: &str = "containers.droidc.session";
pub const RENDERER_LABEL: &str = "containers.droidc.session.renderer";
pub const EXEC_LABEL: &str = "containers.droidc.session.exec";
pub const SERVICES_LABEL: &str = "containers.droidc.session.services";

/// How the session's pixels reach the host compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Renderer {
    /// The session compositor runs nested, as a Wayland client of the host compositor.
    Wayland,
    /// The session compositor runs headless; `droidc-bridge` presents its ScreenCast stream
    /// (Mutter's ScreenCast/RemoteDesktop D-Bus API) as a Wayland client and forwards input.
    Screencast,
}

impl Renderer {
    pub fn parse(s: &str) -> Option<Renderer> {
        match s {
            "wayland" => Some(Renderer::Wayland),
            "screencast" => Some(Renderer::Screencast),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Display name, also exported as `XDG_CURRENT_DESKTOP` when set.
    #[serde(default)]
    pub name: Option<String>,
    pub renderer: Renderer,
    /// Session command; may contain `{width}`, `{height}`, `{scale}`, `{wayland_display}`.
    pub exec: Vec<String>,
    /// Commands started before `exec` and stopped after it (same placeholders).
    #[serde(default)]
    pub services: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Label,
    Override,
}

impl SessionSpec {
    /// Builds a spec from image labels. `Ok(None)` when the image declares no session renderer.
    pub fn from_labels(labels: &BTreeMap<String, String>) -> Result<Option<SessionSpec>> {
        let Some(r) = labels.get(RENDERER_LABEL) else {
            return Ok(None);
        };
        let renderer = Renderer::parse(r.trim())
            .ok_or_else(|| Error::invalid(format!("{RENDERER_LABEL}: unknown renderer '{r}'")))?;
        let exec = labels.get(EXEC_LABEL).ok_or_else(|| {
            Error::invalid(format!("{RENDERER_LABEL} is set but {EXEC_LABEL} is not"))
        })?;
        let exec = parse_argv(exec).map_err(|e| Error::invalid(format!("{EXEC_LABEL}: {e}")))?;
        let services = match labels.get(SERVICES_LABEL) {
            Some(s) => {
                parse_services(s).map_err(|e| Error::invalid(format!("{SERVICES_LABEL}: {e}")))?
            }
            None => Vec::new(),
        };
        Ok(Some(SessionSpec {
            name: labels.get(NAME_LABEL).cloned(),
            renderer,
            exec,
            services,
        }))
    }

    pub fn validate(&self) -> Result<()> {
        if self.exec.is_empty() || self.exec[0].is_empty() {
            return Err(Error::invalid("session exec must not be empty"));
        }
        if self
            .services
            .iter()
            .any(|s| s.is_empty() || s[0].is_empty())
        {
            return Err(Error::invalid(
                "session services must not contain empty commands",
            ));
        }
        Ok(())
    }
}

/// A JSON argv: `["cmd","arg",...]`.
pub fn parse_argv(s: &str) -> std::result::Result<Vec<String>, String> {
    let v: Vec<String> =
        serde_json::from_str(s).map_err(|e| format!("expected a JSON array of strings: {e}"))?;
    if v.is_empty() || v[0].is_empty() {
        return Err("empty command".into());
    }
    Ok(v)
}

/// A JSON list of argv: `[["cmd","arg"],...]`.
pub fn parse_services(s: &str) -> std::result::Result<Vec<Vec<String>>, String> {
    let v: Vec<Vec<String>> = serde_json::from_str(s)
        .map_err(|e| format!("expected a JSON array of string arrays: {e}"))?;
    if v.iter().any(|c| c.is_empty() || c[0].is_empty()) {
        return Err("empty command".into());
    }
    Ok(v)
}

/// Values for the placeholders in `exec` and `services`.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub wayland_display: String,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            width: 1280,
            height: 800,
            scale: 1.0,
            wayland_display: "wayland-0".into(),
        }
    }
}

/// Replaces `{width}`, `{height}`, `{scale}`, `{wayland_display}` in each argument.
pub fn fill(argv: &[String], p: &Params) -> Vec<String> {
    argv.iter()
        .map(|a| {
            a.replace("{width}", &p.width.to_string())
                .replace("{height}", &p.height.to_string())
                .replace("{scale}", &p.scale.to_string())
                .replace("{wayland_display}", &p.wayland_display)
        })
        .collect()
}

pub fn override_path(root: &Root) -> PathBuf {
    root.path.join("session.json")
}

pub fn read_override(root: &Root) -> Result<Option<SessionSpec>> {
    let p = override_path(root);
    match fs::read(&p) {
        Ok(b) => {
            let s: SessionSpec = serde_json::from_slice(&b)
                .map_err(|e| Error::invalid(format!("{}: {e}", p.display())))?;
            s.validate()?;
            Ok(Some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context(p.display()),
    }
}

pub fn write_override(root: &Root, s: &SessionSpec) -> Result<()> {
    s.validate()?;
    let p = override_path(root);
    fs::create_dir_all(&root.path).context(root.path.display())?;
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(s)?).context(tmp.display())?;
    fs::rename(&tmp, &p).context(p.display())
}

pub fn clear_override(root: &Root) -> Result<bool> {
    let p = override_path(root);
    match fs::remove_file(&p) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context(p.display()),
    }
}

/// The session for the current deployment: the override, else the current image's labels.
pub fn resolve(root_path: &Path) -> Result<Option<(SessionSpec, Source)>> {
    let root = Root::new(root_path);
    if let Some(s) = read_override(&root)? {
        return Ok(Some((s, Source::Override)));
    }
    let Some(id) = root.current_id() else {
        return Ok(None);
    };
    let rec = root.read_record(&id)?;
    Ok(SessionSpec::from_labels(&rec.labels)?.map(|s| (s, Source::Label)))
}

/// `shell-spec`'s `session` object (also `droidc session show`).
pub fn describe(
    resolved: &Option<(SessionSpec, Source)>,
    label_name: Option<&str>,
) -> serde_json::Value {
    match resolved {
        Some((s, src)) => serde_json::json!({
            "name": s.name.as_deref().or(label_name),
            "renderer": s.renderer,
            "exec": s.exec,
            "services": s.services,
            "configured": true,
            "source": src,
        }),
        None => serde_json::json!({
            "name": label_name,
            "renderer": null,
            "exec": null,
            "services": [],
            "configured": false,
            "source": null,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn labels_parse() {
        assert_eq!(
            SessionSpec::from_labels(&labels(&[(NAME_LABEL, "gnome")])).unwrap(),
            None
        );
        let s = SessionSpec::from_labels(&labels(&[
            (NAME_LABEL, "GNOME"),
            (RENDERER_LABEL, "screencast"),
            (
                EXEC_LABEL,
                r#"["gnome-shell","--headless","--virtual-monitor","{width}x{height}"]"#,
            ),
            (SERVICES_LABEL, r#"[["pipewire"],["wireplumber"]]"#),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(s.name.as_deref(), Some("GNOME"));
        assert_eq!(s.renderer, Renderer::Screencast);
        assert_eq!(s.exec[0], "gnome-shell");
        assert_eq!(
            s.services,
            vec![
                vec!["pipewire".to_string()],
                vec!["wireplumber".to_string()]
            ]
        );
    }

    #[test]
    fn bad_labels_are_invalid() {
        for kv in [
            vec![(RENDERER_LABEL, "x11")],
            vec![(RENDERER_LABEL, "wayland")],
            vec![(RENDERER_LABEL, "wayland"), (EXEC_LABEL, "gamescope")],
            vec![(RENDERER_LABEL, "wayland"), (EXEC_LABEL, "[]")],
            vec![
                (RENDERER_LABEL, "wayland"),
                (EXEC_LABEL, r#"["a"]"#),
                (SERVICES_LABEL, "[[]]"),
            ],
        ] {
            let e = SessionSpec::from_labels(&labels(&kv)).unwrap_err();
            assert_eq!(e.kind, crate::error::ErrorKind::Invalid, "{kv:?}");
        }
    }

    #[test]
    fn placeholders_fill() {
        let p = Params {
            width: 1920,
            height: 1200,
            scale: 1.5,
            wayland_display: "wayland-0".into(),
        };
        assert_eq!(
            fill(
                &[
                    "gamescope".into(),
                    "-W".into(),
                    "{width}".into(),
                    "-H".into(),
                    "{height}".into(),
                    "--scale={scale}".into(),
                    "--display={wayland_display}".into()
                ],
                &p
            ),
            vec![
                "gamescope",
                "-W",
                "1920",
                "-H",
                "1200",
                "--scale=1.5",
                "--display=wayland-0"
            ]
        );
    }

    #[test]
    fn override_wins_and_clears() {
        let d = tempfile::tempdir().unwrap();
        let root = Root::new(d.path());
        assert_eq!(resolve(d.path()).unwrap(), None);
        let s = SessionSpec {
            name: Some("Plasma".into()),
            renderer: Renderer::Wayland,
            exec: vec![
                "kwin_wayland".into(),
                "--wayland-display={wayland_display}".into(),
            ],
            services: vec![],
        };
        write_override(&root, &s).unwrap();
        assert_eq!(resolve(d.path()).unwrap(), Some((s, Source::Override)));
        assert!(clear_override(&root).unwrap());
        assert!(!clear_override(&root).unwrap());
        assert_eq!(resolve(d.path()).unwrap(), None);
        let bad = SessionSpec {
            name: None,
            renderer: Renderer::Wayland,
            exec: vec![],
            services: vec![],
        };
        assert!(write_override(&root, &bad).is_err());
    }
}
