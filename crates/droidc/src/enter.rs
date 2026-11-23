//! `droidc enter -- cmd...`: run a command in the current deployment with real root instead of
//! proot (Magisk, KernelSU, Shizuku started as root; Selkie runs this through `su`).
//!
//! The parent unshares a mount namespace (required) and pid/uts/ipc/cgroup namespaces (each
//! best-effort: Android kernels may lack some), makes every mount private so nothing leaks into
//! Android's namespace, and re-execs itself as `__enter-init`. With a pid namespace the init is its
//! PID 1: it dies with the parent (`PR_SET_PDEATHSIG`) and the kernel then kills everything else in
//! the namespace. Without one it is a subreaper and kills what is left when the command exits. The
//! init mounts the chroot launcher's `mounts`, creates its `symlinks`, binds, chroots, runs the
//! command and reaps until it exits.
//!
//! Every guest path is resolved with `openat2(RESOLVE_IN_ROOT)` against the rootfs, so a symlink
//! in the image (bootc's `/home -> var/home`, or a hostile absolute one) can never make a mount
//! land on the host. Missing mount points are created inside the rootfs.

use crate::error::{Context, Error, Result};
use crate::layout::Root;
use crate::status::{self, Bind, Launcher, LauncherSpec, Mount, Symlink};
use rustix::fs::{
    chownat, fstat, mkdirat, openat, openat2, symlinkat, AtFlags, Mode, OFlags, ResolveFlags,
};
use rustix::io::Errno;
use rustix::mount::{mount, mount_bind_recursive, mount_change, MountFlags, MountPropagationFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
    pub symlinks: Vec<Symlink>,
    pub binds: Vec<Bind>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub hostname: Option<String>,
    /// The namespaces the parent actually got (set after unshare).
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// The `enter` process: without a pid namespace the init watches for it to go away.
    #[serde(default)]
    pub parent: Option<i32>,
}

impl Plan {
    fn has_ns(&self, name: &str) -> bool {
        self.namespaces.iter().any(|n| n == name)
    }
}

#[derive(Debug, Default)]
pub struct EnterOpts {
    /// `HOST:GUEST`, after the deployment's binds (e.g. `/usr/bin/droidc` -> libdroidc-guest.so).
    pub binds: Vec<String>,
    /// `KEY=VALUE`, over the deployment's environment.
    pub env: Vec<String>,
    pub hostname: Option<String>,
}

pub fn plan(root: &Root, opts: &EnterOpts) -> Result<(Plan, Vec<String>)> {
    let spec = status::shell_spec(root, Launcher::Chroot)?;
    let LauncherSpec::Chroot {
        mounts,
        symlinks,
        warnings,
        ..
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
            symlinks,
            binds,
            env,
            cwd: spec.cwd,
            hostname: opts.hostname.clone(),
            namespaces: Vec::new(),
            parent: None,
        },
        warnings,
    ))
}

/// Parent side: returns the command's exit status.
pub fn enter(root: &Root, opts: &EnterOpts, cmd: &[String]) -> Result<i32> {
    if cmd.is_empty() {
        return Err(Error::invalid("enter needs a command after --"));
    }
    if !rustix::process::geteuid().is_root() {
        return Err(Error::invalid(
            "enter needs root (run it through su); without root use shell-spec and proot",
        ));
    }
    // Selkie stops a session with SIGTERM (or the terminal's SIGHUP); an "ignore" inherited from
    // whatever launched us (su, `unshare --fork`, ...) would survive exec and keep the guest alive.
    // Reset both, for us and, through exec, for the init and the guest command.
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: plain signal(2) to the default action; no handler runs.
        unsafe { libc::signal(sig, libc::SIG_DFL) };
    }
    let (mut plan, warnings) = plan(root, opts)?;
    for w in &warnings {
        eprintln!("warn: {w}");
    }
    let exe = std::env::current_exe().context("current_exe")?;
    use rustix::thread::{unshare_unsafe, UnshareFlags};
    // SAFETY (all unshare calls): no FILES flag, so no fd table is unshared. Single-threaded here
    // (unshare(NEWNS) would fail with EINVAL otherwise).
    unsafe { unshare_unsafe(UnshareFlags::NEWNS) }
        .map_err(|e| sys("unshare mount namespace", e))?;
    plan.namespaces.push("mount".into());
    for (name, flag) in [
        ("pid", UnshareFlags::NEWPID),
        ("uts", UnshareFlags::NEWUTS),
        ("ipc", UnshareFlags::NEWIPC),
        ("cgroup", UnshareFlags::NEWCGROUP),
    ] {
        // DROIDC_ENTER_NO_NS=pid,ipc,...: behave as if the kernel lacked them (testing).
        let skip = std::env::var("DROIDC_ENTER_NO_NS").unwrap_or_default();
        if skip.split(',').any(|s| s == name) {
            eprintln!("warn: {name} namespace skipped (DROIDC_ENTER_NO_NS)");
            continue;
        }
        match unsafe { unshare_unsafe(flag) } {
            Ok(()) => plan.namespaces.push(name.into()),
            Err(e) => eprintln!("warn: no {name} namespace: {e}"),
        }
    }
    if plan.hostname.is_some() && !plan.has_ns("uts") {
        // sethostname would rename Android itself.
        eprintln!("warn: --hostname ignored without a uts namespace");
        plan.hostname = None;
    }
    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| sys("make / rprivate", e))?;
    plan.parent = Some(std::process::id() as i32);
    // With a pid namespace, the first child after unshare(NEWPID) is its PID 1.
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

/// The init: PID 1 of the new pid namespace, or a subreaper without one.
pub fn init(plan_json: &str, cmd: &[String]) -> Result<i32> {
    let plan: Plan = serde_json::from_str(plan_json)?;
    let pid_ns = plan.has_ns("pid");
    if pid_ns {
        // When we die the kernel kills the whole namespace, so dying with `enter` is enough.
        rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
            .map_err(|e| sys("PR_SET_PDEATHSIG", e))?;
    } else {
        // Nothing kills the guest for us (Android kernels often lack CONFIG_PID_NS). Orphans
        // reparent to us; we outlive `enter` long enough to kill everything below us, whether the
        // command exits or `enter` is killed (see `reap_or_orphaned`).
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
            .map_err(|e| sys("PR_SET_CHILD_SUBREAPER", e))?;
    }
    let rootfd = rustix::fs::open(
        &plan.rootfs,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| sys(&plan.rootfs, e))?;
    for m in &plan.mounts {
        if m.fstype.as_deref() == Some("cgroup2") && !plan.has_ns("cgroup") {
            eprintln!("warn: no cgroup namespace, not mounting {}", m.guest);
            continue;
        }
        if let Err(e) = do_mount(&rootfd, m) {
            if !m.optional {
                return Err(e);
            }
            eprintln!("warn: {e}");
        }
    }
    for l in &plan.symlinks {
        make_symlink(&rootfd, &l.target, &l.guest)?;
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
    if pid_ns {
        // When PID 1 exits the kernel kills the rest of the namespace.
        return crate::supervise::reap_until(child.id() as i32);
    }
    let parent = plan.parent.unwrap_or(0);
    let code = reap_or_orphaned(child.id() as i32, || current_parent() == parent);
    kill_descendants();
    // `enter` was killed (SIGTERM from Selkie, hangup): report it like a signal death.
    Ok(code.unwrap_or(128 + 15))
}

fn current_parent() -> i32 {
    rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get())
}

/// Reap every child until `pid` exits (its exit code, or 128+signal), or return None as soon as
/// `parent_alive` turns false. Polls every 50 ms instead of blocking in wait(2), because without a
/// pid namespace nothing would wake us when `enter` dies.
pub(crate) fn reap_or_orphaned(pid: i32, parent_alive: impl Fn() -> bool) -> Option<i32> {
    use rustix::process::{wait, WaitOptions};
    loop {
        match wait(WaitOptions::NOHANG) {
            Ok(Some((p, st))) if p.as_raw_nonzero().get() == pid => {
                return Some(
                    st.exit_status()
                        .or_else(|| st.terminating_signal().map(|s| 128 + s))
                        .unwrap_or(1),
                );
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(Errno::INTR) => {}
            // No children left at all: the command is gone without us seeing it.
            Err(_) => return Some(1),
        }
        if !parent_alive() {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// SIGKILL every process below us (reading the guest's `/proc`, which is in our pid namespace)
/// and reap them, until none is left.
fn kill_descendants() {
    use rustix::process::{kill_process, wait, Pid, Signal, WaitOptions};
    let me = rustix::process::getpid().as_raw_nonzero().get();
    for _ in 0..100 {
        let parents = ppids();
        let below: Vec<i32> = parents
            .keys()
            .copied()
            .filter(|&p| is_descendant(&parents, p, me))
            .collect();
        if below.is_empty() {
            return;
        }
        for p in below {
            let _ = kill_process(Pid::from_raw(p).unwrap(), Signal::KILL);
        }
        while let Ok(Some(_)) = wait(WaitOptions::NOHANG) {}
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn ppids() -> HashMap<i32, i32> {
    let mut m = HashMap::new();
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Ok(stat) = fs::read_to_string(e.path().join("stat")) {
            if let Some(ppid) = parse_ppid(&stat) {
                m.insert(pid, ppid);
            }
        }
    }
    m
}

/// `ppid` from `/proc/<pid>/stat` (`pid (comm) state ppid ...`; comm may contain spaces and ')').
fn parse_ppid(stat: &str) -> Option<i32> {
    stat.get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn is_descendant(parents: &HashMap<i32, i32>, mut p: i32, ancestor: i32) -> bool {
    for _ in 0..4096 {
        match parents.get(&p) {
            Some(&pp) if pp == ancestor => return true,
            Some(&pp) if pp > 0 && pp != p => p = pp,
            _ => return false,
        }
    }
    false
}

fn make_symlink(rootfd: &OwnedFd, target: &str, guest: &str) -> Result<()> {
    let (dir, name) = guest
        .rsplit_once('/')
        .filter(|(_, n)| !n.is_empty())
        .ok_or_else(|| Error::invalid(format!("symlink {guest}: no file name")))?;
    let parent = ensure_target(rootfd, if dir.is_empty() { "/" } else { dir }, true)?;
    match symlinkat(target, parent.as_fd(), name) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(sys(&format!("symlink {guest}"), e)),
    }
}

fn do_mount(rootfd: &OwnedFd, m: &Mount) -> Result<()> {
    if let (None, Some(host)) = (&m.fstype, &m.bind) {
        return bind_into(rootfd, host, &m.guest);
    }
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
        (None, Some(_)) => unreachable!("handled above"),
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
            Ok(()) => {
                // We are root but the tree belongs to the app uid, whose `gc` must be able to
                // delete what we add: give new entries their parent's owner.
                let st =
                    fstat(&parent).map_err(|e| sys(&format!("stat parent of /{prefix}"), e))?;
                let uid = rustix::process::Uid::from_raw(st.st_uid);
                let gid = rustix::process::Gid::from_raw(st.st_gid);
                chownat(&parent, *c, Some(uid), Some(gid), AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|e| sys(&format!("chown /{prefix}"), e))?;
                let dir = proc_fd(&parent);
                let label = crate::fsutil::selinux_label(Path::new(&dir), true);
                crate::fsutil::inherit_selinux_label(&Path::new(&dir).join(c), label.as_deref());
            }
            Err(Errno::EXIST) => {}
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

    /// wait(-1) would steal other tests' children (and they ours): run alone in a fresh process.
    #[test]
    fn reaping_stops_when_the_parent_goes_away() {
        const ME: &str = "enter::tests::reaping_stops_when_the_parent_goes_away";
        if std::env::var_os("DROIDC_ISOLATED_TEST").is_none() {
            let out = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", ME, "--test-threads=1"])
                .env("DROIDC_ISOLATED_TEST", "1")
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && log.contains("1 passed"), "{log}");
            return;
        }
        let mut c = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        assert_eq!(reap_or_orphaned(c.id() as i32, || true), Some(7));
        let _ = c.try_wait();
        let mut long = Command::new("sleep").arg("30").spawn().unwrap();
        let t = std::time::Instant::now();
        assert_eq!(reap_or_orphaned(long.id() as i32, || false), None);
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
        long.kill().unwrap();
        long.wait().unwrap();
    }

    #[test]
    fn ppid_parsing_and_descendants() {
        assert_eq!(parse_ppid("42 (a b) c)) S 7 42 42 0"), Some(7));
        assert_eq!(parse_ppid("garbage"), None);
        let m: HashMap<i32, i32> = [(10, 1), (11, 10), (12, 11), (20, 1), (1, 0)].into();
        assert!(is_descendant(&m, 12, 10));
        assert!(is_descendant(&m, 11, 10));
        assert!(!is_descendant(&m, 20, 10));
        assert!(!is_descendant(&m, 10, 10));
        let cyc: HashMap<i32, i32> = [(5, 6), (6, 5)].into();
        assert!(!is_descendant(&cyc, 5, 99));
    }

    #[test]
    fn symlinks_are_created_inside_the_rootfs() {
        let d = tempfile::tempdir().unwrap();
        let r = open_root(d.path());
        make_symlink(&r, "pts/ptmx", "/dev/ptmx").unwrap();
        make_symlink(&r, "pts/ptmx", "/dev/ptmx").unwrap(); // idempotent
        assert_eq!(
            fs::read_link(d.path().join("dev/ptmx")).unwrap(),
            Path::new("pts/ptmx")
        );
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
