//! `droidc supervise [--session] -- cmd...`: the in-guest session supervisor (no systemd).
//!
//! Makes sure `XDG_RUNTIME_DIR` exists (0700), starts a D-Bus session bus and exports
//! `DBUS_SESSION_BUS_ADDRESS`, runs `cmd` as a child and reaps every child (including orphans
//! re-parented to us) until it exits, then exits with its status. Under proot the guest has no PID 1,
//! so orphans only come to us because we mark ourselves a child subreaper first.
//!
//! With `--session` it also starts the deployment's graphical session (see `session.rs`): a system
//! bus, the session's services, its compositor, and for `screencast` sessions `droidc bridge`.
//! The session ends when the compositor or the bridge (or the given command) exits; everything
//! else started here is stopped then.

use crate::error::{Context, Error, Result};
use crate::session::{self, Renderer, SessionSpec};
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn runtime_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => {
            let uid = rustix::process::getuid().as_raw();
            let run = PathBuf::from(format!("/run/user/{uid}"));
            if fs::create_dir_all(&run).is_ok() {
                run
            } else {
                PathBuf::from(format!("/tmp/xdg-runtime-{uid}"))
            }
        }
    };
    fs::create_dir_all(&dir).context(dir.display())?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).context(dir.display())?;
    Ok(dir)
}

/// Start `dbus-daemon` with the session configuration listening on `socket` (also used for the
/// system bus: the system configuration switches to the `dbus` user, which an app uid can't).
/// Runs in the foreground so it is our child and stops with the session; returns its address.
fn start_dbus(socket: &std::path::Path, bin: &std::path::Path) -> Result<(String, Child)> {
    use std::io::BufRead;
    let _ = fs::remove_file(socket);
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).context(parent.display())?;
    }
    let mut child = Command::new(bin)
        .env_remove("WAYLAND_DISPLAY")
        .arg("--config-file=/usr/share/dbus-1/session.conf")
        .arg(format!("--address=unix:path={}", socket.display()))
        .arg("--nofork")
        .arg("--print-address=1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context(bin.display())?;
    let mut addr = String::new();
    let read = std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut addr);
    let addr = addr.trim().to_string();
    if read.is_err() || addr.is_empty() {
        let _ = child.kill();
        let st = child.wait()?;
        return Err(Error::internal(format!(
            "dbus-daemon exited with {st} before printing its address"
        )));
    }
    // The address is printed before the daemon finishes starting; if it then dies (e.g. SELinux
    // AVC init denied) the socket never accepts connections.
    std::thread::sleep(Duration::from_millis(250));
    if let Some(st) = child.try_wait()? {
        return Err(Error::internal(format!(
            "dbus-daemon exited with {st} after printing its address"
        )));
    }
    Ok((addr, child))
}

/// A bus on `socket`, exported as `var`. `dbus-daemon` first: it activates `.service` files itself,
/// while `dbus-broker-launch` hands activation to systemd, which can't run here. Best effort.
fn start_bus(socket: &std::path::Path, scope: &str, var: &str) -> Option<Child> {
    // Another supervisor (a terminal next to the desktop) may own a live bus here: share it rather
    // than replacing the socket under its clients.
    if std::os::unix::net::UnixStream::connect(socket).is_ok() {
        std::env::set_var(var, format!("unix:path={}", socket.display()));
        return None;
    }
    let started = if let Some(bin) = find_in_path("dbus-daemon") {
        start_dbus(socket, &bin)
    } else {
        let bin = find_in_path("dbus-broker-launch")?;
        crate::journal::start(false);
        start_broker(socket, &bin, &["--scope", scope])
    };
    match started {
        Ok((addr, child)) => {
            std::env::set_var(var, addr);
            Some(child)
        }
        Err(e) => {
            eprintln!("warn: could not start the {scope} bus: {e}");
            None
        }
    }
}

const MACHINE_ID: &str = "/etc/machine-id";

fn is_valid_machine_id(s: &str) -> bool {
    let s = s.trim();
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0')
}

/// bootc images ship `/etc/machine-id` empty (or `uninitialized`) and rely on first boot to fill it
/// in. There is no first boot here, and `sd_id128_get_machine` (dbus-broker-launch, systemd tools)
/// fails with ENOMEDIUM until it is set. Generate a random id once; it is per-installation state,
/// so droidc's `/etc` merge keeps it across upgrades. Returns whether it wrote the file.
fn ensure_machine_id(path: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Read;
    match fs::read_to_string(path) {
        Ok(cur) if is_valid_machine_id(&cur) => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut raw = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    fs::write(path, format!("{id}\n"))?;
    Ok(true)
}

/// Socket-activate `dbus-broker-launch --scope user` by hand (without `--systemd`, nothing here runs
/// the systemd user manager it expects): bind+listen `<dir>/bus`, pass it as fd 3 with `LISTEN_FDS=1`, `LISTEN_PID=<child pid>`.
/// Returns the bus address and the running broker.
fn start_broker(
    path: &std::path::Path,
    bin: &std::path::Path,
    args: &[&str],
) -> Result<(String, Child)> {
    let _ = fs::remove_file(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context(parent.display())?;
    }
    let listener = UnixListener::bind(path).context(path.display())?;
    let fd = listener.as_raw_fd();
    // `sh` sets LISTEN_PID to its own pid and execs the broker (same pid): no setenv between
    // fork and exec, which could deadlock in a multithreaded process.
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("LISTEN_PID=$$; export LISTEN_PID; exec \"$0\" \"$@\"")
        .arg(bin)
        .args(args)
        .env_remove("WAYLAND_DISPLAY")
        .env("LISTEN_FDS", "1")
        .env("LISTEN_FDNAMES", "dbus.socket")
        .stdin(Stdio::null());
    // SAFETY: runs in the forked child before exec; only dup2/fcntl (async-signal-safe).
    unsafe {
        cmd.pre_exec(move || {
            use rustix::io::{dup2, fcntl_setfd, FdFlags};
            use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};
            let src = BorrowedFd::borrow_raw(fd);
            if fd == 3 {
                fcntl_setfd(src, FdFlags::empty()).map_err(std::io::Error::from)?;
            } else {
                // dup2 onto fd 3 yields an fd without CLOEXEC.
                let mut dst = OwnedFd::from_raw_fd(3);
                dup2(src, &mut dst).map_err(std::io::Error::from)?;
                std::mem::forget(dst);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context(bin.display())?;
    drop(listener);
    // If it dies right away (no systemd environment), report failure.
    std::thread::sleep(Duration::from_millis(250));
    if let Some(st) = child.try_wait()? {
        let _ = fs::remove_file(path);
        return Err(Error::internal(format!(
            "{} exited with {st}",
            bin.display()
        )));
    }
    Ok((format!("unix:path={}", path.display()), child))
}

fn start_session_bus(dir: &std::path::Path) -> Option<Child> {
    start_bus(&dir.join("bus"), "user", "DBUS_SESSION_BUS_ADDRESS")
}

/// Options for [`supervise_with`]. `session: None` is the plain `supervise -- cmd` behaviour.
#[derive(Debug, Default)]
pub struct Opts {
    pub session: Option<SessionOpts>,
    pub systemd: SystemdMode,
}

/// Whether the image's own `systemd --user` runs the session services (on the cgroup shim, see
/// `crates/cgshim`) instead of a bare `dbus-daemon`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SystemdMode {
    /// Never: `dbus-daemon` and the session's own services (the default while this is proven)
    #[default]
    Off,
    /// When the image has systemd and the shim is installed; otherwise as `off`
    Auto,
    /// Always; fail when systemd or the shim is missing
    On,
}

const SYSTEMD: &str = "/usr/lib/systemd/systemd";
const USER_UNITS: &str = "/usr/lib/systemd/user";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const DEFAULT_SHIM: &str = "/usr/lib/droidc/cgshim.so";
const CGROUP2_MAGIC: i64 = 0x6367_7270;
/// The cgroup files systemd opens in the root of the tree (real cgroupfs has them itself).
const CGROUP_ROOT_FILES: [&str; 5] = [
    "cgroup.procs",
    "cgroup.controllers",
    "cgroup.subtree_control",
    "cgroup.events",
    "cgroup.type",
];

/// The shim to preload into `systemd --user`, when `mode` and the guest allow it.
fn resolve_systemd(
    mode: SystemdMode,
    systemd: &std::path::Path,
    shim: &std::path::Path,
) -> Result<Option<PathBuf>> {
    let present = systemd.is_file() && shim.is_file();
    match (mode, present) {
        (SystemdMode::Off, _) => Ok(None),
        (SystemdMode::Auto, false) => Ok(None),
        (_, true) => Ok(Some(shim.to_path_buf())),
        (SystemdMode::On, false) => Err(Error::not_found(format!(
            "--systemd=on needs {} and the cgroup shim {} (DROIDC_CGSHIM)",
            systemd.display(),
            shim.display()
        ))),
    }
}

/// What `systemd --user` insists on before it starts: `/run/systemd/system` (its `sd_booted()`
/// test) and a cgroup tree it can write. Without a real cgroup2 mount the tree is a plain directory
/// that needs the root `cgroup.*` files a kernel would provide.
/// Returns whether the tree is the fake one (a real cgroup2 mount needs no accounting).
fn prepare_systemd(run: &std::path::Path, cgroup: &std::path::Path) -> Result<bool> {
    let sys = run.join("systemd/system");
    fs::create_dir_all(&sys).context(sys.display())?;
    // `f_type`'s width and signedness differ by target (x86_64 glibc i64, aarch64 musl u32, ...).
    #[allow(clippy::unnecessary_cast)]
    let real = rustix::fs::statfs(cgroup)
        .map(|s| s.f_type as i64 == CGROUP2_MAGIC)
        .unwrap_or(false);
    if real {
        return Ok(false);
    }
    fs::create_dir_all(cgroup).context(cgroup.display())?;
    for f in CGROUP_ROOT_FILES {
        let p = cgroup.join(f);
        if !p.exists() {
            fs::write(&p, "").context(p.display())?;
        }
    }
    Ok(true)
}

/// A socket left by an earlier session. `wait_for_socket` would take a bus that is still answering (a
/// session being torn down) for the new one, and the bridge then dies with "Connection reset by peer"
/// when it goes. Wait up to `wait` for a listener to go away, and remove the socket file once nothing
/// answers. False when something still listens at the end: another live session's bus, left alone.
fn retire_stale_socket(path: &std::path::Path, wait: Duration) -> bool {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match UnixStream::connect(path) {
            Ok(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Ok(_) => return false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => {
                let _ = fs::remove_file(path);
                return true;
            }
        }
    }
}

/// What an earlier user manager left under `$XDG_RUNTIME_DIR` that the next one must not trust: its
/// generated units (a second run logs "Failed to create unit file ... File exists" for each) and its
/// transient units. `systemd/user` (the overrides droidc writes) and `systemd/system` are kept.
fn clear_stale_manager_state(runtime: &std::path::Path) {
    let base = runtime.join("systemd");
    for d in [
        "generator",
        "generator.early",
        "generator.late",
        "transient",
    ] {
        let _ = fs::remove_dir_all(base.join(d));
    }
}

/// Why `ld.so` might not be able to preload the shim: it only says "cannot open shared object file".
/// Opens it the same way and reports the OS error, with the process's open descriptors and their limit
/// (a leak across sessions would show here). Printed only when the open fails, or under `CGSHIM_TRACE`.
fn explain_shim_access(shim: &std::path::Path) {
    let opened = fs::File::open(shim);
    if opened.is_ok() && std::env::var_os("CGSHIM_TRACE").is_none() {
        return;
    }
    let fds = fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0);
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let dir = shim.parent().map(|p| fs::read_dir(p).map(|d| d.count()));
    match opened {
        Ok(_) => eprintln!(
            "note: shim {} opens (fds={fds}, nofile={:?})",
            shim.display(),
            limit.current
        ),
        Err(e) => eprintln!(
            "warn: cannot open the shim {}: {e} (fds={fds}, nofile={:?}, dir entries={dir:?})",
            shim.display(),
            limit.current
        ),
    }
}

/// Runs `prog args` with the shim preloaded, `LD_DEBUG=files` and `LD_SHOW_AUXV=1`, the way `systemd --user` is
/// started, and prints what `ld.so` says plus the auxiliary vector lines that decide "secure execution"
/// (`AT_SECURE`, the uids), to see why a preload fails for one program and not another. Up to `tries` times,
/// stopping at the first clean run. Only under `CGSHIM_TRACE`.
fn probe_preload(shim: &std::path::Path, prog: &str, args: &[&str], tries: u32) {
    if std::env::var_os("CGSHIM_TRACE").is_none() {
        return;
    }
    for n in 1..=tries {
        let out = Command::new(prog)
            .args(args)
            .env("LD_PRELOAD", shim)
            .env("LD_DEBUG", "files")
            .env("LD_SHOW_AUXV", "1")
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stderr);
                let failed = text.contains("cannot be preloaded");
                let shown: String = text
                    .lines()
                    .filter(|l| {
                        failed
                            || [
                                "AT_SECURE",
                                "AT_UID",
                                "AT_EUID",
                                "AT_GID",
                                "AT_EGID",
                                "cgshim",
                            ]
                            .iter()
                            .any(|k| l.contains(k))
                    })
                    .map(str::trim)
                    .take(14)
                    .collect::<Vec<_>>()
                    .join(" | ");
                eprintln!(
                    "note: preload probe {prog} {n}/{tries}: {} {shown}",
                    if failed { "FAILED" } else { "ok" }
                );
                if !failed {
                    return;
                }
            }
            Err(e) => {
                eprintln!("note: preload probe {prog} {n}/{tries}: could not run: {e}");
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Unit files the user manager reads from `$XDG_RUNTIME_DIR/systemd/user`, which outrank the image's own
/// (`/usr/lib/systemd/user`). Images that ship `dbus-broker` as the bus (Bluefin, CentOS bootc) also ship
/// `dbus-daemon`; `dbus-broker-launch` exits `ENOENT` (launcher.c +1345) here on every start, so with
/// `dbus-daemon` available both names of the bus unit are made aliases of `dbus-daemon.service`.
/// Returns whether the override was written.
fn prefer_dbus_daemon(runtime: &std::path::Path, unit_dir: &std::path::Path) -> bool {
    let daemon_unit = unit_dir.join("dbus-daemon.service");
    if !daemon_unit.is_file() || find_in_path("dbus-daemon").is_none() {
        return false;
    }
    let dest = runtime.join("systemd/user");
    if fs::create_dir_all(&dest).is_err() {
        return false;
    }
    let link = |name: &str, target: &std::path::Path| {
        let at = dest.join(name);
        let _ = fs::remove_file(&at);
        std::os::unix::fs::symlink(target, &at).is_ok()
    };
    // Both names: the image's dbus.socket says `Service=dbus-broker.service`, so masking that one makes
    // the socket refuse to start. A symlink to a unit file of another name is an alias of it.
    link("dbus.service", &daemon_unit) && link("dbus-broker.service", &daemon_unit)
}

/// The image's own units for the session services droidc runs itself. The graphical session's
/// `services` (PipeWire and WirePlumber for Selkie) are started by droidc with the environment the host
/// compositor arrangement needs: PipeWire's access module runs a Flatpak check through
/// `/proc/<pid>/root`, which Android hides, and restricts every client to nothing unless it uses the
/// manager socket. The image's socket-activated `pipewire.service` does not give that (Mutter's
/// screen-cast stream never appears), so under `systemd --user` these units are masked and droidc's
/// copies run, exactly as without systemd. `pipewire-pulse` needs `pipewire`, so it goes with it.
fn units_replaced_by_services(services: &[Vec<String>]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |base: &str| {
        for suffix in ["service", "socket"] {
            let unit = format!("{base}.{suffix}");
            if !names.contains(&unit) {
                names.push(unit);
            }
        }
    };
    for svc in services {
        let Some(base) = svc
            .first()
            .and_then(|a| std::path::Path::new(a).file_name())
            .and_then(|n| n.to_str())
        else {
            continue;
        };
        add(base);
        if base == "pipewire" {
            add("pipewire-pulse");
        }
    }
    names
}

/// Mask `units` for the user manager (`$XDG_RUNTIME_DIR/systemd/user/<unit>` -> `/dev/null`).
/// Returns how many were masked.
fn mask_units(runtime: &std::path::Path, units: &[String]) -> usize {
    let dest = runtime.join("systemd/user");
    if fs::create_dir_all(&dest).is_err() {
        return 0;
    }
    units
        .iter()
        .filter(|u| {
            let at = dest.join(u);
            let _ = fs::remove_file(&at);
            std::os::unix::fs::symlink("/dev/null", &at).is_ok()
        })
        .count()
}

/// Start the user manager with the shim preloaded; its units start the bus and the session. The
/// host display stays out of its environment, like the buses (see `start_dbus`).
fn start_systemd_user(
    bin: &std::path::Path,
    shim: &std::path::Path,
    dir: &std::path::Path,
) -> Result<Child> {
    let preload = match std::env::var("LD_PRELOAD") {
        Ok(p) if !p.is_empty() => format!("{}:{p}", shim.display()),
        _ => shim.display().to_string(),
    };
    let mut cmd = Command::new(bin);
    // Like the log level, the target only sticks as an argument: the stand-in journal socket would
    // otherwise swallow the manager's own messages.
    let target = if crate::journal::print_enabled() {
        "journal"
    } else {
        "console"
    };
    cmd.args(["--user", &format!("--log-target={target}")]);
    // Without journald the units' output is discarded; `DROIDC_SYSTEMD_STDIO=inherit` sends it to our
    // stdout instead (CI logs, debugging).
    if std::env::var("DROIDC_SYSTEMD_STDIO").as_deref() == Ok("inherit") {
        cmd.args([
            "--default-standard-output=inherit",
            "--default-standard-error=inherit",
        ]);
    }
    if let Ok(level) = std::env::var("DROIDC_SYSTEMD_LOG_LEVEL") {
        // The manager ignores SYSTEMD_LOG_LEVEL for its own messages; only the argument counts.
        cmd.arg(format!("--log-level={level}"));
    }
    // Under CGSHIM_TRACE every process the manager starts writes the dynamic loader's trace to
    // /tmp/lddebug.<pid>, to see which one cannot preload the shim and why (the services inherit these).
    if std::env::var_os("CGSHIM_TRACE").is_some() {
        cmd.env("LD_DEBUG", "files,libs")
            .env("LD_DEBUG_OUTPUT", "/tmp/lddebug");
    }
    cmd.env("LD_PRELOAD", preload)
        .env("XDG_RUNTIME_DIR", dir)
        .env("SYSTEMD_LOG_TARGET", target)
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .spawn()
        .context(bin.display())
}

/// Wait until something accepts connections on `socket`.
fn wait_for_socket(socket: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[derive(Debug)]
pub struct SessionOpts {
    /// droidc root (inside the guest `DROIDC_ROOT`, i.e. `/run/droidc/root`).
    pub root: PathBuf,
    pub params: session::Params,
}

pub const SYSTEM_BUS_SOCKET: &str = "/run/dbus/system_bus_socket";

/// A system bus for the session. Nothing system-wide runs on it, so most names are unknown, but
/// clients that need a connection get one: gnome-shell 50 aborts at startup without it.
fn start_system_bus() -> Option<Child> {
    start_bus(
        std::path::Path::new(SYSTEM_BUS_SOCKET),
        "system",
        "DBUS_SYSTEM_BUS_ADDRESS",
    )
}

/// Orphaned descendants (double-forked daemons) are re-parented to the nearest subreaper, else to
/// the host's init, where we would never reap them or see them exit. Best effort.
fn become_subreaper() {
    if let Err(e) = rustix::process::set_child_subreaper(Some(rustix::process::getpid())) {
        eprintln!("warn: could not become a child subreaper: {e}");
    }
}

/// `droidc login1` (this binary; `DROIDC_LOGIN1` replaces it in tests): the logind stand-in on the
/// system bus. It leaves a real logind in charge if one owns the name.
fn start_login1() -> Option<Child> {
    let argv = match std::env::var("DROIDC_LOGIN1") {
        Ok(b) => vec![b],
        Err(_) => vec![
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "droidc".into()),
            "login1".into(),
        ],
    };
    match spawn(&argv, &[]) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("warn: login1: {e}");
            None
        }
    }
}

/// The system bus (unless one is given) and the logind stand-in on it.
fn start_system_services() -> Vec<Child> {
    let mut out = Vec::new();
    if std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").is_none() {
        out.extend(start_system_bus());
    }
    // logind answers on that bus; without one (a dbus-broker-only image has no dbus-daemon to run a
    // system bus on, and dbus-broker's system scope needs systemd and root) there is nothing to serve.
    if std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").is_some() {
        out.extend(start_login1());
    }
    out
}

pub fn supervise(cmd: &[String]) -> Result<i32> {
    supervise_with(cmd, &Opts::default())
}

pub fn supervise_with(cmd: &[String], opts: &Opts) -> Result<i32> {
    let spec = match &opts.session {
        Some(so) => Some(session::resolve(&so.root)?.map(|(s, _)| s).ok_or_else(|| {
            Error::not_found(
                "no graphical session: the image sets no containers.droidc.session.* labels \
                         and there is no override (droidc session set)",
            )
        })?),
        None => None,
    };
    if spec.is_none() && cmd.is_empty() {
        return Err(Error::invalid("supervise needs a command after --"));
    }
    become_subreaper();
    let dir = runtime_dir()?;
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    if let Err(e) = ensure_machine_id(std::path::Path::new(MACHINE_ID)) {
        eprintln!("warn: could not initialise {MACHINE_ID}: {e}");
    }
    let shim = resolve_systemd(
        opts.systemd,
        std::path::Path::new(SYSTEMD),
        &std::env::var_os("DROIDC_CGSHIM")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SHIM)),
    )?;
    // Before the buses start: activated services inherit the bus's environment.
    if spec.is_some() || shim.is_some() {
        // The one session/seat the login1 stand-in serves; mutter and libsystemd look them up by id.
        std::env::set_var("XDG_SESSION_ID", "1");
        std::env::set_var("XDG_SEAT", "seat0");
    }
    if let Some(spec) = &spec {
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
        if let Some(n) = &spec.name {
            std::env::set_var("XDG_CURRENT_DESKTOP", n);
            std::env::set_var("XDG_SESSION_DESKTOP", n.to_lowercase());
        }
    }
    // Long-running helpers (buses, session services), stopped when the session ends.
    let mut background: Vec<Child> = Vec::new();
    if shim.is_some() {
        // What the user manager's units expect on the system bus: a bus, and logind answering on it
        // (`systemd --user` itself logs "Failed to connect to system bus" without one).
        background.extend(start_system_services());
    }
    let mut user_manager = false;
    if let Some(shim) = &shim {
        crate::journal::start(true);
        // A desktop closed and reopened leaves the previous session's bus and generated units behind.
        explain_shim_access(shim);
        probe_preload(shim, "true", &[], 2);
        probe_preload(shim, SYSTEMD, &["--version"], 3);
        if !retire_stale_socket(&dir.join("bus"), Duration::from_secs(5)) {
            eprintln!(
                "warn: something still listens on {}",
                dir.join("bus").display()
            );
        }
        clear_stale_manager_state(&dir);
        let started = prepare_systemd(
            std::path::Path::new("/run"),
            std::path::Path::new(CGROUP_ROOT),
        )
        .and_then(|fake| {
            if fake {
                // Before the manager starts: it consults the tree from its first unit on.
                let me = std::process::id() as i32;
                std::thread::spawn(move || {
                    crate::cgroups::Cgroups::new(
                        CGROUP_ROOT,
                        crate::cgroups::Scope::Descendants(me),
                    )
                    .run(Duration::from_millis(200))
                });
            }
            if !prefer_dbus_daemon(&dir, std::path::Path::new(USER_UNITS)) {
                eprintln!("warn: no dbus-daemon unit; the image's own bus unit is used");
            }
            if let Some(spec) = &spec {
                mask_units(&dir, &units_replaced_by_services(&spec.services));
            }
            start_systemd_user(std::path::Path::new(SYSTEMD), shim, &dir)
        });
        match started {
            Ok(child) => {
                background.push(child);
                user_manager = true;
                // The image's `dbus.socket` user unit listens here; real systemd activates the bus.
                let bus = dir.join("bus");
                if !wait_for_socket(&bus, Duration::from_secs(30)) {
                    eprintln!("warn: systemd --user did not open {} in 30s", bus.display());
                }
                if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
                    std::env::set_var(
                        "DBUS_SESSION_BUS_ADDRESS",
                        format!("unix:path={}", bus.display()),
                    );
                }
            }
            Err(e) if opts.systemd == SystemdMode::On => return Err(e),
            Err(e) => eprintln!("warn: systemd --user not started, using dbus-daemon: {e}"),
        }
    }
    if !user_manager && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        background.extend(start_session_bus(&dir));
    }
    // The first of these to exit ends the supervisor; the first one's status wins ties.
    let critical = match (&spec, &opts.session) {
        (Some(spec), Some(so)) => {
            start_session(spec, &so.params, &dir, cmd, user_manager, &mut background)?
        }
        _ => vec![spawn(cmd, &[])?],
    };
    let code = wait_for_any(&critical)?;
    let mut all: Vec<u32> = critical.iter().map(Child::id).collect();
    all.extend(background.iter().map(Child::id));
    stop_all(&all, Duration::from_secs(3));
    Ok(code)
}

/// Spawns `argv` with `env` changes (`None` removes the variable).
fn spawn(argv: &[String], env: &[(&str, Option<&str>)]) -> Result<Child> {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    for (k, v) in env {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    c.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::not_found(format!("{}: command not found", argv[0]))
        } else {
            Error::internal(format!("{}: {e}", argv[0]))
        }
    })
}

/// Starts the graphical session described by `spec`; returns the critical children, the one whose
/// exit status the supervisor reports first.
fn start_session(
    spec: &SessionSpec,
    params: &session::Params,
    dir: &std::path::Path,
    cmd: &[String],
    user_manager: bool,
    background: &mut Vec<Child>,
) -> Result<Vec<Child>> {
    // The host compositor (Selkie) listens on $XDG_RUNTIME_DIR/<display>.
    let host_display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| params.wayland_display.clone());
    if !dir.join(&host_display).exists() {
        eprintln!(
            "warn: no host compositor socket at {}",
            dir.join(&host_display).display()
        );
    }
    let params = session::Params {
        wayland_display: host_display.clone(),
        ..params.clone()
    };
    // With the user manager running the buses are already up. The session's own services are still
    // started here: the image's units for them are masked (see `units_replaced_by_services`).
    if !user_manager {
        background.extend(start_system_services());
    }
    let host = [("WAYLAND_DISPLAY", Some(host_display.as_str()))];
    for svc in &spec.services {
        match spawn(&session::fill(svc, &params), &host) {
            Ok(c) => background.push(c),
            Err(e) => eprintln!("warn: session service {}: {e}", svc[0]),
        }
    }
    let exec = session::fill(&spec.exec, &params);
    announce_session_display(dir, &host_display);
    Ok(match spec.renderer {
        Renderer::Wayland => {
            let session = spawn(&exec, &host)?;
            if cmd.is_empty() {
                vec![session]
            } else {
                vec![spawn(cmd, &host)?, session]
            }
        }
        Renderer::Screencast => {
            // Headless: the session compositor must not try to nest in the host compositor.
            let session = spawn(&exec, &[("WAYLAND_DISPLAY", None)])?;
            let bridge = if cmd.is_empty() {
                // `droidc bridge` (this binary); DROIDC_BRIDGE replaces it in tests.
                let mut argv = match std::env::var("DROIDC_BRIDGE") {
                    Ok(b) => vec![b],
                    Err(_) => vec![
                        std::env::current_exe()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "droidc".into()),
                        "bridge".into(),
                    ],
                };
                argv.extend([
                    "--width".into(),
                    params.width.to_string(),
                    "--height".into(),
                    params.height.to_string(),
                ]);
                argv
            } else {
                cmd.to_vec()
            };
            vec![spawn(&bridge, &host)?, session]
        }
    })
}

/// Waits (in the background) for the session compositor's own Wayland socket and passes it to the
/// session bus's activation environment, so D-Bus-activated apps open in the session rather than on
/// the host compositor. Best effort; gives up after a minute.
fn announce_session_display(dir: &std::path::Path, host_display: &str) {
    let dir = dir.to_path_buf();
    let host = host_display.to_string();
    let existing: Vec<String> = wayland_sockets(&dir);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if let Some(name) = wayland_sockets(&dir)
                .into_iter()
                .find(|n| *n != host && !existing.contains(n))
            {
                let env = std::collections::HashMap::from([("WAYLAND_DISPLAY", name.as_str())]);
                let r = zbus::blocking::Connection::session().and_then(|c| {
                    c.call_method(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        Some("org.freedesktop.DBus"),
                        "UpdateActivationEnvironment",
                        &(env,),
                    )
                });
                if let Err(e) = r {
                    eprintln!("warn: activation environment: {e}");
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

/// Names of the Wayland sockets (`wayland-N`) in `dir`.
fn wayland_sockets(dir: &std::path::Path) -> Vec<String> {
    use std::os::unix::fs::FileTypeExt;
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_socket()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("wayland-"))
                .collect()
        })
        .unwrap_or_default()
}

/// Reaps every child (including orphans re-parented to us) until one of `critical` exits;
/// returns its exit code (128 + signal when killed).
fn wait_for_any(critical: &[Child]) -> Result<i32> {
    let pids: Vec<i32> = critical.iter().map(|c| c.id() as i32).collect();
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::empty()) {
            Ok(Some((p, st))) => {
                let p = p.as_raw_nonzero().get();
                if let Some(i) = pids.iter().position(|&c| c == p) {
                    if i > 0 {
                        eprintln!("droidc supervise: {} exited", critical_name(i));
                    }
                    return Ok(st
                        .exit_status()
                        .or_else(|| st.terminating_signal().map(|s| 128 + s))
                        .unwrap_or(1));
                }
            }
            Ok(None) => {}
            Err(e) if e == rustix::io::Errno::INTR => {}
            Err(e) => return Err(Error::internal(format!("wait: {e}"))),
        }
    }
}

fn critical_name(i: usize) -> &'static str {
    if i == 1 {
        "the session compositor"
    } else {
        "a session process"
    }
}

/// TERM every pid, reap them for up to `grace`, then KILL what is left.
fn stop_all(pids: &[u32], grace: Duration) {
    use rustix::process::{kill_process, waitpid, Pid, Signal, WaitOptions};
    let mut left: Vec<Pid> = pids
        .iter()
        .filter_map(|&p| Pid::from_raw(p as i32))
        .collect();
    for p in &left {
        let _ = kill_process(*p, Signal::TERM);
    }
    let deadline = std::time::Instant::now() + grace;
    while !left.is_empty() && std::time::Instant::now() < deadline {
        left.retain(|p| matches!(waitpid(Some(*p), WaitOptions::NOHANG), Ok(None)));
        std::thread::sleep(Duration::from_millis(50));
    }
    for p in &left {
        let _ = kill_process(*p, Signal::KILL);
        let _ = waitpid(Some(*p), WaitOptions::empty());
    }
}

/// `wait` in [`wait_for_any`] reaps any child of the process, so tests that run a supervisor must not
/// overlap with tests that spawn and wait for their own children.
#[cfg(test)]
pub(crate) static CHILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn child_lock() -> std::sync::MutexGuard<'static, ()> {
        CHILD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn live_bus_is_shared_not_replaced() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bus");
        let _live = UnixListener::bind(&p).unwrap();
        assert!(start_bus(&p, "user", "DROIDC_TEST_SHARED_BUS").is_none());
        assert_eq!(
            std::env::var("DROIDC_TEST_SHARED_BUS").unwrap(),
            format!("unix:path={}", p.display())
        );
        UnixStream::connect(&p).expect("the original listener still owns the socket");
    }

    #[test]
    fn machine_id_is_filled_once() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("machine-id");
        for initial in [
            None,
            Some(""),
            Some("uninitialized\n"),
            Some("0".repeat(32).as_str()),
        ] {
            match initial {
                Some(c) => fs::write(&p, c).unwrap(),
                None => {
                    let _ = fs::remove_file(&p);
                }
            }
            assert!(ensure_machine_id(&p).unwrap());
            let id = fs::read_to_string(&p).unwrap();
            assert!(is_valid_machine_id(&id), "{id:?}");
            assert_eq!(id.len(), 33);
            assert!(!ensure_machine_id(&p).unwrap());
            assert_eq!(fs::read_to_string(&p).unwrap(), id);
        }
    }

    #[test]
    fn broker_gets_socket_activation_env() {
        let _lock = child_lock();
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("out");
        let script = d.path().join("fake-broker");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ -S /dev/fd/3 ]; then s=sock; else s=nosock; fi\necho \"$LISTEN_FDS $LISTEN_PID $$ $LISTEN_FDNAMES $s\" > {}\nsleep 2\n",
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let (addr, mut child) = start_broker(&d.path().join("bus"), &script, &[]).unwrap();
        assert_eq!(addr, format!("unix:path={}/bus", d.path().display()));
        std::thread::sleep(Duration::from_millis(300));
        let got = fs::read_to_string(&out).unwrap();
        let f: Vec<&str> = got.split_whitespace().collect();
        assert_eq!(f[0], "1");
        assert_eq!(f[1], f[2], "LISTEN_PID must be the broker's pid");
        assert_eq!(f[3], "dbus.socket");
        assert_eq!(f[4], "sock");
        let _ = child.kill();
        let _ = child.wait();
        // a broker that exits immediately is reported as a failure
        let bad = d.path().join("bad");
        fs::write(&bad, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(start_broker(&d.path().join("bus"), &bad, &[]).is_err());
    }

    #[test]
    fn systemd_mode_resolution() {
        let d = tempfile::tempdir().unwrap();
        let (sd, shim) = (d.path().join("systemd"), d.path().join("cgshim.so"));
        let none = |m| resolve_systemd(m, &sd, &shim).unwrap();
        assert_eq!(none(SystemdMode::Off), None);
        assert_eq!(none(SystemdMode::Auto), None);
        assert_eq!(
            resolve_systemd(SystemdMode::On, &sd, &shim)
                .unwrap_err()
                .kind,
            crate::error::ErrorKind::NotFound
        );
        fs::write(&sd, "").unwrap();
        assert_eq!(none(SystemdMode::Auto), None, "systemd alone is not enough");
        fs::write(&shim, "").unwrap();
        assert_eq!(none(SystemdMode::Off), None);
        assert_eq!(none(SystemdMode::Auto), Some(shim.clone()));
        assert_eq!(none(SystemdMode::On), Some(shim));
    }

    #[test]
    fn systemd_prerequisites_are_created_once() {
        let d = tempfile::tempdir().unwrap();
        let (run, cg) = (d.path().join("run"), d.path().join("cgroup"));
        assert!(prepare_systemd(&run, &cg).unwrap());
        assert!(run.join("systemd/system").is_dir());
        for f in CGROUP_ROOT_FILES {
            assert!(cg.join(f).is_file(), "{f}");
        }
        // existing content is not clobbered
        fs::write(cg.join("cgroup.events"), "populated 0\n").unwrap();
        prepare_systemd(&run, &cg).unwrap();
        assert_eq!(
            fs::read_to_string(cg.join("cgroup.events")).unwrap(),
            "populated 0\n"
        );
    }

    #[test]
    fn stale_sockets_are_retired_and_live_ones_are_not() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("bus");
        // nothing there: fine
        assert!(retire_stale_socket(&sock, Duration::from_millis(200)));
        // a socket file nobody listens on (the listener was dropped): removed
        drop(UnixListener::bind(&sock).unwrap());
        assert!(sock.exists());
        assert!(retire_stale_socket(&sock, Duration::from_millis(200)));
        assert!(!sock.exists());
        // a live listener is waited on and then left alone
        let live = UnixListener::bind(&sock).unwrap();
        assert!(!retire_stale_socket(&sock, Duration::from_millis(300)));
        assert!(sock.exists());
        // ... and is retired once it goes away while we wait
        let t = std::thread::spawn({
            let sock = sock.clone();
            move || retire_stale_socket(&sock, Duration::from_secs(5))
        });
        std::thread::sleep(Duration::from_millis(300));
        drop(live);
        assert!(t.join().unwrap());
        assert!(!sock.exists());
    }

    #[test]
    fn stale_manager_state_is_cleared_but_the_overrides_stay() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("systemd");
        for sub in ["generator.late", "generator", "transient", "user", "system"] {
            fs::create_dir_all(base.join(sub)).unwrap();
            fs::write(base.join(sub).join("x"), "").unwrap();
        }
        clear_stale_manager_state(d.path());
        for gone in ["generator.late", "generator", "transient"] {
            assert!(!base.join(gone).exists(), "{gone}");
        }
        for kept in ["user", "system"] {
            assert!(base.join(kept).join("x").exists(), "{kept}");
        }
    }

    #[test]
    fn session_services_replace_the_images_units() {
        let svcs = vec![
            vec!["/usr/bin/pipewire".to_string(), "-c".into(), "x".into()],
            vec!["wireplumber".to_string()],
            vec!["/usr/bin/pipewire".to_string()],
            vec![],
        ];
        assert_eq!(
            units_replaced_by_services(&svcs),
            [
                "pipewire.service",
                "pipewire.socket",
                "pipewire-pulse.service",
                "pipewire-pulse.socket",
                "wireplumber.service",
                "wireplumber.socket",
            ]
        );
        assert!(units_replaced_by_services(&[]).is_empty());
        let d = tempfile::tempdir().unwrap();
        let names = units_replaced_by_services(&svcs);
        assert_eq!(mask_units(d.path(), &names), 6);
        assert_eq!(
            fs::read_link(d.path().join("systemd/user/pipewire.socket")).unwrap(),
            std::path::PathBuf::from("/dev/null")
        );
        assert_eq!(mask_units(d.path(), &names), 6, "idempotent");
    }

    #[test]
    fn dbus_daemon_replaces_the_broker_unit_when_present() {
        let d = tempfile::tempdir().unwrap();
        let (run, units) = (d.path().join("run"), d.path().join("units"));
        fs::create_dir_all(&units).unwrap();
        // no dbus-daemon unit: nothing is written
        assert!(!prefer_dbus_daemon(&run, &units));
        assert!(!run.join("systemd/user").exists());
        fs::write(units.join("dbus-daemon.service"), "").unwrap();
        if find_in_path("dbus-daemon").is_none() {
            assert!(!prefer_dbus_daemon(&run, &units), "needs the binary too");
            return;
        }
        assert!(prefer_dbus_daemon(&run, &units));
        assert!(prefer_dbus_daemon(&run, &units), "idempotent");
        let dest = run.join("systemd/user");
        assert_eq!(
            fs::read_link(dest.join("dbus.service")).unwrap(),
            units.join("dbus-daemon.service")
        );
        assert_eq!(
            fs::read_link(dest.join("dbus-broker.service")).unwrap(),
            units.join("dbus-daemon.service"),
            "dbus.socket names the broker unit, so it must resolve to the daemon"
        );
    }

    #[test]
    fn user_manager_gets_the_shim_and_no_host_display() {
        let _lock = child_lock();
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("out");
        let fake = script(
            d.path(),
            "systemd",
            &format!(
                "echo \"$* $LD_PRELOAD $XDG_RUNTIME_DIR ${{WAYLAND_DISPLAY-unset}}\" > {}; exec sleep 5",
                out.display()
            ),
        );
        std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
        let mut child = start_systemd_user(
            std::path::Path::new(&fake),
            std::path::Path::new("/x/cgshim.so"),
            d.path(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let got = fs::read_to_string(&out).unwrap();
        assert_eq!(
            got.trim(),
            format!(
                "--user --log-target=console /x/cgshim.so {} unset",
                d.path().display()
            )
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn waits_for_a_socket_to_accept() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bus");
        assert!(!wait_for_socket(&p, Duration::from_millis(200)));
        let _l = UnixListener::bind(&p).unwrap();
        assert!(wait_for_socket(&p, Duration::from_millis(200)));
    }

    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn session_opts(root: &std::path::Path, spec: SessionSpec) -> Opts {
        session::write_override(&crate::layout::Root::new(root), &spec).unwrap();
        Opts {
            session: Some(SessionOpts {
                root: root.to_path_buf(),
                params: session::Params {
                    width: 640,
                    height: 480,
                    ..Default::default()
                },
            }),
            ..Default::default()
        }
    }

    /// One test: `wait` reaps any child of the process, so supervisor runs must not overlap.
    #[test]
    fn graphical_sessions() {
        let _lock = child_lock();
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        fs::create_dir_all(&run).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &run);
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent");
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent");
        std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
        let o = |n: &str| d.path().join(n).display().to_string();
        let svc = script(
            d.path(),
            "svc",
            &format!("echo up > {}; exec sleep 30", o("svc.out")),
        );
        let comp = script(
            d.path(),
            "comp",
            &format!(
                "echo \"$* ${{WAYLAND_DISPLAY:-unset}} $XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP\" > {}; exec sleep 30",
                o("comp.out")
            ),
        );

        // nested: the session compositor gets the host display; a given command ends the session
        let spec = SessionSpec {
            name: Some("Test".into()),
            renderer: Renderer::Wayland,
            exec: vec![
                comp.clone(),
                "--display={wayland_display}".into(),
                "{width}x{height}".into(),
            ],
            services: vec![vec![svc.clone()]],
        };
        let opts = session_opts(d.path(), spec.clone());
        let t0 = std::time::Instant::now();
        let code = supervise_with(
            &["sh".into(), "-c".into(), "sleep 0.5; exit 5".into()],
            &opts,
        )
        .unwrap();
        assert_eq!(code, 5);
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "children must be stopped, not waited for"
        );
        assert_eq!(
            fs::read_to_string(o("comp.out")).unwrap().trim(),
            "--display=wayland-0 640x480 wayland-0 wayland Test"
        );
        assert_eq!(fs::read_to_string(o("svc.out")).unwrap().trim(), "up");

        // nested, no command: the compositor's own exit ends the session
        let quick = script(d.path(), "quick", "exit 4");
        let opts = session_opts(
            d.path(),
            SessionSpec {
                exec: vec![quick.clone()],
                ..spec.clone()
            },
        );
        assert_eq!(supervise_with(&[], &opts).unwrap(), 4);

        // screencast: compositor without WAYLAND_DISPLAY, then the bridge with the host display
        let bridge = script(
            d.path(),
            "bridge",
            &format!(
                "echo \"$* $WAYLAND_DISPLAY\" > {}; sleep 0.5; exit 3",
                o("bridge.out")
            ),
        );
        std::env::set_var("DROIDC_BRIDGE", &bridge);
        // the real login1 would run the test binary; a stub that records its environment instead
        std::env::set_var(
            "DROIDC_LOGIN1",
            script(
                d.path(),
                "login1",
                &format!(
                    "echo \"$XDG_SESSION_ID $XDG_SEAT\" > {}; exec sleep 30",
                    o("login1.out")
                ),
            ),
        );
        let opts = session_opts(
            d.path(),
            SessionSpec {
                renderer: Renderer::Screencast,
                services: vec![],
                ..spec.clone()
            },
        );
        assert_eq!(supervise_with(&[], &opts).unwrap(), 3);
        assert_eq!(
            fs::read_to_string(o("bridge.out")).unwrap().trim(),
            "--width 640 --height 480 wayland-0"
        );
        assert!(fs::read_to_string(o("comp.out"))
            .unwrap()
            .contains(" unset wayland Test"));
        assert_eq!(
            fs::read_to_string(o("login1.out")).unwrap().trim(),
            "1 seat0"
        );
        // a given command replaces the bridge
        let opts = session_opts(
            d.path(),
            SessionSpec {
                renderer: Renderer::Screencast,
                services: vec![],
                ..spec.clone()
            },
        );
        assert_eq!(
            supervise_with(&["sh".into(), "-c".into(), "exit 9".into()], &opts).unwrap(),
            9
        );
        std::env::remove_var("DROIDC_BRIDGE");
        std::env::remove_var("DROIDC_LOGIN1");

        // --session without a configured session is not-found
        let empty = tempfile::tempdir().unwrap();
        let opts = Opts {
            session: Some(SessionOpts {
                root: empty.path().to_path_buf(),
                params: Default::default(),
            }),
            ..Default::default()
        };
        assert_eq!(
            supervise_with(&[], &opts).unwrap_err().kind,
            crate::error::ErrorKind::NotFound
        );

        // plain mode still works
        assert_eq!(
            supervise(&["sh".into(), "-c".into(), "exit 7".into()]).unwrap(),
            7
        );
        assert_eq!(
            supervise(&[
                "sh".into(),
                "-c".into(),
                "test -n \"$XDG_RUNTIME_DIR\"".into()
            ])
            .unwrap(),
            0
        );
        assert_eq!(
            supervise(&["definitely-not-a-command".into()])
                .unwrap_err()
                .kind,
            crate::error::ErrorKind::NotFound
        );
    }
}
