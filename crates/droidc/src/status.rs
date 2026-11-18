//! `status` and `shell-spec`. Both are read-only and work inside the guest with `DROIDC_ROOT`
//! pointing at the bound root: they only follow the relative `current`/`previous` symlinks.

use crate::error::{Context, Error, Result};
use crate::layout::Root;
use crate::platform::Platform;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

fn rec(root: &Root, id: Option<String>) -> Value {
    id.and_then(|i| root.read_record(&i).ok())
        .map(|r| serde_json::to_value(r).unwrap())
        .unwrap_or(Value::Null)
}

pub fn status(root_path: &Path) -> Result<Value> {
    let root = Root::new(root_path);
    let platform = Platform::detect()
        .ok()
        .map(|p| serde_json::to_value(p).unwrap())
        .unwrap_or(Value::Null);
    let pending = root
        .read_pending()?
        .map(|p| serde_json::to_value(p).unwrap())
        .unwrap_or(Value::Null);
    Ok(json!({
        "root": root_path.display().to_string(),
        "platform": platform,
        "current": rec(&root, root.current_id()),
        "previous": rec(&root, root.previous_id()),
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

pub fn shell_spec(root_path: &Path, launcher: Launcher) -> Result<Value> {
    let root = Root::new(root_path);
    let cur = root
        .current_id()
        .ok_or_else(|| Error::not_found("no current deployment"))?;
    let record = root.read_record(&cur)?;
    let abs = std::path::absolute(root_path).unwrap_or_else(|_| root_path.to_path_buf());
    let r = abs.display().to_string();
    let rootfs = format!("{r}/current/rootfs");
    let link2symlink = record.stats.link_strategy_used.contains("link2symlink");
    let mut binds = vec![
        json!({"host": format!("{r}/var"), "guest": "/var"}),
        json!({"host": format!("{r}/home"), "guest": "/home"}),
        json!({"host": r, "guest": "/run/droidc/root"}),
    ];
    let launcher = match launcher {
        Launcher::Proot => json!({
            "kind": "proot",
            "requires_root": false,
            // The deployment contains link2symlink chains; proot must run with --link2symlink.
            "link2symlink": link2symlink,
        }),
        Launcher::Chroot => {
            let mut warnings = Vec::new();
            if link2symlink {
                // The chains are absolute symlinks to `<canonical root>/...` on the host. Binding
                // the root at that same path inside the guest makes them resolve without proot
                // (they still look like symlinks to lstat, not hard links).
                let canon = fs::canonicalize(root_path).context(root_path.display())?;
                let c = canon.display().to_string();
                binds.push(json!({"host": c, "guest": c}));
                warnings.push("deployment uses link2symlink; redeploy with --links hardlink as root for real hard links");
            }
            if !owned_by_root(Path::new(&rootfs)) {
                warnings.push("ownership not applied: files belong to the app uid, so setuid binaries (sudo, su, pkexec) and ownership checks (sshd, polkit) will fail");
            }
            json!({
                "kind": "chroot",
                "requires_root": true,
                "namespaces": ["mount", "pid", "uts", "ipc"],
                // Mounted inside the new mount namespace, after the chroot target is known and
                // before the binds; guest paths are relative to `rootfs`.
                "mounts": [
                    {"fstype": "proc", "guest": "/proc"},
                    {"fstype": "sysfs", "guest": "/sys"},
                    {"fstype": "cgroup2", "guest": "/sys/fs/cgroup"},
                    {"bind": "/dev", "guest": "/dev"},
                    {"fstype": "devpts", "guest": "/dev/pts", "options": "newinstance,ptmxmode=0666"},
                    {"fstype": "tmpfs", "guest": "/run", "options": "mode=0755"},
                    {"fstype": "tmpfs", "guest": "/tmp", "options": "mode=1777"},
                ],
                "warnings": warnings,
            })
        }
    };
    Ok(json!({
        "rootfs": rootfs,
        "binds": binds,
        // Only for `supervise --systemd`: the cgroup stand-in must be bound after `/sys`, and the
        // LD_PRELOAD library (built for the guest's glibc) placed at `shim`.
        "systemd": {
            "binds": [{"host": format!("{r}/cgroup"), "guest": "/sys/fs/cgroup"}],
            "shim": "/usr/lib/droidc/cgshim.so",
        },
        "env": {
            "DROIDC_ROOT": "/run/droidc/root",
            "HOME": "/root",
            "PATH": DEFAULT_PATH,
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
        },
        "cwd": "/root",
        "session_label": record.session_label,
        "session": crate::session::describe(
            &crate::session::resolve(root_path)?,
            record.session_label.as_deref(),
        ),
        "launcher": launcher,
    }))
}

/// Whether unpack applied image ownership (it does not without root: everything is the app uid).
fn owned_by_root(rootfs: &Path) -> bool {
    fs::symlink_metadata(rootfs.join("usr"))
        .map(|m| m.uid() == 0)
        .unwrap_or(false)
}
