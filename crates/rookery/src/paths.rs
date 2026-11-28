//! The unit search path, in the order `systemd.unit(5)` lists it (highest priority first), plus the
//! directories droidc's shim image ships its own units in.
//!
//! droidc's directories sit just above the image's vendor directory (`/usr/lib/systemd/...`), so the
//! shim image can add units and drop-ins that override the image's packaged ones, while the image's
//! `/etc` and anything written to `/run` still override droidc.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Which manager: PID 1 or a user's `user@UID.service`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

/// Where droidc's shim image puts its units (`docs/design/systemd-standin.md`, "The shim image").
pub const DROIDC_SYSTEM_UNITS: &str = "/usr/lib/droidc/units/system";
pub const DROIDC_USER_UNITS: &str = "/usr/lib/droidc/units/user";

/// The environment the user search path is built from. All optional: the XDG defaults apply, as the
/// XDG Base Directory specification says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserDirs {
    pub home: Option<PathBuf>,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_config_dirs: Option<String>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_data_dirs: Option<String>,
}

impl UserDirs {
    /// Reads `HOME` and the `XDG_*` variables. Empty values count as unset, as the spec says.
    pub fn from_env() -> UserDirs {
        UserDirs::from_vars(|k| std::env::var_os(k))
    }

    /// As [`UserDirs::from_env`], with the variables looked up by `var`.
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> UserDirs {
        let path = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let list = |k: &str| {
            var(k)
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string_lossy().into_owned())
        };
        UserDirs {
            home: path("HOME"),
            xdg_runtime_dir: path("XDG_RUNTIME_DIR"),
            xdg_config_home: path("XDG_CONFIG_HOME"),
            xdg_config_dirs: list("XDG_CONFIG_DIRS"),
            xdg_data_home: path("XDG_DATA_HOME"),
            xdg_data_dirs: list("XDG_DATA_DIRS"),
        }
    }

    fn config_home(&self) -> Option<PathBuf> {
        self.xdg_config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|h| h.join(".config")))
    }

    fn data_home(&self) -> Option<PathBuf> {
        self.xdg_data_home
            .clone()
            .or_else(|| self.home.as_ref().map(|h| h.join(".local/share")))
    }

    fn config_dirs(&self) -> Vec<PathBuf> {
        split_dirs(self.xdg_config_dirs.as_deref().unwrap_or("/etc/xdg"))
    }

    fn data_dirs(&self) -> Vec<PathBuf> {
        split_dirs(
            self.xdg_data_dirs
                .as_deref()
                .unwrap_or("/usr/local/share:/usr/share"),
        )
    }
}

fn split_dirs(list: &str) -> Vec<PathBuf> {
    list.split(':')
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

/// An ordered list of unit directories, highest priority first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPath {
    dirs: Vec<PathBuf>,
}

impl SearchPath {
    /// Exactly these directories, in this order. For tests and for callers that build their own.
    pub fn from_dirs<I, P>(dirs: I) -> SearchPath
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let mut out = SearchPath { dirs: Vec::new() };
        for d in dirs {
            out.push(d.into());
        }
        out
    }

    /// The default path for `scope`, with droidc's directory included.
    ///
    /// `unit_path` is `$SYSTEMD_UNIT_PATH`: when set it replaces the default, and a trailing `:` (an
    /// empty last element) appends the default after it (`systemd.unit(5)`).
    pub fn new(scope: Scope, user: &UserDirs, unit_path: Option<&str>) -> SearchPath {
        let defaults = match scope {
            Scope::System => system_defaults(),
            Scope::User => user_defaults(user),
        };
        let dirs = match unit_path {
            None => defaults,
            Some(spec) => {
                let mut parts: Vec<&str> = spec.split(':').collect();
                let append = parts.last() == Some(&"");
                if append {
                    parts.pop();
                }
                let mut dirs: Vec<PathBuf> = parts
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
                    .collect();
                if append {
                    dirs.extend(defaults);
                }
                dirs
            }
        };
        SearchPath::from_dirs(dirs)
    }

    fn push(&mut self, dir: PathBuf) {
        if !self.dirs.contains(&dir) {
            self.dirs.push(dir);
        }
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The priority of `dir` (0 is highest), or `None` if it isn't on the path.
    pub fn rank(&self, dir: &Path) -> Option<usize> {
        self.dirs.iter().position(|d| d == dir)
    }
}

fn system_defaults() -> Vec<PathBuf> {
    [
        "/etc/systemd/system.control",
        "/run/systemd/system.control",
        "/run/systemd/transient",
        "/run/systemd/generator.early",
        "/etc/systemd/system",
        "/etc/systemd/system.attached",
        "/run/systemd/system",
        "/run/systemd/system.attached",
        "/run/systemd/generator",
        "/usr/local/lib/systemd/system",
        DROIDC_SYSTEM_UNITS,
        "/usr/lib/systemd/system",
        "/run/systemd/generator.late",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

fn user_defaults(user: &UserDirs) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let config = user.config_home();
    let runtime = user.xdg_runtime_dir.clone();
    let rt = |sub: &str| runtime.as_ref().map(|r| r.join("systemd").join(sub));

    v.extend(config.as_ref().map(|c| c.join("systemd/user.control")));
    v.extend(rt("user.control"));
    v.extend(rt("transient"));
    v.extend(rt("generator.early"));
    v.extend(config.as_ref().map(|c| c.join("systemd/user")));
    v.extend(
        user.config_dirs()
            .into_iter()
            .map(|d| d.join("systemd/user")),
    );
    v.push(PathBuf::from("/etc/systemd/user"));
    v.extend(rt("user"));
    v.push(PathBuf::from("/run/systemd/user"));
    v.extend(rt("generator"));
    v.extend(user.data_home().map(|d| d.join("systemd/user")));
    v.extend(user.data_dirs().into_iter().map(|d| d.join("systemd/user")));
    v.push(PathBuf::from("/usr/local/lib/systemd/user"));
    v.push(PathBuf::from(DROIDC_USER_UNITS));
    v.push(PathBuf::from("/usr/lib/systemd/user"));
    v.extend(rt("generator.late"));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(p: &SearchPath) -> Vec<String> {
        p.dirs().iter().map(|d| d.display().to_string()).collect()
    }

    #[test]
    fn system_order() {
        let p = SearchPath::new(Scope::System, &UserDirs::default(), None);
        let s = strs(&p);
        let pos = |d: &str| s.iter().position(|x| x == d).unwrap();
        assert!(pos("/etc/systemd/system") < pos("/run/systemd/system"));
        assert!(pos("/run/systemd/system") < pos(DROIDC_SYSTEM_UNITS));
        assert_eq!(pos(DROIDC_SYSTEM_UNITS) + 1, pos("/usr/lib/systemd/system"));
        assert_eq!(s.last().unwrap(), "/run/systemd/generator.late");
        assert_eq!(p.rank(Path::new("/etc/systemd/system.control")), Some(0));
        assert_eq!(p.rank(Path::new("/nope")), None);
    }

    #[test]
    fn user_order_with_xdg_defaults() {
        let dirs = UserDirs {
            home: Some("/home/k".into()),
            xdg_runtime_dir: Some("/run/user/1000".into()),
            ..Default::default()
        };
        let s = strs(&SearchPath::new(Scope::User, &dirs, None));
        assert_eq!(s[0], "/home/k/.config/systemd/user.control");
        assert_eq!(s[1], "/run/user/1000/systemd/user.control");
        let pos = |d: &str| {
            s.iter()
                .position(|x| x == d)
                .unwrap_or_else(|| panic!("{d} missing in {s:?}"))
        };
        assert!(pos("/home/k/.config/systemd/user") < pos("/etc/xdg/systemd/user"));
        assert!(pos("/etc/systemd/user") < pos("/run/user/1000/systemd/user"));
        assert!(pos("/home/k/.local/share/systemd/user") < pos("/usr/share/systemd/user"));
        assert!(pos("/usr/local/share/systemd/user") < pos("/usr/share/systemd/user"));
        assert_eq!(pos(DROIDC_USER_UNITS) + 1, pos("/usr/lib/systemd/user"));
        assert_eq!(s.last().unwrap(), "/run/user/1000/systemd/generator.late");
    }

    #[test]
    fn user_without_runtime_dir_or_home() {
        let s = strs(&SearchPath::new(Scope::User, &UserDirs::default(), None));
        assert_eq!(s[0], "/etc/xdg/systemd/user");
        assert!(s.iter().all(|d| d.starts_with('/')));
    }

    #[test]
    fn explicit_xdg_lists() {
        let dirs = UserDirs {
            xdg_config_home: Some("/c".into()),
            xdg_data_home: Some("/d".into()),
            xdg_config_dirs: Some("/x1:relative:/x2".into()),
            xdg_data_dirs: Some("/y".into()),
            ..Default::default()
        };
        let s = strs(&SearchPath::new(Scope::User, &dirs, None));
        assert!(s.contains(&"/c/systemd/user".to_string()));
        assert!(s.contains(&"/d/systemd/user".to_string()));
        assert!(s.contains(&"/x2/systemd/user".to_string()));
        assert!(!s.iter().any(|d| d.contains("relative")));
        assert!(s.contains(&"/y/systemd/user".to_string()));
        assert!(!s.contains(&"/usr/share/systemd/user".to_string()));
    }

    #[test]
    fn dirs_from_variables() {
        let vars = |k: &str| -> Option<OsString> {
            match k {
                "HOME" => Some("/home/k".into()),
                "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
                "XDG_CONFIG_DIRS" => Some("/x".into()),
                // Empty counts as unset.
                "XDG_CONFIG_HOME" | "XDG_DATA_DIRS" => Some("".into()),
                _ => None,
            }
        };
        let d = UserDirs::from_vars(vars);
        assert_eq!(
            d,
            UserDirs {
                home: Some("/home/k".into()),
                xdg_runtime_dir: Some("/run/user/1000".into()),
                xdg_config_dirs: Some("/x".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            UserDirs::from_env(),
            UserDirs::from_vars(|k| std::env::var_os(k))
        );
    }

    #[test]
    fn unit_path_override() {
        let only = SearchPath::new(Scope::System, &UserDirs::default(), Some("/a:/b"));
        assert_eq!(strs(&only), vec!["/a", "/b"]);
        let append = SearchPath::new(Scope::System, &UserDirs::default(), Some("/a:"));
        let s = strs(&append);
        assert_eq!(s[0], "/a");
        assert_eq!(s[1], "/etc/systemd/system.control");
        let dup = SearchPath::from_dirs(["/a", "/a", "/b"]);
        assert_eq!(strs(&dup), vec!["/a", "/b"]);
    }
}
