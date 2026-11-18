//! `droidc enter -- cmd...`: run a command in the current deployment with real root instead of
//! proot (Magisk, KernelSU, Shizuku started as root; Selkie runs this through `su`).
//!
//! The parent unshares mount/pid/uts/ipc namespaces, makes every mount private so nothing leaks
//! into Android's namespace, and re-execs itself as `__enter-init`, which is PID 1 of the new pid
//! namespace. The init dies with the parent (`PR_SET_PDEATHSIG`), and the kernel then kills
//! everything else in the namespace. The init mounts the chroot launcher's `mounts`, then the
//! binds, chroots, runs the command and reaps until it exits.
//!
//! Every guest path is resolved with `openat2(RESOLVE_IN_ROOT)` against the rootfs, so a symlink
//! in the image (bootc's `/home -> var/home`, or a hostile absolute one) can never make a mount
//! land on the host. Missing mount points are created inside the rootfs.

use crate::error::{Context, Error, Result};
use crate::status::{self, Bind, Launcher, LauncherSpec, Mount};
use rustix::fs::{mkdirat, openat, openat2, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use rustix::mount::{mount, mount_bind_recursive, mount_change, MountFlags, MountPropagationFlags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::process::Command;

/// Everything the init needs, passed from the parent as JSON on the command line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Canonical (`deployments/<id>/rootfs`): pins the deployment against a concurrent switch.
    pub rootfs: String,
    pub mounts: Vec<Mount>,
    pub binds: Vec<Bind>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub hostname: Option<String>,
}

#[derive(Debug, Default)]
pub struct EnterOpts {
    /// `HOST:GUEST`, after the deployment's binds (e.g. `/usr/bin/droidc` -> libdroidc-guest.so).
    pub binds: Vec<String>,
    /// `KEY=VALUE`, over the deployment's environment.
    pub env: Vec<String>,
    pub hostname: Option<String>,
}

pub fn plan(root_path: &Path, opts: &EnterOpts) -> Result<(Plan, Vec<String>)> {
    let spec = status::shell_spec(root_path, Launcher::Chroot)?;
    let LauncherSpec::Chroot {
        mounts, warnings, ..
    } = spec.launcher
    else {
        unreachable!("chroot spec");
    };
    let rootfs = fs::canonicalize(&spec.rootfs).context(&spec.rootfs)?;
    let mut binds = spec.binds;
    for b in &opts.binds {
        let (host, guest) = b
            .rsplit_once(':')
            .filter(|(h, g)| !h.is_empty() && g.starts_with('/'))
            .ok_or_else(|| Error::invalid(format!("--bind '{b}': expected HOST:/GUEST")))?;
        binds.push(Bind {
            host: host.into(),
            guest: guest.into(),
        });
    }
    let mut env = spec.env;
    for e in &opts.env {
        let (k, v) = e
            .split_once('=')
            .filter(|(k, _)| !k.is_empty())
            .ok_or_else(|| Error::invalid(format!("--env '{e}': expected KEY=VALUE")))?;
        env.insert(k.into(), v.into());
    }
    Ok((
        Plan {
            rootfs: rootfs.display().to_string(),
            mounts,
            binds,
            env,
            cwd: spec.cwd,
            hostname: opts.hostname.clone(),
        },
        warnings,
    ))
}

/// Parent side: returns the command's exit status.
pub fn enter(root_path: &Path, opts: &EnterOpts, cmd: &[String]) -> Result<i32> {
    if cmd.is_empty() {
        return Err(Error::invalid("enter needs a command after --"));
    }
    if !rustix::process::geteuid().is_root() {
        return Err(Error::invalid(
            "enter needs root (run it through su); without root use shell-spec and proot",
        ));
    }
    let (plan, warnings) = plan(root_path, opts)?;
    for w in &warnings {
        eprintln!("warn: {w}");
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let flags = rustix::thread::UnshareFlags::NEWNS
        | rustix::thread::UnshareFlags::NEWPID
        | rustix::thread::UnshareFlags::NEWUTS
        | rustix::thread::UnshareFlags::NEWIPC;
    // SAFETY: no FILES flag, so no fd table is unshared. Single-threaded here (unshare(NEWNS)
    // would fail with EINVAL otherwise).
    unsafe { rustix::thread::unshare_unsafe(flags) }.map_err(|e| sys("unshare", e))?;
    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| sys("make / rprivate", e))?;
    // The first child after unshare(NEWPID) is PID 1 of the new namespace.
    let mut child = Command::new(exe)
        .arg("__enter-init")
        .arg(serde_json::to_string(&plan)?)
        .arg("--")
        .args(cmd)
        .spawn()
        .context("spawn init")?;
    let st = child.wait().context("wait init")?;
    use std::os::unix::process::ExitStatusExt;
    Ok(st
        .code()
        .or_else(|| st.signal().map(|s| 128 + s))
        .unwrap_or(1))
}

/// PID 1 of the new pid namespace.
pub fn init(plan_json: &str, cmd: &[String]) -> Result<i32> {
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
        .map_err(|e| sys("PR_SET_PDEATHSIG", e))?;
    let plan: Plan = serde_json::from_str(plan_json)?;
    let rootfd = rustix::fs::open(
        &plan.rootfs,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| sys(&plan.rootfs, e))?;
    for m in &plan.mounts {
        if let Err(e) = do_mount(&rootfd, m) {
            if !m.optional {
                return Err(e);
            }
            eprintln!("warn: {e}");
        }
        if m.fstype.as_deref() == Some("devpts") {
            // /dev is the host's: point its ptmx at our devpts instance.
            bind_ptmx(&rootfd, &format!("{}/ptmx", m.guest))?;
        }
    }
    for b in &plan.binds {
        bind_into(&rootfd, &b.host, &b.guest)?;
    }
    if let Some(h) = &plan.hostname {
        rustix::system::sethostname(h.as_bytes()).map_err(|e| sys("sethostname", e))?;
    }
    rustix::process::chroot(&plan.rootfs).map_err(|e| sys("chroot", e))?;
    if std::env::set_current_dir(&plan.cwd).is_err() {
        std::env::set_current_dir("/").context("chdir /")?;
    }
    let child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .env_clear()
        .envs(&plan.env)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::not_found(format!("{}: command not found", cmd[0]))
            } else {
                Error::internal(format!("{}: {e}", cmd[0]))
            }
        })?;
    // When PID 1 exits the kernel kills the rest of the namespace.
    crate::supervise::reap_until(child.id() as i32)
}

fn bind_ptmx(rootfd: &OwnedFd, ptmx: &str) -> Result<()> {
    let src = fd_of(rootfd, ptmx)?;
    let dst = ensure_target(rootfd, "/dev/ptmx", false)?;
    mount_bind_recursive(proc_fd(&src), proc_fd(&dst)).map_err(|e| sys("bind ptmx", e))
}

fn do_mount(rootfd: &OwnedFd, m: &Mount) -> Result<()> {
    let target = ensure_target(rootfd, &m.guest, true)?;
    let t = proc_fd(&target);
    match (&m.fstype, &m.bind) {
        (Some(fstype), _) => {
            let (flags, data) = parse_options(m.options.as_deref().unwrap_or(""));
            let data = CString::new(data).map_err(|_| Error::invalid("NUL in mount options"))?;
            mount(
                fstype.as_str(),
                t.as_str(),
                fstype.as_str(),
                flags,
                Some(data.as_c_str()),
            )
            .map_err(|e| sys(&format!("mount {fstype} on {}", m.guest), e))
        }
        (None, Some(host)) => mount_bind_recursive(host.as_str(), t.as_str())
            .map_err(|e| sys(&format!("bind {host} on {}", m.guest), e)),
        (None, None) => Err(Error::invalid(format!(
            "mount {}: no fstype or bind",
            m.guest
        ))),
    }
}

/// Bind the host path `host` onto `guest` inside the rootfs, creating the mount point (a
/// directory or an empty file, matching `host`).
fn bind_into(rootfd: &OwnedFd, host: &str, guest: &str) -> Result<()> {
    let is_dir = fs::metadata(host).context(host)?.is_dir();
    let target = ensure_target(rootfd, guest, is_dir)?;
    mount_bind_recursive(host, proc_fd(&target).as_str())
        .map_err(|e| sys(&format!("bind {host} on {guest}"), e))
}

fn proc_fd(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

const RESOLVE: ResolveFlags = ResolveFlags::IN_ROOT.union(ResolveFlags::NO_MAGICLINKS);

/// `O_PATH` fd for `guest` resolved inside `rootfd`.
fn fd_of(rootfd: &OwnedFd, guest: &str) -> Result<OwnedFd> {
    let rel = guest.trim_start_matches('/');
    let rel = if rel.is_empty() { "." } else { rel };
    openat2(
        rootfd,
        rel,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        RESOLVE,
    )
    .map_err(|e| sys(guest, e))
}

/// `O_PATH` fd for `guest` inside the rootfs, creating missing components (directories, and an
/// empty file for the last one when `dir` is false). Each prefix is resolved from the rootfs with
/// `RESOLVE_IN_ROOT`, so symlinks, absolute ones included, stay inside it; a dangling symlink is
/// followed (inside the rootfs) and its target created.
pub fn ensure_target(rootfd: &OwnedFd, guest: &str, dir: bool) -> Result<OwnedFd> {
    ensure_target_at(rootfd, guest, dir, 0)
}

fn ensure_target_at(rootfd: &OwnedFd, guest: &str, dir: bool, depth: u32) -> Result<OwnedFd> {
    if depth > 40 {
        return Err(sys(guest, Errno::LOOP));
    }
    if let Ok(fd) = fd_of(rootfd, guest) {
        return Ok(fd);
    }
    let comps: Vec<&str> = guest
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let mut parent = fd_of(rootfd, "/")?;
    for (i, c) in comps.iter().enumerate() {
        let prefix = comps[..=i].join("/");
        if let Ok(fd) = fd_of(rootfd, &prefix) {
            parent = fd;
            continue;
        }
        if let Ok(link) = rustix::fs::readlinkat(parent.as_fd(), *c, Vec::new()) {
            let link = link.to_string_lossy().into_owned();
            let base = if link.starts_with('/') {
                String::new()
            } else {
                comps[..i].join("/")
            };
            let rest = comps[i + 1..].join("/");
            let next = format!("/{base}/{link}/{rest}");
            return ensure_target_at(rootfd, &next, dir, depth + 1);
        }
        let made = if i + 1 == comps.len() && !dir {
            openat(
                parent.as_fd(),
                *c,
                OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o644),
            )
            .map(drop)
        } else {
            mkdirat(parent.as_fd(), *c, Mode::from_raw_mode(0o755))
        };
        match made {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(sys(&format!("create /{prefix}"), e)),
        }
        parent = fd_of(rootfd, &prefix)?;
    }
    Ok(parent)
}

/// fstab-style options: flag words become `MountFlags`, the rest is passed as fs data.
fn parse_options(opts: &str) -> (MountFlags, String) {
    let mut flags = MountFlags::empty();
    let mut data = Vec::new();
    for o in opts.split(',').filter(|o| !o.is_empty()) {
        match o {
            "ro" => flags |= MountFlags::RDONLY,
            "nosuid" => flags |= MountFlags::NOSUID,
            "nodev" => flags |= MountFlags::NODEV,
            "noexec" => flags |= MountFlags::NOEXEC,
            _ => data.push(o),
        }
    }
    (flags, data.join(","))
}

fn sys(what: &str, e: Errno) -> Error {
    let e = std::io::Error::from(e);
    Error::new(crate::error::io_kind(&e), format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn open_root(p: &Path) -> OwnedFd {
        rustix::fs::open(p, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap()
    }

    #[test]
    fn options_split_into_flags_and_data() {
        let (f, d) = parse_options("ro,nosuid,nodev,noexec");
        assert_eq!(
            f,
            MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC
        );
        assert_eq!(d, "");
        let (f, d) = parse_options("nosuid,noexec,newinstance,ptmxmode=0666");
        assert_eq!(f, MountFlags::NOSUID | MountFlags::NOEXEC);
        assert_eq!(d, "newinstance,ptmxmode=0666");
    }

    #[test]
    fn targets_stay_inside_the_rootfs() {
        let d = tempfile::tempdir().unwrap();
        let host = d.path().join("host");
        let rootfs = d.path().join("rootfs");
        fs::create_dir_all(&host).unwrap();
        fs::create_dir_all(&rootfs).unwrap();
        // bootc: relative; a hostile image: absolute and escaping with ..
        symlink("var/home", rootfs.join("home")).unwrap();
        symlink(&host, rootfs.join("abs")).unwrap();
        symlink("../../../host", rootfs.join("up")).unwrap();
        let r = open_root(&rootfs);

        ensure_target(&r, "/home/user", true).unwrap();
        assert!(rootfs.join("var/home/user").is_dir());

        ensure_target(&r, "/abs/x", true).unwrap();
        ensure_target(&r, "/up/y", true).unwrap();
        assert!(
            fs::read_dir(&host).unwrap().next().is_none(),
            "nothing created on the host"
        );
        assert!(rootfs
            .join(host.strip_prefix("/").unwrap())
            .join("x")
            .is_dir());
        assert!(rootfs.join("host/y").is_dir());

        ensure_target(&r, "/usr/bin/droidc", false).unwrap();
        assert!(rootfs.join("usr/bin/droidc").is_file());
        // existing targets are reused
        ensure_target(&r, "/usr/bin/droidc", false).unwrap();
        ensure_target(&r, "/", true).unwrap();
    }
}
