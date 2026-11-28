//! `droidc apps`: the applications a container offers, read from its freedesktop desktop entries.
//!
//! Runs on the host. The guest's file system is the current deployment's `rootfs` with the
//! container's own `var/` and `home/` over `/var` and `/home` (as `shell-spec` binds them), so guest
//! paths are resolved here the same way, symlinks included (`/home` -> `var/home` on bootc images).
//!
//! Entries come from the XDG data dirs the guest would use: the image's (`/usr/local/share`,
//! `/usr/share`), system Flatpak exports, then every user's `~/.local/share` and user Flatpak
//! exports. A user's entry overrides the image's with the same desktop ID, as in the spec. Icons are
//! looked up in the hicolor theme and `pixmaps`, and given as host paths the app can read.

use crate::error::{Error, Result};
use crate::layout::Root;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Where an entry came from, which is also how Selkie groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    /// Shipped in the image (`/usr/share/applications`, `/usr/local/share/applications`).
    Image,
    /// A system-wide Flatpak (`/var/lib/flatpak/exports`).
    Flatpak,
    /// The user's own (`~/.local/share/applications`).
    User,
    /// A per-user Flatpak (`~/.local/share/flatpak/exports`).
    UserFlatpak,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Icon {
    /// As the entry names it: a theme name or an absolute guest path.
    pub name: String,
    /// The file found for it, readable on the host. Prefers a PNG of at least 96 px, then SVG.
    pub host_path: Option<String>,
    /// `png`, `svg` or `xpm`.
    pub format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    pub id: String,
    pub name: String,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct App {
    /// Desktop ID, e.g. `org.gnome.TextEditor.desktop`.
    pub id: String,
    pub name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub mime_types: Vec<String>,
    /// `Exec` as written.
    pub exec: String,
    /// `Exec` split into arguments, with field codes for files and URLs dropped (nothing is opened).
    pub argv: Vec<String>,
    pub terminal: bool,
    /// `OnlyShowIn` names desktops: the app expects to run inside one of them.
    pub only_show_in: Vec<String>,
    pub needs_desktop: bool,
    pub dbus_activatable: bool,
    pub startup_wm_class: Option<String>,
    pub origin: Origin,
    /// Guest path of the entry.
    pub path: String,
    pub icon: Option<Icon>,
    pub actions: Vec<Action>,
}

/// Why an entry isn't listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HiddenReason {
    /// `NoDisplay=true`: a helper or handler, not something to open from a menu.
    NoDisplay,
    /// `Hidden=true`: deleted, as far as the desktop is concerned.
    Hidden,
    /// `TryExec` names a program that isn't there.
    NotInstalled,
    /// Not `Type=Application`, no `Name` or no `Exec`.
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HiddenEntry {
    pub id: String,
    pub origin: Origin,
    pub reason: HiddenReason,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppList {
    pub container: String,
    pub apps: Vec<App>,
    pub hidden: Vec<HiddenEntry>,
}

/// The guest file system as the host sees it: guest paths to host paths, symlinks followed.
#[derive(Debug, Clone)]
pub struct GuestFs {
    rootfs: PathBuf,
    /// Guest prefix -> host directory, longest first.
    binds: Vec<(PathBuf, PathBuf)>,
}

impl GuestFs {
    pub fn new(rootfs: impl Into<PathBuf>, binds: Vec<(PathBuf, PathBuf)>) -> GuestFs {
        let mut binds = binds;
        binds.sort_by_key(|(g, _)| std::cmp::Reverse(g.components().count()));
        GuestFs {
            rootfs: rootfs.into(),
            binds,
        }
    }

    /// The guest file system of `root`'s current deployment, as `shell-spec` binds it.
    pub fn for_root(root: &Root) -> Result<GuestFs> {
        if root.current_id().is_none() {
            return Err(Error::not_found("no current deployment"));
        }
        Ok(GuestFs::new(
            root.cdir.join("current/rootfs"),
            vec![
                (PathBuf::from("/var"), root.cdir.join("var")),
                (PathBuf::from("/home"), root.cdir.join("home")),
            ],
        ))
    }

    /// Host path of a (normalised, absolute) guest path, without following symlinks.
    fn host_of(&self, guest: &Path) -> PathBuf {
        for (g, h) in &self.binds {
            if let Ok(rest) = guest.strip_prefix(g) {
                return h.join(rest);
            }
        }
        self.rootfs.join(guest.strip_prefix("/").unwrap_or(guest))
    }

    /// Resolves `guest` inside the guest, following symlinks as the guest kernel would, and returns
    /// the guest path it ends at. `None` when a symlink loop or too many links get in the way.
    pub fn resolve(&self, guest: &Path) -> Option<PathBuf> {
        let mut done = PathBuf::from("/");
        let mut todo: Vec<std::ffi::OsString> = guest
            .components()
            .rev()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_os_string()),
                Component::ParentDir => Some("..".into()),
                _ => None,
            })
            .collect();
        let mut hops = 0;
        while let Some(part) = todo.pop() {
            if part == ".." {
                done.pop();
                continue;
            }
            let next = done.join(&part);
            match fs::symlink_metadata(self.host_of(&next)) {
                Ok(m) if m.file_type().is_symlink() => {
                    hops += 1;
                    if hops > 40 {
                        return None;
                    }
                    let target = fs::read_link(self.host_of(&next)).ok()?;
                    if target.is_absolute() {
                        done = PathBuf::from("/");
                    }
                    for c in target.components().rev() {
                        match c {
                            Component::Normal(s) => todo.push(s.to_os_string()),
                            Component::ParentDir => todo.push("..".into()),
                            _ => {}
                        }
                    }
                }
                _ => done = next,
            }
        }
        Some(done)
    }

    /// Host path of `guest` after following its symlinks.
    pub fn host(&self, guest: &Path) -> Option<PathBuf> {
        self.resolve(guest).map(|g| self.host_of(&g))
    }

    pub(crate) fn exists(&self, guest: &Path) -> bool {
        self.host(guest).is_some_and(|h| h.exists())
    }

    /// Guest paths of the entries in directory `guest` (empty if it isn't one).
    pub(crate) fn read_dir(&self, guest: &Path) -> Vec<PathBuf> {
        let Some(g) = self.resolve(guest) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = fs::read_dir(self.host_of(&g))
            .map(|it| it.flatten().map(|e| g.join(e.file_name())).collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    pub(crate) fn read_to_string(&self, guest: &Path) -> Option<String> {
        fs::read_to_string(self.host(guest)?).ok()
    }

    pub(crate) fn is_dir(&self, guest: &Path) -> bool {
        self.host(guest).is_some_and(|h| h.is_dir())
    }
}

/// Home directories in the guest: every directory under `/home` (or where it points), and `/root`.
fn homes(g: &GuestFs) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = g
        .read_dir(Path::new("/home"))
        .into_iter()
        .filter(|p| g.is_dir(p))
        .collect();
    out.push(PathBuf::from("/root"));
    out
}

/// Data dirs with their origin, lowest precedence first (later ones override earlier IDs).
fn data_dirs(g: &GuestFs) -> Vec<(PathBuf, Origin)> {
    let mut dirs = vec![
        (PathBuf::from("/usr/share"), Origin::Image),
        (PathBuf::from("/usr/local/share"), Origin::Image),
        (
            PathBuf::from("/var/lib/flatpak/exports/share"),
            Origin::Flatpak,
        ),
    ];
    for h in homes(g) {
        dirs.push((
            h.join(".local/share/flatpak/exports/share"),
            Origin::UserFlatpak,
        ));
        dirs.push((h.join(".local/share"), Origin::User));
    }
    dirs
}

/// Every `.desktop` file under `dir`, with its desktop ID (subdirectories joined with `-`).
pub(crate) fn desktop_files(
    g: &GuestFs,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf)>,
    depth: u32,
) {
    if depth > 4 {
        return;
    }
    for p in g.read_dir(dir) {
        let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if g.is_dir(&p) {
            desktop_files(g, &p, &format!("{prefix}{name}-"), out, depth + 1);
        } else if name.ends_with(".desktop") {
            out.push((format!("{prefix}{name}"), p));
        }
    }
}

/// The groups of a desktop entry file: `[Desktop Entry]`, `[Desktop Action x]`, ... Keys with a
/// locale (`Name[fr]`) are skipped: the unlocalised value is what's shown.
pub fn parse_groups(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut groups = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].to_string();
            groups.entry(name.clone()).or_insert_with(BTreeMap::new);
            current = Some(name);
            continue;
        }
        let (Some(group), Some((k, v))) = (&current, line.split_once('=')) else {
            continue;
        };
        let k = k.trim();
        if k.contains('[') {
            continue;
        }
        groups
            .get_mut(group)
            .expect("group was inserted")
            .entry(k.to_string())
            .or_insert_with(|| unescape(v.trim()));
    }
    groups
}

/// The spec's escapes for string values: `\s`, `\n`, `\t`, `\r`, `\\`.
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            // Leave others (`\;` in lists, quoting inside Exec) for later stages.
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A `;`-separated list (a `\;` stays in the item).
pub(crate) fn split_list(v: Option<&String>) -> Vec<String> {
    let Some(v) = v else { return Vec::new() };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&';') {
            cur.push(';');
            chars.next();
        } else if c == ';' {
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

pub(crate) fn truthy(v: Option<&String>) -> bool {
    v.is_some_and(|v| v.trim() == "true")
}

/// Splits `Exec` into arguments per the spec's quoting, then expands field codes: `%f %F %u %U` (and
/// the deprecated ones) go, since nothing is being opened; `%i` becomes `--icon ICON`, `%c` the name,
/// `%k` the entry's path, `%%` a `%`. `None` when the quoting is broken.
pub fn exec_argv(exec: &str, icon: Option<&str>, name: &str, path: &str) -> Option<Vec<String>> {
    let mut words: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_word = false;
    let mut was_quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' => cur.push(chars.next()?),
                _ => cur.push(c),
            }
        } else {
            match c {
                '"' => {
                    quoted = true;
                    in_word = true;
                    was_quoted = true;
                }
                ' ' | '\t' | '\n' => {
                    if in_word {
                        words.push((std::mem::take(&mut cur), was_quoted));
                        in_word = false;
                        was_quoted = false;
                    }
                }
                _ => {
                    cur.push(c);
                    in_word = true;
                }
            }
        }
    }
    if quoted {
        return None;
    }
    if in_word {
        words.push((cur, was_quoted));
    }
    let mut argv = Vec::new();
    for (w, was_quoted) in words {
        // A field code on its own: expand to zero, one or two arguments.
        if !was_quoted && w.len() == 2 && w.starts_with('%') {
            match &w[1..] {
                "i" => {
                    if let Some(i) = icon {
                        argv.push("--icon".into());
                        argv.push(i.to_string());
                    }
                    continue;
                }
                "c" => {
                    argv.push(name.to_string());
                    continue;
                }
                "k" => {
                    argv.push(path.to_string());
                    continue;
                }
                "%" => {
                    argv.push("%".into());
                    continue;
                }
                _ => continue,
            }
        }
        // Inside a word: `%%` is a `%`, other codes are dropped.
        let mut out = String::new();
        let mut cs = w.chars();
        while let Some(c) = cs.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match cs.next() {
                Some('%') => out.push('%'),
                Some('c') => out.push_str(name),
                Some('k') => out.push_str(path),
                _ => {}
            }
        }
        argv.push(out);
    }
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Whether `prog` (a name looked up on the guest's PATH, or an absolute path) is there.
pub(crate) fn program_exists(g: &GuestFs, prog: &str) -> bool {
    if prog.starts_with('/') {
        return g.exists(Path::new(prog));
    }
    crate::status::DEFAULT_PATH
        .split(':')
        .chain(["/var/lib/flatpak/exports/bin"])
        .any(|d| g.exists(&Path::new(d).join(prog)))
}

const ICON_SIZES: [&str; 9] = [
    "256x256", "192x192", "128x128", "96x96", "512x512", "64x64", "48x48", "32x32", "scalable",
];

/// Finds an icon file for `name` in the hicolor theme and `pixmaps`, as the host can read it.
fn find_icon(g: &GuestFs, name: &str) -> Icon {
    let found = |p: PathBuf| -> Option<(PathBuf, String)> {
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        let host = g.host(&p)?;
        host.is_file().then_some((host, ext))
    };
    let mut hit: Option<(PathBuf, String)> = None;
    if name.starts_with('/') {
        hit = found(PathBuf::from(name));
    } else {
        let mut bases: Vec<PathBuf> = Vec::new();
        for h in homes(g) {
            bases.push(h.join(".local/share/icons"));
            bases.push(h.join(".local/share/flatpak/exports/share/icons"));
        }
        bases.extend(
            [
                "/usr/local/share/icons",
                "/usr/share/icons",
                "/var/lib/flatpak/exports/share/icons",
            ]
            .map(PathBuf::from),
        );
        'search: for size in ICON_SIZES {
            for base in &bases {
                for ext in ["png", "svg"] {
                    if let Some(h) = found(base.join(format!("hicolor/{size}/apps/{name}.{ext}"))) {
                        hit = Some(h);
                        break 'search;
                    }
                }
            }
        }
        if hit.is_none() {
            for ext in ["png", "svg", "xpm"] {
                if let Some(h) = found(PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"))) {
                    hit = Some(h);
                    break;
                }
            }
        }
    }
    Icon {
        name: name.to_string(),
        host_path: hit.as_ref().map(|(p, _)| p.display().to_string()),
        format: hit.map(|(_, e)| e),
    }
}

/// Reads one entry. `Err` carries why it isn't listed.
fn read_entry(
    g: &GuestFs,
    id: &str,
    path: &Path,
    origin: Origin,
) -> std::result::Result<App, HiddenReason> {
    let text = g.read_to_string(path).ok_or(HiddenReason::Invalid)?;
    let groups = parse_groups(&text);
    let e = groups.get("Desktop Entry").ok_or(HiddenReason::Invalid)?;
    if e.get("Type").map(String::as_str) != Some("Application") {
        return Err(HiddenReason::Invalid);
    }
    if truthy(e.get("Hidden")) {
        return Err(HiddenReason::Hidden);
    }
    if truthy(e.get("NoDisplay")) {
        return Err(HiddenReason::NoDisplay);
    }
    let name = e.get("Name").cloned().ok_or(HiddenReason::Invalid)?;
    let exec = e.get("Exec").cloned().ok_or(HiddenReason::Invalid)?;
    if let Some(t) = e.get("TryExec") {
        if !program_exists(g, t) {
            return Err(HiddenReason::NotInstalled);
        }
    }
    let path_s = path.display().to_string();
    let icon_name = e.get("Icon").filter(|i| !i.is_empty());
    let argv = exec_argv(&exec, icon_name.map(String::as_str), &name, &path_s)
        .ok_or(HiddenReason::Invalid)?;
    let actions = split_list(e.get("Actions"))
        .into_iter()
        .filter_map(|a| {
            let grp = groups.get(&format!("Desktop Action {a}"))?;
            let an = grp.get("Name")?.clone();
            let argv = exec_argv(
                grp.get("Exec")?,
                icon_name.map(String::as_str),
                &name,
                &path_s,
            )?;
            Some(Action {
                id: a,
                name: an,
                argv,
            })
        })
        .collect();
    let only_show_in = split_list(e.get("OnlyShowIn"));
    Ok(App {
        id: id.to_string(),
        generic_name: e.get("GenericName").cloned(),
        comment: e.get("Comment").cloned(),
        categories: split_list(e.get("Categories")),
        keywords: split_list(e.get("Keywords")),
        mime_types: split_list(e.get("MimeType")),
        terminal: truthy(e.get("Terminal")),
        needs_desktop: !only_show_in.is_empty(),
        only_show_in,
        dbus_activatable: truthy(e.get("DBusActivatable")),
        startup_wm_class: e.get("StartupWMClass").cloned(),
        origin,
        path: path_s,
        icon: icon_name.map(|i| find_icon(g, i)),
        actions,
        name,
        exec,
        argv,
    })
}

/// Every application in the guest, by name; and those left out, with why.
pub fn list_in(g: &GuestFs, container: &str) -> AppList {
    // Later dirs override earlier ones with the same ID.
    let mut by_id: BTreeMap<String, (PathBuf, Origin)> = BTreeMap::new();
    for (dir, origin) in data_dirs(g) {
        let mut files = Vec::new();
        desktop_files(g, &dir.join("applications"), "", &mut files, 0);
        for (id, path) in files {
            by_id.insert(id, (path, origin));
        }
    }
    let mut apps = Vec::new();
    let mut hidden = Vec::new();
    for (id, (path, origin)) in by_id {
        match read_entry(g, &id, &path, origin) {
            Ok(a) => apps.push(a),
            Err(reason) => hidden.push(HiddenEntry { id, origin, reason }),
        }
    }
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    AppList {
        container: container.to_string(),
        apps,
        hidden,
    }
}

/// `droidc apps`: the current deployment's applications, as JSON.
pub fn list(root: &Root) -> Result<Value> {
    let g = GuestFs::for_root(root)?;
    Ok(json!(list_in(&g, &root.container)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    /// A guest with bootc's `/home -> var/home` and `/root -> var/roothome`.
    fn guest() -> (tempfile::TempDir, GuestFs) {
        let t = tempfile::tempdir().unwrap();
        let rootfs = t.path().join("rootfs");
        let var = t.path().join("var");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(var.join("home/kepler")).unwrap();
        fs::create_dir_all(var.join("roothome")).unwrap();
        symlink("var/home", rootfs.join("home")).unwrap();
        symlink("var/roothome", rootfs.join("root")).unwrap();
        let g = GuestFs::new(&rootfs, vec![(PathBuf::from("/var"), var)]);
        (t, g)
    }

    #[test]
    fn exec_is_split_and_field_codes_go() {
        let a = |e: &str| {
            exec_argv(
                e,
                Some("ed"),
                "Text Editor",
                "/usr/share/applications/x.desktop",
            )
        };
        assert_eq!(a("gnome-text-editor %U").unwrap(), ["gnome-text-editor"]);
        assert_eq!(
            a(r#"sh -c "echo \"hi there\" 100%%" %F"#).unwrap(),
            ["sh", "-c", r#"echo "hi there" 100%"#]
        );
        assert_eq!(
            a("app %i --name=%c").unwrap(),
            ["app", "--icon", "ed", "--name=Text Editor"]
        );
        assert_eq!(
            a("/usr/bin/flatpak run --branch=stable --command=zed dev.zed.Zed @@u %U @@").unwrap(),
            [
                "/usr/bin/flatpak",
                "run",
                "--branch=stable",
                "--command=zed",
                "dev.zed.Zed",
                "@@u",
                "@@"
            ]
        );
        assert_eq!(a(r#"broken "quote"#), None);
        assert_eq!(a("%U"), None);
    }

    #[test]
    fn groups_skip_locales_and_unescape() {
        let g = parse_groups(
            "# c\n[Desktop Entry]\nName=Files\nName[fr]=Fichiers\nComment=a\\sb\nActions=new;\n\n[Desktop Action new]\nName=New Window\nExec=nautilus --new-window\n",
        );
        assert_eq!(g["Desktop Entry"]["Name"], "Files");
        assert_eq!(g["Desktop Entry"]["Comment"], "a b");
        assert_eq!(g["Desktop Action new"]["Exec"], "nautilus --new-window");
        assert_eq!(split_list(Some(&"a;b\\;c;;".to_string())), ["a", "b;c"]);
    }

    #[test]
    fn resolve_follows_symlinks_into_binds() {
        let (_t, g) = guest();
        assert_eq!(
            g.resolve(Path::new("/home/kepler")).unwrap(),
            Path::new("/var/home/kepler")
        );
        // `..` after a symlink goes up from where it points, as the kernel does.
        assert_eq!(
            g.resolve(Path::new("/root/../usr")).unwrap(),
            Path::new("/var/usr")
        );
        assert!(g
            .host(Path::new("/home/kepler"))
            .unwrap()
            .ends_with("var/home/kepler"));
    }

    #[test]
    fn lists_apps_with_icons_and_hides_the_rest() {
        let (_t, g) = guest();
        let host = |p: &str| g.host(Path::new(p)).unwrap();
        write(
            &host("/usr/share/applications/org.gnome.TextEditor.desktop"),
            "[Desktop Entry]\nType=Application\nName=Text Editor\nComment=Edit text files\nExec=gnome-text-editor %U\nIcon=org.gnome.TextEditor\nCategories=GNOME;GTK;Utility;TextEditor;\nMimeType=text/plain;\nDBusActivatable=true\nActions=new-window;\n\n[Desktop Action new-window]\nName=New Window\nExec=gnome-text-editor --new-window\n",
        );
        write(
            &host("/usr/share/icons/hicolor/scalable/apps/org.gnome.TextEditor.svg"),
            "<svg/>",
        );
        write(
            &host("/usr/share/applications/org.gnome.Settings.desktop"),
            "[Desktop Entry]\nType=Application\nName=Settings\nExec=gnome-control-center\nIcon=org.gnome.Settings\nOnlyShowIn=GNOME;Unity;\n",
        );
        write(
            &host("/usr/share/icons/hicolor/48x48/apps/org.gnome.Settings.png"),
            "png",
        );
        write(
            &host("/usr/share/icons/hicolor/256x256/apps/org.gnome.Settings.png"),
            "png",
        );
        write(
            &host("/usr/share/applications/mimeinfo.desktop"),
            "[Desktop Entry]\nType=Application\nName=Handler\nExec=handler %u\nNoDisplay=true\n",
        );
        write(
            &host("/usr/share/applications/gone.desktop"),
            "[Desktop Entry]\nType=Application\nName=Gone\nExec=gone\nTryExec=gone\n",
        );
        write(
            &host("/usr/share/applications/kde4/old.desktop"),
            "[Desktop Entry]\nType=Link\nName=Old\nURL=x\n",
        );
        // The user's own entry overrides the image's with the same ID.
        write(
            &host("/home/kepler/.local/share/applications/org.gnome.Settings.desktop"),
            "[Desktop Entry]\nType=Application\nName=Settings\nHidden=true\n",
        );
        write(
            &host("/home/kepler/.local/share/applications/dev.zed.Zed.desktop"),
            "[Desktop Entry]\nType=Application\nName=Zed\nExec=zed %F\nIcon=/home/kepler/.local/zed.app/zed.png\n",
        );
        write(&host("/home/kepler/.local/zed.app/zed.png"), "png");
        write(
            &host("/var/lib/flatpak/exports/share/applications/org.gnome.Calculator.desktop"),
            "[Desktop Entry]\nType=Application\nName=Calculator\nExec=/usr/bin/flatpak run --branch=stable --arch=aarch64 --command=gnome-calculator org.gnome.Calculator\nIcon=org.gnome.Calculator\nTerminal=false\n",
        );
        write(
            &host("/var/lib/flatpak/exports/share/icons/hicolor/128x128/apps/org.gnome.Calculator.png"),
            "png",
        );

        let l = list_in(&g, "default");
        let names: Vec<&str> = l.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Calculator", "Text Editor", "Zed"]);

        let ed = &l.apps[1];
        assert_eq!(ed.argv, ["gnome-text-editor"]);
        assert_eq!(ed.origin, Origin::Image);
        assert!(ed.dbus_activatable && !ed.needs_desktop);
        assert_eq!(ed.mime_types, ["text/plain"]);
        assert_eq!(ed.actions[0].name, "New Window");
        let icon = ed.icon.as_ref().unwrap();
        assert_eq!(icon.format.as_deref(), Some("svg"));
        assert!(icon
            .host_path
            .as_ref()
            .unwrap()
            .ends_with("scalable/apps/org.gnome.TextEditor.svg"));

        assert_eq!(l.apps[0].origin, Origin::Flatpak);
        assert_eq!(
            l.apps[0].icon.as_ref().unwrap().format.as_deref(),
            Some("png")
        );
        assert_eq!(l.apps[2].origin, Origin::User);
        assert!(l.apps[2].icon.as_ref().unwrap().host_path.is_some());

        let hidden: Vec<(&str, HiddenReason)> =
            l.hidden.iter().map(|h| (h.id.as_str(), h.reason)).collect();
        assert_eq!(
            hidden,
            [
                ("gone.desktop", HiddenReason::NotInstalled),
                ("kde4-old.desktop", HiddenReason::Invalid),
                ("mimeinfo.desktop", HiddenReason::NoDisplay),
                ("org.gnome.Settings.desktop", HiddenReason::Hidden),
            ]
        );
    }

    #[test]
    fn needs_desktop_and_the_largest_png_first() {
        let (_t, g) = guest();
        let host = |p: &str| g.host(Path::new(p)).unwrap();
        write(
            &host("/usr/share/applications/org.gnome.Settings.desktop"),
            "[Desktop Entry]\nType=Application\nName=Settings\nExec=gnome-control-center\nIcon=org.gnome.Settings\nOnlyShowIn=GNOME;Unity;\n",
        );
        write(
            &host("/usr/share/icons/hicolor/48x48/apps/org.gnome.Settings.png"),
            "png",
        );
        write(
            &host("/usr/share/icons/hicolor/256x256/apps/org.gnome.Settings.png"),
            "png",
        );
        let l = list_in(&g, "default");
        assert!(l.apps[0].needs_desktop);
        assert_eq!(l.apps[0].only_show_in, ["GNOME", "Unity"]);
        assert!(l.apps[0]
            .icon
            .as_ref()
            .unwrap()
            .host_path
            .as_ref()
            .unwrap()
            .contains("256x256"));
    }
}
