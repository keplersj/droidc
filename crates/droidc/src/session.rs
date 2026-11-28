//! How a deployment's graphical sessions are started and rendered, see docs/design-mvp.md.
//!
//! An image offers sessions the way any Linux system does, with `wayland-sessions` desktop entries
//! (`/usr/share/wayland-sessions/*.desktop`, overridden by `/usr/local/share/wayland-sessions`),
//! plus `X-Droidc-*` keys that say how droidc shows each one. It can also declare one session with
//! labels (`containers.droidc.session.renderer`, `.exec`, `.services`); `<root>/session.json`
//! (written by `droidc session set`) is the user's own. droidc keeps no table of desktops:
//! whatever the image (or the user) names is what runs.

use crate::apps::{self, GuestFs};
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
/// Devices the label session suits, `;`- or `,`-separated (see [`FORM_FACTORS`]).
pub const FORM_FACTORS_LABEL: &str = "containers.droidc.session.form-factors";

/// Where session desktop entries live, lowest precedence first (XDG_DATA_DIRS order reversed).
pub const SESSION_DIRS: [&str; 2] = [
    "/usr/share/wayland-sessions",
    "/usr/local/share/wayland-sessions",
];

/// The device shapes a session can say it suits (`X-Droidc-FormFactors`, the form-factors label).
/// Others are kept as given, for frontends that know them.
pub const FORM_FACTORS: [&str; 7] = ["phone", "tablet", "desktop", "tv", "watch", "xr", "car"];

/// Ids of the sessions that don't come from a desktop entry.
pub const OVERRIDE_ID: &str = "override";
pub const LABEL_ID: &str = "label";

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
    /// Devices it suits (see [`FORM_FACTORS`]); empty: not said.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub form_factors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Label,
    Override,
    /// A `wayland-sessions` desktop entry.
    Xdg,
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
            form_factors: labels
                .get(FORM_FACTORS_LABEL)
                .map(|v| parse_form_factors(v))
                .unwrap_or_default(),
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

/// A `;`- or `,`-separated list of device shapes, lowercased.
pub fn parse_form_factors(v: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in v.split([';', ',']).map(|f| f.trim().to_ascii_lowercase()) {
        if !f.is_empty() && !out.contains(&f) {
            out.push(f);
        }
    }
    out
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
    root.cdir.join("session.json")
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
    fs::create_dir_all(&root.cdir).context(root.cdir.display())?;
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

/// One session a deployment offers, runnable or not.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// `override`, `label`, or the desktop entry's id without `.desktop` (`gnome`, `sm.puri.Phosh`).
    pub id: String,
    pub source: Source,
    pub name: Option<String>,
    pub comment: Option<String>,
    /// The entry's `DesktopNames`, exported (`:`-joined) as `XDG_CURRENT_DESKTOP`.
    pub desktop_names: Vec<String>,
    pub form_factors: Vec<String>,
    /// Guest path of the desktop entry.
    pub path: Option<String>,
    /// How to start it; `None` when droidc can't (see `problem`).
    pub spec: Option<SessionSpec>,
    pub problem: Option<String>,
}

impl Entry {
    fn from_spec(id: &str, source: Source, spec: SessionSpec) -> Entry {
        Entry {
            id: id.into(),
            source,
            name: spec.name.clone(),
            comment: None,
            desktop_names: Vec::new(),
            form_factors: spec.form_factors.clone(),
            path: None,
            spec: Some(spec),
            problem: None,
        }
    }

    pub fn runnable(&self) -> bool {
        self.spec.is_some()
    }

    /// `XDG_CURRENT_DESKTOP`: `DesktopNames` joined with `:`, else the name.
    pub fn current_desktop(&self) -> Option<String> {
        if self.desktop_names.is_empty() {
            self.name.clone()
        } else {
            Some(self.desktop_names.join(":"))
        }
    }

    /// `XDG_SESSION_DESKTOP`: the desktop entry's id, as display managers set it; else the
    /// lowercased name.
    pub fn session_desktop(&self) -> Option<String> {
        match self.source {
            Source::Xdg => Some(self.id.clone()),
            _ => self.name.as_ref().map(|n| n.to_lowercase()),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let s = self.spec.as_ref();
        serde_json::json!({
            "id": self.id,
            "source": self.source,
            "name": self.name,
            "comment": self.comment,
            "desktop_names": self.desktop_names,
            "form_factors": self.form_factors,
            "path": self.path,
            "runnable": self.runnable(),
            "renderer": s.map(|s| s.renderer),
            "exec": s.map(|s| &s.exec),
            "services": s.map(|s| s.services.clone()).unwrap_or_default(),
            "problem": self.problem,
        })
    }
}

/// Every session a deployment offers: the override, the label session, then desktop entries by id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub sessions: Vec<Entry>,
}

impl Catalog {
    /// The session started when none is named: the first runnable one.
    pub fn default_entry(&self) -> Option<&Entry> {
        self.sessions.iter().find(|e| e.runnable())
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.sessions.iter().find(|e| e.id == id)
    }

    /// The session to start: `id`, else the default.
    pub fn choose(&self, id: Option<&str>) -> Result<&Entry> {
        if let Some(id) = id {
            let e = self.get(id).ok_or_else(|| {
                Error::not_found(format!(
                    "no session '{id}' in this deployment (droidc session list)"
                ))
            })?;
            if !e.runnable() {
                return Err(Error::invalid(format!(
                    "session '{id}' can't start here: {}",
                    e.problem.as_deref().unwrap_or("no command")
                )));
            }
            return Ok(e);
        }
        if let Some(e) = self.default_entry() {
            return Ok(e);
        }
        // Broken labels say why there is nothing to start, as they did before desktop entries.
        if let Some(p) = self.get(LABEL_ID).and_then(|e| e.problem.as_deref()) {
            return Err(Error::invalid(p.to_string()));
        }
        Err(Error::not_found(
            "no graphical session: the image has no wayland-sessions entry with X-Droidc-Renderer, \
             sets no containers.droidc.session.* labels, and there is no override (droidc session set)",
        ))
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sessions": self.sessions.iter().map(Entry::to_json).collect::<Vec<_>>(),
            "default": self.default_entry().map(|e| &e.id),
        })
    }
}

/// The label session, `None` when the image declares no renderer; broken labels give an entry
/// that can't start, with the reason.
pub fn label_entry(labels: &BTreeMap<String, String>) -> Option<Entry> {
    match SessionSpec::from_labels(labels) {
        Ok(None) => None,
        Ok(Some(s)) => Some(Entry::from_spec(LABEL_ID, Source::Label, s)),
        Err(e) => Some(Entry {
            id: LABEL_ID.into(),
            source: Source::Label,
            name: labels.get(NAME_LABEL).cloned(),
            comment: None,
            desktop_names: Vec::new(),
            form_factors: Vec::new(),
            path: None,
            spec: None,
            problem: Some(e.message),
        }),
    }
}

/// The `wayland-sessions` desktop entries of a guest, by id. Entries that are `Hidden`, `NoDisplay`,
/// not of `Type=Application`, or whose `TryExec` isn't installed are left out (and hide the same id
/// in a lower-precedence directory), as a display manager would.
pub fn xdg_entries(g: &GuestFs) -> Vec<Entry> {
    let mut by_id: BTreeMap<String, Option<Entry>> = BTreeMap::new();
    for dir in SESSION_DIRS {
        let mut files = Vec::new();
        apps::desktop_files(g, Path::new(dir), "", &mut files, 0);
        for (file_id, path) in files {
            let id = file_id.trim_end_matches(".desktop").to_string();
            if id == OVERRIDE_ID || id == LABEL_ID {
                continue;
            }
            let entry = g
                .read_to_string(&path)
                .and_then(|t| xdg_entry(g, &id, &path, &t));
            by_id.insert(id, entry);
        }
    }
    by_id.into_values().flatten().collect()
}

/// Parses one session desktop entry; `None` when it is not to be listed.
/// Shells whose display-manager entry runs as it is nested in Selkie's compositor: Phosh's
/// `phosh-session` starts phoc, and wlroots picks its Wayland backend when `WAYLAND_DISPLAY` is set.
/// Their entries need no `X-Droidc-*` keys; an image can still set them to change anything.
const KNOWN_NESTED: &[&str] = &["Phosh"];

fn known_nested(desktop_names: &[String]) -> bool {
    desktop_names
        .iter()
        .any(|n| KNOWN_NESTED.iter().any(|k| n.eq_ignore_ascii_case(k)))
}

/// Devices a known shell suits when its entry doesn't say: Phosh is a phone shell.
fn known_form_factors(desktop_names: &[String]) -> Vec<String> {
    if desktop_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case("Phosh"))
    {
        vec!["phone".into()]
    } else {
        vec![]
    }
}

fn xdg_entry(g: &GuestFs, id: &str, path: &Path, text: &str) -> Option<Entry> {
    let groups = apps::parse_groups(text);
    let d = groups.get("Desktop Entry")?;
    if d.get("Type").is_some_and(|t| t.trim() != "Application")
        || apps::truthy(d.get("Hidden"))
        || apps::truthy(d.get("NoDisplay"))
    {
        return None;
    }
    if let Some(t) = d.get("TryExec").map(|t| t.trim()).filter(|t| !t.is_empty()) {
        if !apps::program_exists(g, t) {
            return None;
        }
    }
    let name = d.get("Name").cloned();
    let shown = name.clone().unwrap_or_else(|| id.to_string());
    let guest_path = path.display().to_string();
    let mut e = Entry {
        id: id.into(),
        source: Source::Xdg,
        name: name.clone(),
        comment: d.get("Comment").cloned(),
        desktop_names: apps::split_list(d.get("DesktopNames")),
        form_factors: d
            .get("X-Droidc-FormFactors")
            .map(|v| parse_form_factors(v))
            .unwrap_or_else(|| known_form_factors(&apps::split_list(d.get("DesktopNames")))),
        path: Some(guest_path.clone()),
        spec: None,
        problem: None,
    };
    let argv = |exec: &str| apps::exec_argv(exec, None, &shown, &guest_path);
    let built = (|| -> std::result::Result<SessionSpec, String> {
        let r = match d.get("X-Droidc-Renderer").map(|r| r.trim()) {
            Some(r) => r,
            // A shell droidc knows runs nested as it is, from its own unmodified entry.
            None if known_nested(&e.desktop_names) => "wayland",
            None => {
                return Err(
                    "made for a display manager: the entry has no X-Droidc-Renderer key".into(),
                )
            }
        };
        let renderer = Renderer::parse(r)
            .ok_or_else(|| format!("X-Droidc-Renderer: unknown renderer '{r}'"))?;
        let exec = d
            .get("X-Droidc-Exec")
            .or_else(|| d.get("Exec"))
            .ok_or("the entry has no Exec")?;
        let exec = argv(exec).ok_or_else(|| format!("can't parse the command '{exec}'"))?;
        let mut services = Vec::new();
        for c in apps::split_list(d.get("X-Droidc-Services")) {
            services.push(argv(&c).ok_or_else(|| format!("X-Droidc-Services: can't parse '{c}'"))?);
        }
        let s = SessionSpec {
            name: name.clone(),
            renderer,
            exec,
            services,
            form_factors: e.form_factors.clone(),
        };
        s.validate().map_err(|e| e.message)?;
        Ok(s)
    })();
    match built {
        Ok(s) => e.spec = Some(s),
        Err(p) => e.problem = Some(p),
    }
    Some(e)
}

/// Every session of `root`'s current deployment (see [`Catalog`]).
pub fn catalog(root: &Root) -> Result<Catalog> {
    let mut sessions = Vec::new();
    if let Some(s) = read_override(root)? {
        sessions.push(Entry::from_spec(OVERRIDE_ID, Source::Override, s));
    }
    if let Some(id) = root.current_id() {
        let rec = root.read_record(&id)?;
        sessions.extend(label_entry(&rec.labels));
        sessions.extend(xdg_entries(&GuestFs::for_root(root)?));
    }
    Ok(Catalog { sessions })
}

/// The session started when none is named: the override, else the label session, else the first
/// runnable desktop entry.
pub fn resolve(root: &Root) -> Result<Option<(SessionSpec, Source)>> {
    let c = catalog(root)?;
    Ok(c.default_entry()
        .and_then(|e| e.spec.clone().map(|s| (s, e.source))))
}

/// `shell-spec`'s `session` object (also `droidc session show`): the default session.
pub fn describe(default: Option<&Entry>, label_name: Option<&str>) -> serde_json::Value {
    match default.and_then(|e| e.spec.as_ref().map(|s| (e, s))) {
        Some((e, s)) => serde_json::json!({
            "id": e.id,
            "name": s.name.as_deref().or(label_name),
            "renderer": s.renderer,
            "exec": s.exec,
            "services": s.services,
            "configured": true,
            "source": e.source,
        }),
        None => serde_json::json!({
            "id": null,
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
        assert_eq!(resolve(&Root::new(d.path())).unwrap(), None);
        let s = SessionSpec {
            name: Some("Plasma".into()),
            renderer: Renderer::Wayland,
            exec: vec![
                "kwin_wayland".into(),
                "--wayland-display={wayland_display}".into(),
            ],
            services: vec![],
            form_factors: vec!["desktop".into()],
        };
        write_override(&root, &s).unwrap();
        assert_eq!(
            resolve(&Root::new(d.path())).unwrap(),
            Some((s, Source::Override))
        );
        assert!(clear_override(&root).unwrap());
        assert!(!clear_override(&root).unwrap());
        assert_eq!(resolve(&Root::new(d.path())).unwrap(), None);
        let bad = SessionSpec {
            name: None,
            renderer: Renderer::Wayland,
            exec: vec![],
            services: vec![],
            form_factors: vec![],
        };
        assert!(write_override(&root, &bad).is_err());
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    /// A guest with gnome-shell, phoc and phosh installed and a few session entries.
    fn guest() -> (tempfile::TempDir, GuestFs) {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("rootfs");
        for b in ["gnome-shell", "phoc", "phosh-session"] {
            write(&r.join("usr/bin").join(b), "");
        }
        let ws = r.join("usr/share/wayland-sessions");
        // What gnome-session ships: for a display manager, nothing for droidc.
        write(
            &ws.join("gnome-wayland.desktop"),
            "[Desktop Entry]\nName=GNOME\nComment=This session logs you into GNOME\n\
             Exec=gnome-session\nTryExec=gnome-shell\nType=Application\nDesktopNames=GNOME\n",
        );
        write(
            &ws.join("gnome-droidc.desktop"),
            "[Desktop Entry]\nName=GNOME\nName[de]=GNOME\nExec=gnome-session\nTryExec=gnome-shell\n\
             DesktopNames=GNOME\nX-Droidc-Renderer=screencast\n\
             X-Droidc-Exec=gnome-shell --headless --virtual-monitor {width}x{height}\n\
             X-Droidc-Services=pipewire;wireplumber\nX-Droidc-FormFactors=tablet;Desktop;xr\n",
        );
        write(
            &ws.join("sm.puri.Phosh.desktop"),
            "[Desktop Entry]\nName=Phosh\nComment=Phone Shell\nExec=phosh-session\n\
             Type=Application\nDesktopNames=Phosh;GNOME\nX-Droidc-Renderer=wayland\n\
             X-Droidc-FormFactors=phone\n",
        );
        // Not installed, hidden, or broken.
        write(
            &ws.join("plasma.desktop"),
            "[Desktop Entry]\nName=Plasma\nExec=startplasma-wayland\nTryExec=startplasma-wayland\n\
             X-Droidc-Renderer=wayland\n",
        );
        write(
            &ws.join("hidden.desktop"),
            "[Desktop Entry]\nName=Hidden\nExec=phoc\nNoDisplay=true\nX-Droidc-Renderer=wayland\n",
        );
        write(
            &ws.join("weird.desktop"),
            "[Desktop Entry]\nName=Weird\nExec=phoc\nX-Droidc-Renderer=x11\n",
        );
        write(
            &ws.join("label.desktop"),
            "[Desktop Entry]\nName=Clash\nExec=phoc\nX-Droidc-Renderer=wayland\n",
        );
        let g = GuestFs::new(&r, vec![]);
        (t, g)
    }

    #[test]
    fn desktop_entries_list_sessions() {
        let (t, g) = guest();
        let es = xdg_entries(&g);
        let ids: Vec<&str> = es.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            ["gnome-droidc", "gnome-wayland", "sm.puri.Phosh", "weird"]
        );

        let gnome = &es[0];
        assert_eq!(gnome.source, Source::Xdg);
        assert_eq!(gnome.form_factors, ["tablet", "desktop", "xr"]);
        let spec = gnome.spec.as_ref().unwrap();
        assert_eq!(spec.renderer, Renderer::Screencast);
        assert_eq!(
            spec.exec,
            [
                "gnome-shell",
                "--headless",
                "--virtual-monitor",
                "{width}x{height}"
            ]
        );
        assert_eq!(spec.services, [vec!["pipewire"], vec!["wireplumber"]]);
        assert_eq!(gnome.current_desktop().as_deref(), Some("GNOME"));
        assert_eq!(gnome.session_desktop().as_deref(), Some("gnome-droidc"));
        assert_eq!(
            gnome.path.as_deref(),
            Some("/usr/share/wayland-sessions/gnome-droidc.desktop")
        );

        let dm = &es[1];
        assert!(!dm.runnable());
        assert!(dm.problem.as_deref().unwrap().contains("X-Droidc-Renderer"));
        assert_eq!(
            dm.comment.as_deref(),
            Some("This session logs you into GNOME")
        );

        let phosh = &es[2];
        assert_eq!(phosh.spec.as_ref().unwrap().exec, ["phosh-session"]);
        assert_eq!(phosh.current_desktop().as_deref(), Some("Phosh:GNOME"));
        assert!(es[3].problem.as_deref().unwrap().contains("x11"));

        // /usr/local/share overrides by id: here it hides GNOME's own entry and changes Phosh.
        let local = t.path().join("rootfs/usr/local/share/wayland-sessions");
        write(
            &local.join("gnome-wayland.desktop"),
            "[Desktop Entry]\nName=GNOME\nExec=x\nHidden=true\n",
        );
        write(
            &local.join("sm.puri.Phosh.desktop"),
            "[Desktop Entry]\nName=Phosh (local)\nExec=phoc -E phosh-session\nX-Droidc-Renderer=wayland\n",
        );
        let es = xdg_entries(&g);
        let ids: Vec<&str> = es.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["gnome-droidc", "sm.puri.Phosh", "weird"]);
        assert_eq!(es[1].name.as_deref(), Some("Phosh (local)"));
        assert_eq!(
            es[1].spec.as_ref().unwrap().exec,
            ["phoc", "-E", "phosh-session"]
        );
        assert!(es[1].form_factors.is_empty());
    }

    #[test]
    fn phoshs_own_entry_runs_nested() {
        let (t, g) = guest();
        // As the phosh package installs it: no X-Droidc-* keys.
        write(
            &t.path().join("rootfs/usr/local/share/wayland-sessions/sm.puri.Phosh.desktop"),
            "[Desktop Entry]\nName=Phosh\nComment=Phone Shell\nExec=phosh-session\nType=Application\nDesktopNames=Phosh;GNOME;\n",
        );
        let es = xdg_entries(&g);
        let phosh = es.iter().find(|e| e.id == "sm.puri.Phosh").unwrap();
        assert!(phosh.runnable(), "{:?}", phosh.problem);
        let spec = phosh.spec.as_ref().unwrap();
        assert_eq!(spec.renderer, Renderer::Wayland);
        assert_eq!(spec.exec, ["phosh-session"]);
        assert_eq!(phosh.form_factors, ["phone"]);
        // GNOME's display-manager entry still needs the keys.
        assert!(!es
            .iter()
            .find(|e| e.id == "gnome-wayland")
            .unwrap()
            .runnable());
    }

    #[test]
    fn catalog_picks_and_explains() {
        let (_t, g) = guest();
        let label = label_entry(&labels(&[
            (NAME_LABEL, "GNOME"),
            (RENDERER_LABEL, "screencast"),
            (EXEC_LABEL, r#"["gnome-shell","--headless"]"#),
            (FORM_FACTORS_LABEL, "tablet, desktop"),
        ]))
        .unwrap();
        assert_eq!(label.form_factors, ["tablet", "desktop"]);
        let mut c = Catalog {
            sessions: std::iter::once(label).chain(xdg_entries(&g)).collect(),
        };
        assert_eq!(c.default_entry().unwrap().id, LABEL_ID);
        assert_eq!(c.choose(None).unwrap().id, LABEL_ID);
        assert_eq!(c.choose(Some("sm.puri.Phosh")).unwrap().id, "sm.puri.Phosh");
        let e = c.choose(Some("gnome-wayland")).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Invalid);
        assert!(e.message.contains("X-Droidc-Renderer"), "{e}");
        let e = c.choose(Some("nope")).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::NotFound);
        let j = c.to_json();
        assert_eq!(j["default"], "label");
        assert_eq!(j["sessions"][2]["runnable"], false);
        assert_eq!(j["sessions"][3]["form_factors"][0], "phone");
        assert_eq!(describe(c.default_entry(), None)["id"], "label");

        // Without labels the first runnable entry is the default.
        c.sessions.remove(0);
        assert_eq!(c.default_entry().unwrap().id, "gnome-droidc");

        // Broken labels and nothing else to run: the label problem is the error.
        let broken = label_entry(&labels(&[(RENDERER_LABEL, "x11")])).unwrap();
        assert!(!broken.runnable());
        let c = Catalog {
            sessions: vec![broken],
        };
        let e = c.choose(None).unwrap_err();
        assert!(e.message.contains("unknown renderer"), "{e}");
        assert_eq!(
            Catalog::default().choose(None).unwrap_err().kind,
            crate::error::ErrorKind::NotFound
        );
    }
}
