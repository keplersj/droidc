//! Finding and reading a unit, as `systemd.unit(5)` describes:
//!
//! - The **fragment** is the first file named after the unit on the search path. An instance with no
//!   file of its own uses its template's (`foo@bar.service` falls back to `foo@.service`).
//! - A fragment that is a symlink to `/dev/null`, or an empty file, **masks** the unit.
//! - A symlink to a file with another name is an **alias**: the unit is loaded under the target's name,
//!   and the link's name is one of its names.
//! - **Drop-ins** are `*.conf` files in `<name>.d/` directories, for every name of the unit, its
//!   template, its dash prefixes (`foo-.service.d/` applies to `foo-bar.service`) and its type
//!   (`service.d/`). They apply in file-name order whichever directory they are in. Of two with the
//!   same file name, the one in the higher-priority directory wins (and, in the same directory, the
//!   one for the more specific name). A drop-in linked to `/dev/null` masks lower ones of that name.
//! - `<name>.wants/`, `.requires/` and `.upholds/` directories add dependencies.
//!
//! Not done here yet: aliases found only by scanning other directories (systemd learns that `gdm.service`
//! is also `display-manager.service` from the symlink, even when asked for `gdm.service`), and
//! `[Install] Alias=`, which only matters for enabling.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::name::{NameError, UnitName};
use crate::paths::SearchPath;
use crate::syntax::{self, SyntaxWarning};

/// The load state `systemctl status` shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    Loaded,
    NotFound,
    Masked,
    /// The fragment exists but couldn't be read.
    Error(String),
}

impl LoadState {
    /// The string systemd uses for `LoadState`.
    pub fn as_str(&self) -> &'static str {
        match self {
            LoadState::Loaded => "loaded",
            LoadState::NotFound => "not-found",
            LoadState::Masked => "masked",
            LoadState::Error(_) => "error",
        }
    }
}

/// One `Key=Value` from the fragment or a drop-in, with where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub section: String,
    pub key: String,
    pub value: String,
    pub file: PathBuf,
    pub line: usize,
}

/// Something wrong in a file the unit was built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileWarning {
    pub file: PathBuf,
    pub message: String,
}

impl FileWarning {
    fn syntax(file: &Path, w: &SyntaxWarning) -> FileWarning {
        FileWarning {
            file: file.to_path_buf(),
            message: w.to_string(),
        }
    }
}

/// A unit as read from disk, before anything is run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedUnit {
    /// The name the unit is known by: the alias target if the requested name was an alias.
    pub id: UnitName,
    /// Every name of the unit, `id` first.
    pub names: Vec<UnitName>,
    pub state: LoadState,
    /// The file the unit was read from, with symlinks resolved.
    pub fragment: Option<PathBuf>,
    pub dropins: Vec<PathBuf>,
    /// Fragment first, then each drop-in, each in file order.
    pub assignments: Vec<Assignment>,
    /// Section headers seen, in order, without repeats.
    pub sections: Vec<String>,
    /// From `.wants/` directories (not from `Wants=`, which is in `assignments`).
    pub wants_dir: Vec<UnitName>,
    pub requires_dir: Vec<UnitName>,
    pub upholds_dir: Vec<UnitName>,
    pub warnings: Vec<FileWarning>,
}

impl LoadedUnit {
    fn empty(name: &UnitName, state: LoadState) -> LoadedUnit {
        LoadedUnit {
            id: name.clone(),
            names: vec![name.clone()],
            state,
            fragment: None,
            dropins: Vec::new(),
            assignments: Vec::new(),
            sections: Vec::new(),
            wants_dir: Vec::new(),
            requires_dir: Vec::new(),
            upholds_dir: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// The assignments to `section`/`key` that still count: for list settings, an empty assignment
    /// clears everything before it (`systemd.unit(5)`), so only what follows the last empty one is kept.
    pub fn list(&self, section: &str, key: &str) -> Vec<&Assignment> {
        let all: Vec<&Assignment> = self
            .assignments
            .iter()
            .filter(|a| a.section == section && a.key == key)
            .collect();
        match all.iter().rposition(|a| a.value.is_empty()) {
            Some(i) => all[i + 1..].to_vec(),
            None => all,
        }
    }

    /// The words of a list setting, after resets: `Wants=a b` and `Wants=c` give `[a, b, c]`.
    /// Assignments with an unterminated quote are skipped (systemd rejects them).
    pub fn words(&self, section: &str, key: &str) -> Vec<String> {
        self.list(section, key)
            .into_iter()
            .filter_map(|a| syntax::split_words(&a.value))
            .flatten()
            .collect()
    }

    /// The value of a single-valued setting: the last assignment wins, and an empty one means the
    /// default (`None`).
    pub fn value(&self, section: &str, key: &str) -> Option<&str> {
        self.assignments
            .iter()
            .rev()
            .find(|a| a.section == section && a.key == key)
            .map(|a| a.value.as_str())
            .filter(|v| !v.is_empty())
    }

    /// A boolean setting; `None` if unset or not a boolean.
    pub fn bool(&self, section: &str, key: &str) -> Option<bool> {
        self.value(section, key).and_then(syntax::parse_bool)
    }
}

/// Reads units from a [`SearchPath`].
#[derive(Clone, Debug)]
pub struct Loader {
    path: SearchPath,
}

impl Loader {
    pub fn new(path: SearchPath) -> Loader {
        Loader { path }
    }

    pub fn search_path(&self) -> &SearchPath {
        &self.path
    }

    /// Loads a unit by name; a bad name is the only error.
    pub fn load_str(&self, name: &str) -> Result<LoadedUnit, NameError> {
        Ok(self.load(&UnitName::parse(name)?))
    }

    pub fn load(&self, name: &UnitName) -> LoadedUnit {
        let found = match self.find_fragment(name) {
            Found::Fragment { id, file } => (id, file),
            Found::Masked => return self.finish(LoadedUnit::empty(name, LoadState::Masked), name),
            Found::Nothing => {
                return self.finish(LoadedUnit::empty(name, LoadState::NotFound), name)
            }
            Found::Unreadable(e) => {
                return self.finish(LoadedUnit::empty(name, LoadState::Error(e)), name)
            }
        };
        let (id, file) = found;
        let mut unit = LoadedUnit::empty(&id, LoadState::Loaded);
        if &id != name {
            unit.names.push(name.clone());
        }
        match fs::read_to_string(&file) {
            Ok(text) => add_file(&mut unit, &file, &text),
            Err(e) => unit.state = LoadState::Error(format!("{}: {e}", file.display())),
        }
        unit.fragment = Some(file);
        self.finish(unit, name)
    }

    /// Adds drop-ins and dependency directories. Done for masked and missing units too: systemd still
    /// reports their drop-ins, and `.wants/` of a missing unit is harmless.
    fn finish(&self, mut unit: LoadedUnit, requested: &UnitName) -> LoadedUnit {
        let names = unit.names.clone();
        if unit.state == LoadState::Loaded {
            for file in self.dropins(&names) {
                match fs::read_to_string(&file) {
                    Ok(text) => add_file(&mut unit, &file, &text),
                    Err(e) => unit.warnings.push(FileWarning {
                        file: file.clone(),
                        message: e.to_string(),
                    }),
                }
                unit.dropins.push(file);
            }
        }
        let mut all_names = names.clone();
        if let Some(t) = requested.template() {
            all_names.push(t);
        }
        let instance = requested
            .is_instance()
            .then(|| requested.instance().to_string());
        unit.wants_dir = self.dep_dir(&all_names, "wants", instance.as_deref(), &mut unit.warnings);
        unit.requires_dir = self.dep_dir(
            &all_names,
            "requires",
            instance.as_deref(),
            &mut unit.warnings,
        );
        unit.upholds_dir = self.dep_dir(
            &all_names,
            "upholds",
            instance.as_deref(),
            &mut unit.warnings,
        );
        unit
    }

    fn find_fragment(&self, name: &UnitName) -> Found {
        let mut candidates = vec![name.clone()];
        candidates.extend(name.template());
        for candidate in &candidates {
            for dir in self.path.dirs() {
                let path = dir.join(candidate.to_string());
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let resolved = match fs::canonicalize(&path) {
                    Ok(r) => r,
                    // A dangling link names nothing; keep looking, as for a missing file.
                    Err(_) => continue,
                };
                if resolved == Path::new("/dev/null") {
                    return Found::Masked;
                }
                let target_meta = match fs::metadata(&resolved) {
                    Ok(m) => m,
                    Err(e) => return Found::Unreadable(format!("{}: {e}", path.display())),
                };
                if !target_meta.is_file() {
                    continue;
                }
                if target_meta.len() == 0 {
                    return Found::Masked;
                }
                let mut id = name.clone();
                if meta.file_type().is_symlink() {
                    if let Some(alias) = alias_target(&resolved, name, candidate) {
                        id = alias;
                    }
                }
                return Found::Fragment { id, file: resolved };
            }
        }
        Found::Nothing
    }

    /// Drop-in files for `names`, in the order they apply.
    pub fn dropins(&self, names: &[UnitName]) -> Vec<PathBuf> {
        // Drop-in directory names, most specific first.
        let mut dir_names: Vec<String> = Vec::new();
        let mut add = |s: String| {
            if !dir_names.contains(&s) {
                dir_names.push(s);
            }
        };
        for n in names {
            add(format!("{n}.d"));
        }
        for n in names {
            if let Some(t) = n.template() {
                add(format!("{t}.d"));
            }
        }
        for n in names {
            for p in n.dash_prefixes() {
                add(format!("{p}.d"));
            }
        }
        if let Some(n) = names.first() {
            add(format!("{}.d", n.ty().suffix()));
        }

        // file name -> (dir rank, specificity, path)
        let mut best: BTreeMap<String, (usize, usize, PathBuf)> = BTreeMap::new();
        for (rank, dir) in self.path.dirs().iter().enumerate() {
            for (spec, dn) in dir_names.iter().enumerate() {
                let Ok(rd) = fs::read_dir(dir.join(dn)) else {
                    continue;
                };
                for entry in rd.flatten() {
                    let file_name = entry.file_name().to_string_lossy().into_owned();
                    if !file_name.ends_with(".conf") {
                        continue;
                    }
                    let key = (rank, spec);
                    match best.get(&file_name) {
                        Some((r, s, _)) if (*r, *s) <= key => {}
                        _ => {
                            best.insert(file_name, (rank, spec, entry.path()));
                        }
                    }
                }
            }
        }
        best.into_values()
            .filter(|(_, _, p)| !is_masked_file(p))
            .map(|(_, _, p)| p)
            .collect()
    }

    fn dep_dir(
        &self,
        names: &[UnitName],
        kind: &str,
        instance: Option<&str>,
        warnings: &mut Vec<FileWarning>,
    ) -> Vec<UnitName> {
        let mut out: Vec<UnitName> = Vec::new();
        for dir in self.path.dirs() {
            for n in names {
                let d = dir.join(format!("{n}.{kind}"));
                let Ok(rd) = fs::read_dir(&d) else { continue };
                let mut entries: Vec<String> = rd
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                entries.sort();
                for e in entries {
                    let dep = match UnitName::parse(&e) {
                        Ok(dep) => dep,
                        Err(err) => {
                            warnings.push(FileWarning {
                                file: d.join(&e),
                                message: err.to_string(),
                            });
                            continue;
                        }
                    };
                    let dep = if dep.is_template() {
                        match instance.and_then(|i| dep.instantiate(i)) {
                            Some(d) => d,
                            None => {
                                warnings.push(FileWarning {
                                    file: d.join(&e),
                                    message: "template with no instance to fill it, ignoring"
                                        .into(),
                                });
                                continue;
                            }
                        }
                    } else {
                        dep
                    };
                    if !out.contains(&dep) {
                        out.push(dep);
                    }
                }
            }
        }
        out
    }

    /// Every unit file name on the search path, masked ones included. For `ListUnitFiles`.
    pub fn unit_file_names(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for dir in self.path.dirs() {
            let Ok(rd) = fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                let is_file_or_link = e
                    .file_type()
                    .map(|t| t.is_file() || t.is_symlink())
                    .unwrap_or(false);
                if is_file_or_link && UnitName::parse(&n).is_ok() {
                    out.insert(n);
                }
            }
        }
        out
    }
}

enum Found {
    Fragment { id: UnitName, file: PathBuf },
    Masked,
    Nothing,
    Unreadable(String),
}

/// If `resolved` (the target of the link found for `candidate`) has another valid unit name of the
/// same type, the unit is that name. An instance that resolved through its template's link to another
/// template keeps its instance.
fn alias_target(resolved: &Path, requested: &UnitName, candidate: &UnitName) -> Option<UnitName> {
    let target = UnitName::parse(&resolved.file_name()?.to_string_lossy()).ok()?;
    if target.ty() != requested.ty() || &target == candidate {
        return None;
    }
    if requested.is_instance() && target.is_template() {
        return target.instantiate(requested.instance());
    }
    // A link from an instance or plain name to a template has nothing to fill it with.
    if target.is_template() {
        return None;
    }
    Some(target)
}

fn is_masked_file(p: &Path) -> bool {
    match fs::canonicalize(p) {
        Ok(r) if r == Path::new("/dev/null") => true,
        Ok(r) => fs::metadata(r).map(|m| m.len() == 0).unwrap_or(false),
        Err(_) => true,
    }
}

fn add_file(unit: &mut LoadedUnit, file: &Path, text: &str) {
    let parsed = syntax::parse(text);
    for w in &parsed.warnings {
        unit.warnings.push(FileWarning::syntax(file, w));
    }
    for s in parsed.sections {
        if !unit.sections.contains(&s) {
            unit.sections.push(s);
        }
    }
    unit.assignments
        .extend(parsed.entries.into_iter().map(|e| Assignment {
            section: e.section,
            key: e.key,
            value: e.value,
            file: file.to_path_buf(),
            line: e.line,
        }));
}

#[cfg(test)]
pub(crate) mod testdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory under the system temp dir, removed on drop.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> TempDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "droidc-systemd-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn write(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            p
        }

        pub fn link(&self, rel: &str, target: impl AsRef<Path>) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, &p).unwrap();
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdir::TempDir;
    use super::*;

    /// `etc` above `run` above `usr`, like the real path.
    fn setup() -> (TempDir, Loader) {
        let t = TempDir::new();
        for d in ["etc", "run", "usr"] {
            fs::create_dir_all(t.path().join(d)).unwrap();
        }
        let loader = Loader::new(SearchPath::from_dirs(
            ["etc", "run", "usr"].map(|d| t.path().join(d)),
        ));
        (t, loader)
    }

    fn values(u: &LoadedUnit, s: &str, k: &str) -> Vec<String> {
        u.list(s, k).into_iter().map(|a| a.value.clone()).collect()
    }

    #[test]
    fn first_fragment_wins() {
        let (t, l) = setup();
        t.write("usr/a.service", "[Unit]\nDescription=vendor\n");
        t.write("etc/a.service", "[Unit]\nDescription=admin\n");
        let u = l.load_str("a.service").unwrap();
        assert_eq!(u.state, LoadState::Loaded);
        assert_eq!(u.value("Unit", "Description"), Some("admin"));
        assert_eq!(
            u.fragment.as_deref(),
            Some(
                fs::canonicalize(t.path().join("etc/a.service"))
                    .unwrap()
                    .as_path()
            )
        );
        assert_eq!(u.state.as_str(), "loaded");
    }

    #[test]
    fn not_found_and_bad_name() {
        let (_t, l) = setup();
        let u = l.load_str("nope.service").unwrap();
        assert_eq!(u.state, LoadState::NotFound);
        assert_eq!(u.state.as_str(), "not-found");
        assert!(l.load_str("nope").is_err());
    }

    #[test]
    fn masks() {
        let (t, l) = setup();
        t.write("usr/a.service", "[Unit]\nDescription=x\n");
        t.link("etc/a.service", "/dev/null");
        assert_eq!(l.load_str("a.service").unwrap().state, LoadState::Masked);
        t.write("usr/b.service", "");
        assert_eq!(l.load_str("b.service").unwrap().state, LoadState::Masked);
        assert_eq!(LoadState::Masked.as_str(), "masked");
    }

    #[test]
    fn dangling_link_is_skipped() {
        let (t, l) = setup();
        t.link("etc/a.service", t.path().join("missing/a.service"));
        t.write("usr/a.service", "[Unit]\nDescription=vendor\n");
        assert_eq!(
            l.load_str("a.service")
                .unwrap()
                .value("Unit", "Description"),
            Some("vendor")
        );
    }

    #[test]
    fn templates_and_instances() {
        let (t, l) = setup();
        t.write(
            "usr/getty@.service",
            "[Service]\nExecStart=/sbin/agetty %I\n",
        );
        let u = l.load_str("getty@tty1.service").unwrap();
        assert_eq!(u.state, LoadState::Loaded);
        assert_eq!(u.id.to_string(), "getty@tty1.service");
        assert_eq!(u.value("Service", "ExecStart"), Some("/sbin/agetty %I"));

        // An instance's own file beats the template's.
        t.write("usr/getty@tty2.service", "[Service]\nExecStart=/special\n");
        assert_eq!(
            l.load_str("getty@tty2.service")
                .unwrap()
                .value("Service", "ExecStart"),
            Some("/special")
        );

        // A masked template masks its instances.
        t.link("etc/getty@.service", "/dev/null");
        assert_eq!(
            l.load_str("getty@tty1.service").unwrap().state,
            LoadState::Masked
        );
    }

    #[test]
    fn aliases() {
        let (t, l) = setup();
        let gdm = t.write("usr/gdm.service", "[Unit]\nDescription=GDM\n");
        t.link("etc/display-manager.service", &gdm);
        let u = l.load_str("display-manager.service").unwrap();
        assert_eq!(u.id.to_string(), "gdm.service");
        let names: Vec<String> = u.names.iter().map(|n| n.to_string()).collect();
        assert_eq!(names, vec!["gdm.service", "display-manager.service"]);

        // Drop-ins for either name apply.
        t.write(
            "etc/display-manager.service.d/a.conf",
            "[Unit]\nDescription=by alias\n",
        );
        t.write("run/gdm.service.d/b.conf", "[Unit]\nAfter=x.target\n");
        let u = l.load_str("display-manager.service").unwrap();
        assert_eq!(u.dropins.len(), 2);
        assert_eq!(u.value("Unit", "Description"), Some("by alias"));

        // A link to a template, through an instance: the instance carries over.
        let tmpl = t.write("usr/real@.service", "[Unit]\nDescription=real %i\n");
        t.link("usr/other@.service", &tmpl);
        assert_eq!(
            l.load_str("other@x.service").unwrap().id.to_string(),
            "real@x.service"
        );

        // A link to another type isn't an alias.
        let sock = t.write("usr/s.socket", "[Socket]\nListenStream=/x\n");
        t.link("usr/s2.service", &sock);
        assert_eq!(
            l.load_str("s2.service").unwrap().id.to_string(),
            "s2.service"
        );
    }

    #[test]
    fn dropin_order_and_override() {
        let (t, l) = setup();
        t.write("usr/a-b-c.service", "[Unit]\nAfter=one.target\n");
        t.write(
            "usr/a-b-c.service.d/20-vendor.conf",
            "[Unit]\nAfter=two.target\n",
        );
        t.write(
            "usr/a-.service.d/10-prefix.conf",
            "[Unit]\nAfter=zero.target\n",
        );
        t.write("usr/service.d/30-all.conf", "[Unit]\nAfter=three.target\n");
        // Same name higher up overrides the vendor's.
        t.write(
            "etc/a-b-c.service.d/20-vendor.conf",
            "[Unit]\nAfter=two-admin.target\n",
        );
        // Same name for a less specific directory but higher up still wins.
        t.write(
            "run/a-b-.service.d/30-all.conf",
            "[Unit]\nAfter=three-run.target\n",
        );
        // Not a .conf file.
        t.write("etc/a-b-c.service.d/README", "[Unit]\nAfter=nope\n");

        let u = l.load_str("a-b-c.service").unwrap();
        let files: Vec<String> = u
            .dropins
            .iter()
            .map(|p| p.strip_prefix(t.path()).unwrap().display().to_string())
            .collect();
        assert_eq!(
            files,
            vec![
                "usr/a-.service.d/10-prefix.conf",
                "etc/a-b-c.service.d/20-vendor.conf",
                "run/a-b-.service.d/30-all.conf"
            ]
        );
        assert_eq!(
            values(&u, "Unit", "After"),
            vec![
                "one.target",
                "zero.target",
                "two-admin.target",
                "three-run.target"
            ]
        );
    }

    #[test]
    fn same_directory_more_specific_wins() {
        let (t, l) = setup();
        t.write("usr/a-b.service", "[Unit]\n");
        t.write("usr/a-.service.d/x.conf", "[Unit]\nDescription=prefix\n");
        t.write("usr/a-b.service.d/x.conf", "[Unit]\nDescription=unit\n");
        assert_eq!(
            l.load_str("a-b.service")
                .unwrap()
                .value("Unit", "Description"),
            Some("unit")
        );
    }

    #[test]
    fn template_dropins_apply_to_instances() {
        let (t, l) = setup();
        t.write("usr/u@.service", "[Service]\nExecStart=/x\n");
        t.write("usr/u@.service.d/a.conf", "[Service]\nEnvironment=A=1\n");
        t.write("etc/u@1.service.d/b.conf", "[Service]\nEnvironment=B=2\n");
        let u = l.load_str("u@1.service").unwrap();
        assert_eq!(u.words("Service", "Environment"), vec!["A=1", "B=2"]);
    }

    #[test]
    fn masked_dropin() {
        let (t, l) = setup();
        t.write("usr/a.service", "[Unit]\n");
        t.write("usr/a.service.d/x.conf", "[Unit]\nDescription=vendor\n");
        t.link("etc/a.service.d/x.conf", "/dev/null");
        let u = l.load_str("a.service").unwrap();
        assert!(u.dropins.is_empty());
        assert_eq!(u.value("Unit", "Description"), None);
    }

    #[test]
    fn list_resets_and_values() {
        let (t, l) = setup();
        t.write(
            "usr/a.service",
            "[Unit]\nAfter=x y\nDescription=old\n[Service]\nRemainAfterExit=yes\n",
        );
        t.write(
            "usr/a.service.d/r.conf",
            "[Unit]\nAfter=\nAfter=z\nDescription=\n",
        );
        let u = l.load_str("a.service").unwrap();
        assert_eq!(u.words("Unit", "After"), vec!["z"]);
        assert_eq!(u.value("Unit", "Description"), None);
        assert_eq!(u.bool("Service", "RemainAfterExit"), Some(true));
        assert_eq!(u.bool("Service", "Nope"), None);
        assert_eq!(u.sections, vec!["Unit", "Service"]);
    }

    #[test]
    fn wants_requires_upholds_dirs() {
        let (t, l) = setup();
        t.write("usr/multi-user.target", "[Unit]\nDescription=Multi-User\n");
        t.link("etc/multi-user.target.wants/sshd.service", "/whatever");
        t.link("usr/multi-user.target.wants/dbus.service", "/whatever");
        t.link("usr/multi-user.target.wants/sshd.service", "/whatever");
        t.write("usr/multi-user.target.requires/basic.target", "");
        t.write("usr/multi-user.target.upholds/up.service", "");
        t.write("usr/multi-user.target.wants/not a unit", "");
        t.write("usr/multi-user.target.wants/tmpl@.service", "");
        let u = l.load_str("multi-user.target").unwrap();
        let s = |v: &[UnitName]| v.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(s(&u.wants_dir), vec!["sshd.service", "dbus.service"]);
        assert_eq!(s(&u.requires_dir), vec!["basic.target"]);
        assert_eq!(s(&u.upholds_dir), vec!["up.service"]);
        assert_eq!(u.warnings.len(), 2, "{:?}", u.warnings);
    }

    #[test]
    fn template_wants_are_instantiated() {
        let (t, l) = setup();
        t.write("usr/user@.service", "[Unit]\n");
        t.write("usr/user@.service.wants/user-runtime-dir@.service", "");
        let u = l.load_str("user@1000.service").unwrap();
        assert_eq!(
            u.wants_dir
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>(),
            vec!["user-runtime-dir@1000.service"]
        );
    }

    #[test]
    fn syntax_warnings_carry_the_file() {
        let (t, l) = setup();
        let f = t.write("usr/a.service", "[Unit]\nbogus\n");
        let u = l.load_str("a.service").unwrap();
        assert_eq!(
            u.warnings,
            vec![FileWarning {
                file: fs::canonicalize(f).unwrap(),
                message: "line 2: missing '=', ignoring line".into()
            }]
        );
    }

    #[test]
    fn unit_file_names() {
        let (t, l) = setup();
        t.write("usr/a.service", "[Unit]\n");
        t.link("etc/b.service", "/dev/null");
        t.write("usr/a.service.d/x.conf", "");
        t.write("usr/README", "");
        let names: Vec<String> = l.unit_file_names().into_iter().collect();
        assert_eq!(names, vec!["a.service", "b.service"]);
    }

    #[test]
    fn unreadable_fragment() {
        let (t, l) = setup();
        // A directory named like a unit is not a fragment.
        fs::create_dir_all(t.path().join("etc/d.service")).unwrap();
        assert_eq!(l.load_str("d.service").unwrap().state, LoadState::NotFound);
    }
}
