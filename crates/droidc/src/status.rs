//! `status` and `shell-spec`. Both are read-only and work inside the guest with `DROIDC_ROOT`
//! pointing at the bound root and `DROIDC_CONTAINER` naming the container: they only follow the
//! relative `current`/`previous` symlinks.

use crate::error::{Context, Error, Result};
use crate::layout::Root;
use crate::platform::Platform;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn rec(root: &Root, id: Option<String>) -> Value {
    id.and_then(|i| root.read_record(&i).ok())
        .map(|r| serde_json::to_value(r).unwrap())
        .unwrap_or(Value::Null)
}

pub fn status(root: &Root) -> Result<Value> {
    let platform = Platform::detect()
        .ok()
        .map(|p| serde_json::to_value(p).unwrap())
        .unwrap_or(Value::Null);
    let pending = root
        .read_pending()?
        .map(|p| serde_json::to_value(p).unwrap())
        .unwrap_or(Value::Null);
    Ok(json!({
        "root": root.path.display().to_string(),
        "container": root.container,
        "platform": platform,
        "current": rec(root, root.current_id()),
        "previous": rec(root, root.previous_id()),
        // Non-null only while an operation is in progress or was interrupted (recovered on next mutating command).
        "staged": pending,
    }))
}

pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// How Selkie enters the deployment. Selkie chooses: droidc runs as the app uid and cannot tell
/// whether a root launcher is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    /// ptrace-based, no root. Selkie adds the Android binds (`/dev`, `/proc`, ...).
    Proot,
    /// Real root (Magisk/KernelSU/Shizuku-as-root): new mount+pid namespace, the `mounts` below,
    /// the binds, then chroot. Native syscall speed, real cgroups for `systemd --user`.
    Chroot,
}

impl Launcher {
    pub fn parse(s: &str) -> Option<Launcher> {
        Some(match s {
            "proot" => Launcher::Proot,
            "chroot" => Launcher::Chroot,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bind {
    pub host: String,
    pub guest: String,
}

/// A filesystem to mount under the rootfs (`fstype`) or a host path to bind recursively (`bind`).
/// `options` is fstab style: `ro,nosuid,nodev,noexec` become mount flags, the rest is fs data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mount {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fstype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    pub guest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
    /// Failure is a warning, not an error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

/// A symlink created at `guest` (after the mounts) pointing at `target`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Symlink {
    pub target: String,
    pub guest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LauncherSpec {
    Proot {
        requires_root: bool,
        /// The deployment contains link2symlink chains; proot must run with --link2symlink.
        link2symlink: bool,
    },
    Chroot {
        requires_root: bool,
        /// `mount` is required; the others are best-effort (Android kernels may lack some).
        namespaces: Vec<String>,
        /// Mounted inside the new mount namespace before the binds; guest paths are in `rootfs`.
        mounts: Vec<Mount>,
        symlinks: Vec<Symlink>,
        warnings: Vec<String>,
    },
}

/// What `supervise --systemd` needs on top of the deployment's binds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemdSpec {
    /// Bound after `/sys`: the writable cgroup stand-in.
    pub binds: Vec<Bind>,
    /// Where the LD_PRELOAD cgroup shim (built for the guest's glibc) is placed in the guest.
    pub shim: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShellSpec {
    pub rootfs: String,
    pub binds: Vec<Bind>,
    pub systemd: SystemdSpec,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub session_label: Option<String>,
    /// The graphical session (see `session::describe`).
    pub session: Value,
    pub launcher: LauncherSpec,
    /// Guest paths the launcher binds its guest droidc over (as it binds `/usr/bin/droidc`): the
    /// `sudo`/`su`/`pkexec` shims, which ask the host spawner for a root command. Only under proot;
    /// chroot has a real root path, and binding over a setuid binary there would disable the real thing.
    pub shims: Vec<String>,
}

fn fs_mount(fstype: &str, guest: &str, options: &str) -> Mount {
    Mount {
        fstype: Some(fstype.into()),
        bind: None,
        guest: guest.into(),
        options: Some(options.into()),
        optional: false,
    }
}

fn bind_mount(host: &str) -> Mount {
    Mount {
        fstype: None,
        bind: Some(host.into()),
        guest: host.into(),
        options: None,
        optional: false,
    }
}

/// A private `/dev` (tmpfs) with only the harmless host nodes: the guest never sees Android's
/// block devices or binder. Selkie adds GPU, input, fuse and tun nodes with `--bind`.
pub fn chroot_mounts() -> Vec<Mount> {
    let mut m = vec![
        fs_mount("proc", "/proc", "nosuid,nodev,noexec"),
        fs_mount("sysfs", "/sys", "ro,nosuid,nodev,noexec"),
        // Only mounted inside a cgroup namespace, never Android's whole tree.
        Mount {
            optional: true,
            ..fs_mount("cgroup2", "/sys/fs/cgroup", "nosuid,nodev,noexec")
        },
        fs_mount("tmpfs", "/dev", "nosuid,mode=0755"),
    ];
    m.extend(
        ["null", "zero", "full", "random", "urandom", "tty"]
            .map(|n| bind_mount(&format!("/dev/{n}"))),
    );
    m.extend([
        fs_mount(
            "devpts",
            "/dev/pts",
            "nosuid,noexec,newinstance,ptmxmode=0666,mode=0620",
        ),
        fs_mount("tmpfs", "/dev/shm", "nosuid,nodev,mode=1777"),
        fs_mount("tmpfs", "/run", "nosuid,nodev,mode=0755"),
        fs_mount("tmpfs", "/tmp", "nosuid,nodev,mode=1777"),
    ]);
    m
}

pub fn chroot_symlinks() -> Vec<Symlink> {
    [
        ("pts/ptmx", "/dev/ptmx"),
        ("/proc/self/fd", "/dev/fd"),
        ("/proc/self/fd/0", "/dev/stdin"),
        ("/proc/self/fd/1", "/dev/stdout"),
        ("/proc/self/fd/2", "/dev/stderr"),
    ]
    .map(|(t, g)| Symlink {
        target: t.into(),
        guest: g.into(),
    })
    .to_vec()
}

pub fn shell_spec(root: &Root, launcher: Launcher) -> Result<ShellSpec> {
    let root_path = root.path.as_path();
    let cur = root
        .current_id()
        .ok_or_else(|| Error::not_found("no current deployment"))?;
    let record = root.read_record(&cur)?;
    let abs = std::path::absolute(root_path).unwrap_or_else(|_| root_path.to_path_buf());
    let r = abs.display().to_string();
    // This container's state directory, absolute like `r`.
    let c = abs.join(root.cdir.strip_prefix(&root.path).unwrap_or(Path::new("")));
    let c = c.display().to_string().trim_end_matches('/').to_string();
    let rootfs = format!("{c}/current/rootfs");
    let link2symlink = record.stats.link_strategy_used.contains("link2symlink");
    let bind = |host: String, guest: &str| Bind {
        host,
        guest: guest.into(),
    };
    let mut binds = vec![
        bind(format!("{c}/var"), "/var"),
        bind(format!("{c}/home"), "/home"),
        bind(r.clone(), "/run/droidc/root"),
    ];
    // The guest droidc lives at a host path only the launcher knows (Selkie's libdroidc-guest.so), so
    // the shims are listed for it to bind rather than bound here.
    let shims = match launcher {
        Launcher::Proot => crate::spawn::SHIMS.iter().map(|s| s.to_string()).collect(),
        Launcher::Chroot => Vec::new(),
    };
    let launcher = match launcher {
        Launcher::Proot => LauncherSpec::Proot {
            requires_root: false,
            link2symlink,
        },
        Launcher::Chroot => {
            let mut warnings = Vec::new();
            if link2symlink {
                // The chains are absolute symlinks to `<canonical root>/...` on the host. Binding
                // the root at that same path inside the guest makes them resolve without proot
                // (they still look like symlinks to lstat, not hard links).
                let canon = fs::canonicalize(root_path).context(root_path.display())?;
                let c = canon.display().to_string();
                binds.push(bind(c.clone(), &c));
                warnings.push("deployment uses link2symlink; redeploy with --links hardlink as root for real hard links".into());
            }
            if record.ownership.as_deref() != Some(crate::record::OWNERSHIP_IMAGE) {
                warnings.push("ownership not applied (run `droidc fix-ownership` as root): setuid binaries (sudo, su, pkexec) and ownership checks (sshd, polkit) will fail".into());
            }
            LauncherSpec::Chroot {
                requires_root: true,
                namespaces: ["mount", "pid", "uts", "ipc", "cgroup"]
                    .map(String::from)
                    .to_vec(),
                mounts: chroot_mounts(),
                symlinks: chroot_symlinks(),
                warnings,
            }
        }
    };
    let env = [
        ("DROIDC_ROOT", "/run/droidc/root"),
        ("DROIDC_CONTAINER", root.container.as_str()),
        ("HOME", "/root"),
        ("PATH", DEFAULT_PATH),
        ("TERM", "xterm-256color"),
        ("LANG", "C.UTF-8"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .into();
    Ok(ShellSpec {
        rootfs,
        binds,
        // Only for `supervise --systemd`: the cgroup stand-in must be bound after `/sys`, and the
        // LD_PRELOAD library (built for the guest's glibc) placed at `shim`.
        systemd: SystemdSpec {
            binds: vec![bind(format!("{c}/cgroup"), "/sys/fs/cgroup")],
            shim: "/usr/lib/droidc/cgshim.so".into(),
        },
        env,
        cwd: "/root".into(),
        session: crate::session::describe(
            &crate::session::resolve(root)?,
            record.session_label.as_deref(),
        ),
        session_label: record.session_label,
        launcher,
        shims,
    })
}
