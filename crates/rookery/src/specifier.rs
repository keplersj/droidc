//! Specifiers: the `%x` sequences `systemd.unit(5)` lists under "Specifiers".
//!
//! [`expand`] replaces each one from a [`SpecContext`]. A specifier whose fact the context doesn't have,
//! or one this table doesn't know, is an error, so a unit never runs with a half-expanded command line.

use std::fmt;
use std::path::PathBuf;

use crate::name::{unescape, unescape_path, UnitName};
use crate::paths::Scope;

/// What specifiers expand to. Unit facts come from the unit's name; the rest is filled in by the
/// manager from the guest's view of itself.
#[derive(Clone, Debug)]
pub struct SpecContext {
    pub unit: UnitName,
    pub scope: Scope,
    /// The unit's fragment, for `%y`/`%Y`.
    pub fragment: Option<PathBuf>,
    pub user_name: Option<String>,
    pub uid: Option<u32>,
    pub group_name: Option<String>,
    pub gid: Option<u32>,
    pub home: Option<PathBuf>,
    pub shell: Option<String>,
    /// `$XDG_RUNTIME_DIR`, `$XDG_STATE_HOME`, `$XDG_CACHE_HOME`, `$XDG_CONFIG_HOME`, for the user
    /// manager's `%t %S %C %L %E`. When unset, the XDG defaults under `home` apply.
    pub xdg_runtime_dir: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub hostname: Option<String>,
    pub machine_id: Option<String>,
    pub boot_id: Option<String>,
    pub kernel_release: Option<String>,
    pub architecture: Option<String>,
    /// From `/etc/os-release`: `ID`, `VERSION_ID`, `BUILD_ID`, `IMAGE_ID`, `IMAGE_VERSION`.
    pub os_id: Option<String>,
    pub os_version_id: Option<String>,
    pub os_build_id: Option<String>,
    pub os_image_id: Option<String>,
    pub os_image_version: Option<String>,
}

impl SpecContext {
    /// A context with only the unit's own facts; the rest is `None`.
    pub fn for_unit(unit: UnitName, scope: Scope) -> SpecContext {
        SpecContext {
            unit,
            scope,
            fragment: None,
            user_name: None,
            uid: None,
            group_name: None,
            gid: None,
            home: None,
            shell: None,
            xdg_runtime_dir: None,
            xdg_state_home: None,
            xdg_cache_home: None,
            xdg_config_home: None,
            hostname: None,
            machine_id: None,
            boot_id: None,
            kernel_release: None,
            architecture: None,
            os_id: None,
            os_version_id: None,
            os_build_id: None,
            os_image_id: None,
            os_image_version: None,
        }
    }

    fn user_dir(&self, explicit: &Option<PathBuf>, under_home: &str) -> Option<String> {
        explicit
            .clone()
            .or_else(|| self.home.as_ref().map(|h| h.join(under_home)))
            .map(|p| p.display().to_string())
    }

    fn lookup(&self, c: char) -> Result<String, SpecError> {
        let missing = || SpecError::Missing(c);
        let path = |p: &Option<PathBuf>| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .ok_or_else(missing)
        };
        let text = |s: &Option<String>| s.clone().ok_or_else(missing);
        let system = self.scope == Scope::System;
        let u = &self.unit;
        Ok(match c {
            '%' => "%".into(),
            'n' => u.to_string(),
            'N' => u.stem(),
            'p' => u.prefix().to_string(),
            'P' => unescape(u.prefix()),
            'i' => u.instance().to_string(),
            'I' => unescape(u.instance()),
            'j' => last_dash_component(u.prefix()).to_string(),
            'J' => unescape(last_dash_component(u.prefix())),
            'f' => {
                let src = if u.is_instance() {
                    u.instance()
                } else {
                    u.prefix()
                };
                unescape_path(src)
            }
            'y' => path(&self.fragment)?,
            'Y' => self
                .fragment
                .as_ref()
                .and_then(|f| f.parent())
                .map(|d| d.display().to_string())
                .ok_or_else(missing)?,
            'u' => text(&self.user_name)?,
            'U' => self.uid.map(|v| v.to_string()).ok_or_else(missing)?,
            'g' => text(&self.group_name)?,
            'G' => self.gid.map(|v| v.to_string()).ok_or_else(missing)?,
            'h' => path(&self.home)?,
            's' => text(&self.shell)?,
            't' if system => "/run".into(),
            't' => path(&self.xdg_runtime_dir)?,
            'S' if system => "/var/lib".into(),
            'S' => self
                .user_dir(&self.xdg_state_home, ".local/state")
                .ok_or_else(missing)?,
            'C' if system => "/var/cache".into(),
            'C' => self
                .user_dir(&self.xdg_cache_home, ".cache")
                .ok_or_else(missing)?,
            'L' if system => "/var/log".into(),
            'L' => self
                .user_dir(&self.xdg_state_home, ".local/state")
                .map(|s| format!("{s}/log"))
                .ok_or_else(missing)?,
            'E' if system => "/etc".into(),
            'E' => self
                .user_dir(&self.xdg_config_home, ".config")
                .ok_or_else(missing)?,
            'T' => tmp_dir(std::env::var("TMPDIR").ok()),
            'V' => "/var/tmp".into(),
            'H' => text(&self.hostname)?,
            'l' => self
                .hostname
                .as_deref()
                .map(|h| h.split('.').next().unwrap_or(h).to_string())
                .ok_or_else(missing)?,
            'm' => text(&self.machine_id)?,
            'b' => text(&self.boot_id)?,
            'v' => text(&self.kernel_release)?,
            'a' => text(&self.architecture)?,
            'o' => text(&self.os_id)?,
            'w' => text(&self.os_version_id)?,
            'B' => text(&self.os_build_id)?,
            'M' => text(&self.os_image_id)?,
            'A' => text(&self.os_image_version)?,
            other => return Err(SpecError::Unknown(other)),
        })
    }
}

/// `%T`: `$TMPDIR`, or `/tmp` when it is unset or empty.
fn tmp_dir(var: Option<String>) -> String {
    var.filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/tmp".into())
}

/// `%j`: the part of the prefix after its last dash (the whole prefix without one).
fn last_dash_component(prefix: &str) -> &str {
    prefix.rsplit('-').next().unwrap_or(prefix)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecError {
    /// `%x` that isn't a specifier.
    Unknown(char),
    /// A specifier the context has no value for.
    Missing(char),
    /// A lone `%` at the end.
    Trailing,
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::Unknown(c) => write!(f, "unknown specifier %{c}"),
            SpecError::Missing(c) => write!(f, "no value for specifier %{c}"),
            SpecError::Trailing => write!(f, "'%' at the end of the value"),
        }
    }
}

impl std::error::Error for SpecError {}

/// Expands every specifier in `value`.
pub fn expand(value: &str, ctx: &SpecContext) -> Result<String, SpecError> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = chars.next().ok_or(SpecError::Trailing)?;
        out.push_str(&ctx.lookup(spec)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(name: &str, scope: Scope) -> SpecContext {
        SpecContext::for_unit(UnitName::parse(name).unwrap(), scope)
    }

    #[test]
    fn unit_name_specifiers() {
        let c = ctx(
            "systemd-fsck@dev-disk-by\\x2dlabel-root.service",
            Scope::System,
        );
        assert_eq!(
            expand("%n", &c).unwrap(),
            "systemd-fsck@dev-disk-by\\x2dlabel-root.service"
        );
        assert_eq!(
            expand("%N", &c).unwrap(),
            "systemd-fsck@dev-disk-by\\x2dlabel-root"
        );
        assert_eq!(expand("%p", &c).unwrap(), "systemd-fsck");
        assert_eq!(expand("%i", &c).unwrap(), "dev-disk-by\\x2dlabel-root");
        assert_eq!(expand("%I", &c).unwrap(), "dev/disk/by-label/root");
        assert_eq!(expand("%f", &c).unwrap(), "/dev/disk/by-label/root");
        assert_eq!(expand("%j", &c).unwrap(), "fsck");
        assert_eq!(expand("100%%", &c).unwrap(), "100%");
    }

    #[test]
    fn plain_unit_f_uses_prefix() {
        let c = ctx("home-k.mount", Scope::System);
        assert_eq!(expand("%f", &c).unwrap(), "/home/k");
        assert_eq!(expand("%i", &c).unwrap(), "");
    }

    #[test]
    fn roots_differ_by_scope() {
        let s = ctx("a.service", Scope::System);
        assert_eq!(
            expand("%t %S %C %L %E %V", &s).unwrap(),
            "/run /var/lib /var/cache /var/log /etc /var/tmp"
        );

        let mut u = ctx("a.service", Scope::User);
        assert_eq!(expand("%t", &u), Err(SpecError::Missing('t')));
        u.home = Some("/home/k".into());
        u.xdg_runtime_dir = Some("/run/user/1000".into());
        assert_eq!(
            expand("%t %S %C %L %E", &u).unwrap(),
            "/run/user/1000 /home/k/.local/state /home/k/.cache /home/k/.local/state/log /home/k/.config"
        );
        u.xdg_config_home = Some("/cfg".into());
        assert_eq!(expand("%E", &u).unwrap(), "/cfg");
    }

    #[test]
    fn host_and_user_facts() {
        let mut c = ctx("a.service", Scope::System);
        assert_eq!(expand("%H", &c), Err(SpecError::Missing('H')));
        c.hostname = Some("tab.local".into());
        c.user_name = Some("k".into());
        c.uid = Some(10123);
        c.fragment = Some("/usr/lib/systemd/system/a.service".into());
        assert_eq!(expand("%H %l %u %U", &c).unwrap(), "tab.local tab k 10123");
        assert_eq!(
            expand("%y|%Y", &c).unwrap(),
            "/usr/lib/systemd/system/a.service|/usr/lib/systemd/system"
        );
    }

    #[test]
    fn tmp_dir_falls_back() {
        assert_eq!(tmp_dir(None), "/tmp");
        assert_eq!(tmp_dir(Some(String::new())), "/tmp");
        assert_eq!(tmp_dir(Some("/t".into())), "/t");
    }

    #[test]
    fn errors() {
        let c = ctx("a.service", Scope::System);
        assert_eq!(expand("%q", &c), Err(SpecError::Unknown('q')));
        assert_eq!(expand("50%", &c), Err(SpecError::Trailing));
        assert_eq!(SpecError::Unknown('q').to_string(), "unknown specifier %q");
    }
}
