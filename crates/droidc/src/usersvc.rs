//! A container's user services: one session bus (or one `systemd --user`) shared by every supervisor in
//! the container, owned by a keeper process (`droidc user-services`) rather than by whichever supervisor
//! happened to start first.
//!
//! Each supervisor holds a shared `flock` on `$XDG_RUNTIME_DIR/droidc/lease` for as long as it runs. The
//! keeper holds an exclusive lock on `keeper.lock` while it runs, writes `state.json` once its bus answers,
//! and (unless `--stay`) stops its services once nobody has held the lease for the linger time. Locks are
//! released by the kernel when their holder dies, so nothing here depends on pids, which Android reuses
//! freely (a `kill(pid, 0)` check cannot tell a dead keeper from an unrelated app).
//!
//! The order of the locks makes joining race-free: a supervisor takes the lease before it looks for a
//! keeper, and a keeper that is going away holds the lease exclusively until it has let go of
//! `keeper.lock`, so a supervisor either finds a live keeper or none at all, never one about to leave.

use crate::error::{Context, Error, Result};
use crate::supervise::{self, SystemdMode};
use rustix::fs::{flock, FlockOperation};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Under `$XDG_RUNTIME_DIR`.
pub const DIR: &str = "droidc";
/// How long a keeper outlives its last supervisor (`DROIDC_USER_LINGER`, seconds).
const DEFAULT_LINGER: Duration = Duration::from_secs(10);
/// How long a supervisor waits for a keeper to answer.
const START_TIMEOUT: Duration = Duration::from_secs(45);

/// What the keeper publishes in `state.json` once its bus answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub pid: u32,
    /// The bus belongs to the image's `systemd --user`.
    pub systemd: bool,
    /// `DBUS_SESSION_BUS_ADDRESS`.
    pub bus: String,
    /// `DBUS_SYSTEM_BUS_ADDRESS`, when the keeper runs the system bus (with `systemd --user`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_bus: Option<String>,
}

struct Paths {
    lease: PathBuf,
    keeper: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

fn paths(run: &Path) -> Result<Paths> {
    let d = run.join(DIR);
    fs::create_dir_all(&d).context(d.display())?;
    Ok(Paths {
        lease: d.join("lease"),
        keeper: d.join("keeper.lock"),
        state: d.join("state.json"),
        log: d.join("user-services.log"),
    })
}

fn open_lock(p: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
        .context(p.display())
}

/// `flock`, retried on `EINTR`. `Ok(false)` when a non-blocking lock is held elsewhere.
fn lock(f: &File, op: FlockOperation) -> Result<bool> {
    loop {
        match flock(f, op) {
            Ok(()) => return Ok(true),
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(false),
            Err(e) => return Err(Error::internal(format!("flock: {e}"))),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Keeper {
    None,
    Starting,
    Ready(State),
}

fn keeper(p: &Paths) -> Result<Keeper> {
    let f = open_lock(&p.keeper)?;
    if lock(&f, FlockOperation::NonBlockingLockShared)? {
        // Nobody holds it exclusively: no keeper. Dropping `f` lets go of our probe.
        return Ok(Keeper::None);
    }
    Ok(
        match fs::read(&p.state)
            .ok()
            .and_then(|b| serde_json::from_slice::<State>(&b).ok())
        {
            Some(s) => Keeper::Ready(s),
            None => Keeper::Starting,
        },
    )
}

/// Whether a bus at `addr` (a D-Bus address) accepts connections. Only `unix:path=` is checked; any
/// other transport is taken on trust.
pub fn bus_alive(addr: &str) -> bool {
    match unix_path(addr) {
        Some(p) => std::os::unix::net::UnixStream::connect(p).is_ok(),
        None => true,
    }
}

fn unix_path(addr: &str) -> Option<PathBuf> {
    let rest = addr.split(';').next()?.strip_prefix("unix:")?;
    rest.split(',')
        .find_map(|kv| kv.strip_prefix("path="))
        .map(PathBuf::from)
}

/// A supervisor's hold on its container's user services. The services stay up while it is held.
#[derive(Debug)]
pub struct Lease {
    _file: File,
    pub state: State,
}

/// `droidc user-services` (this binary; `DROIDC_USER_SERVICES` replaces it in tests).
fn keeper_argv(mode: SystemdMode) -> Vec<String> {
    let mut argv = match std::env::var("DROIDC_USER_SERVICES") {
        Ok(b) if !b.is_empty() => vec![b],
        _ => vec![std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "droidc".into())],
    };
    argv.push("user-services".into());
    argv.push(format!(
        "--systemd={}",
        match mode {
            SystemdMode::Off => "off",
            SystemdMode::Auto => "auto",
            SystemdMode::On => "on",
        }
    ));
    argv
}

/// Starts a keeper in its own session (a terminal's hangup must not reach it), with its output in
/// `user-services.log` rather than on our stdio, which belongs to the command and may close first.
/// Under proot the keeper keeps the starting proot running until it stops (proot waits for every
/// process it traces), so a host that runs several proots per container starts the keeper in one of
/// its own (`droidc user-services --stay`) and sets `DROIDC_USER_SERVICES_WAIT` for its sessions.
fn spawn_keeper(p: &Paths, mode: SystemdMode) -> Result<std::process::Child> {
    let argv = keeper_argv(mode);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null());
    // `DROIDC_SYSTEMD_STDIO=inherit` (CI logs, debugging) keeps the services' output on ours.
    if std::env::var("DROIDC_SYSTEMD_STDIO").as_deref() != Ok("inherit") {
        let log = File::create(&p.log).context(p.log.display())?;
        cmd.stdout(log.try_clone().context(p.log.display())?)
            .stderr(log);
    }
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            let _ = rustix::process::setsid();
            Ok(())
        });
    }
    cmd.spawn().context(&argv[0])
}

fn wait_secs(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(default)
}

/// Joins the container's user services, starting a keeper when none runs. A host that starts the keeper
/// itself (Selkie runs `droidc user-services` as its own process) sets `DROIDC_USER_SERVICES_WAIT` to how
/// long to wait for it before starting one here.
pub fn join(run: &Path, mode: SystemdMode) -> Result<Lease> {
    let p = paths(run)?;
    let file = open_lock(&p.lease)?;
    lock(&file, FlockOperation::LockShared)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let host_wait = Instant::now() + wait_secs("DROIDC_USER_SERVICES_WAIT", Duration::ZERO);
    let mut ours: Option<std::process::Child> = None;
    loop {
        match keeper(&p)? {
            Keeper::Ready(state) if bus_alive(&state.bus) => {
                if let Some(mut c) = ours {
                    // It runs on after we go (reaped by whoever inherits it); don't leave a zombie meanwhile.
                    std::thread::spawn(move || c.wait());
                }
                return Ok(Lease { _file: file, state });
            }
            Keeper::None if ours.is_none() && Instant::now() >= host_wait => {
                eprintln!("note: starting the container's user services");
                ours = Some(spawn_keeper(&p, mode)?);
            }
            Keeper::None => {
                if let Some(c) = ours.as_mut() {
                    if let Some(st) = c.try_wait().context("user-services")? {
                        let log = fs::read_to_string(&p.log).unwrap_or_default();
                        return Err(Error::internal(format!(
                            "droidc user-services exited with {st}: {}",
                            log.trim()
                        )));
                    }
                }
            }
            _ => {}
        }
        if Instant::now() > deadline {
            return Err(Error::internal(format!(
                "the container's user services did not start in {}s (see {})",
                START_TIMEOUT.as_secs(),
                p.log.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The supervisor's side: use an inherited bus that answers, else join the container's user services
/// and export their addresses. `None` when the inherited bus is used.
pub fn attach(run: &Path, mode: SystemdMode) -> Result<Option<Lease>> {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        if bus_alive(&addr) {
            return Ok(None);
        }
        eprintln!(
            "warn: DBUS_SESSION_BUS_ADDRESS={addr} does not answer; using the container's bus"
        );
        std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
    }
    let lease = join(run, mode)?;
    let st = &lease.state;
    if mode == SystemdMode::On && !st.systemd {
        return Err(Error::invalid(
            "--systemd=on, but this container's user services already run without systemd \
             (started by a supervisor with --systemd=off); end the container's sessions first",
        ));
    }
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &st.bus);
    if let Some(sys) = &st.system_bus {
        if std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").is_none() {
            std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", sys);
        }
    }
    Ok(Some(lease))
}

/// `droidc user-services`: start the container's user services and keep them until no supervisor holds
/// the lease for the linger time (never with `stay`, where the host stops us). A bus or user manager
/// that exits is started again (with backoff), so clients that reconnect find it at the same address.
pub fn run(mode: SystemdMode, root: Option<&crate::layout::Root>, stay: bool) -> Result<i32> {
    supervise::become_subreaper();
    let run = supervise::runtime_dir()?;
    std::env::set_var("XDG_RUNTIME_DIR", &run);
    let p = paths(&run)?;
    let keeper_lock = open_lock(&p.keeper)?;
    // A supervisor probing for a keeper holds a shared lock for an instant; a keeper holds it for good.
    let deadline = Instant::now() + Duration::from_secs(1);
    while !lock(&keeper_lock, FlockOperation::NonBlockingLockExclusive)? {
        if Instant::now() > deadline {
            eprintln!("note: the container's user services already run");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = fs::remove_file(&p.state);
    // The session's own units are masked from the start (see `units_replaced_by_services`): the image's
    // session is a property of the deployment, whichever supervisor comes first.
    let spec = root.and_then(|r| crate::session::resolve(r).ok().flatten().map(|(s, _)| s));
    let mut svc = supervise::start_user_services(&run, mode, spec.as_ref())?;
    let state = State {
        pid: std::process::id(),
        systemd: svc.systemd,
        bus: svc.bus.clone(),
        system_bus: svc.system_bus.clone(),
    };
    let tmp = p.state.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(&state).unwrap_or_default()).context(tmp.display())?;
    fs::rename(&tmp, &p.state).context(p.state.display())?;
    eprintln!(
        "droidc user-services: bus {} ({})",
        state.bus,
        if state.systemd {
            "systemd --user"
        } else {
            "dbus-daemon"
        }
    );

    let lease = open_lock(&p.lease)?;
    let linger = wait_secs("DROIDC_USER_LINGER", DEFAULT_LINGER);
    let mut idle_since: Option<Instant> = None;
    let mut backoff = Duration::from_secs(1);
    let mut restart_at: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        // Reap everything (orphans come to us as a subreaper); notice the bus owner going.
        loop {
            match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
                Ok(Some((pid, st))) => {
                    let pid = pid.as_raw_nonzero().get() as u32;
                    if svc.reaped(pid) {
                        eprintln!(
                            "warn: the container's {} exited ({st:?}); starting it again",
                            if svc.systemd {
                                "systemd --user"
                            } else {
                                "session bus"
                            }
                        );
                        restart_at = Some(Instant::now() + backoff);
                    }
                }
                Ok(None) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => break, // ECHILD: nothing left to reap
            }
        }
        if let Some(at) = restart_at {
            if Instant::now() >= at {
                match svc.restart_owner(&run) {
                    Ok(()) => {
                        restart_at = None;
                        backoff = Duration::from_secs(1);
                    }
                    Err(e) => {
                        eprintln!("warn: restarting the bus: {e}");
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                        restart_at = Some(Instant::now() + backoff);
                    }
                }
            }
        }
        if stay {
            continue;
        }
        if lock(&lease, FlockOperation::NonBlockingLockExclusive)? {
            // No supervisor. Keep the lease while deciding so none can join a keeper about to leave.
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= linger {
                eprintln!("droidc user-services: no sessions left; stopping");
                let _ = fs::remove_file(&p.state);
                svc.stop();
                // keeper.lock first, then the lease: a supervisor waiting on the lease then finds no keeper.
                drop(keeper_lock);
                drop(lease);
                return Ok(0);
            }
            lock(&lease, FlockOperation::Unlock)?;
        } else {
            idle_since = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_paths_are_read_from_addresses() {
        assert_eq!(
            unix_path("unix:path=/run/user/1/bus"),
            Some(PathBuf::from("/run/user/1/bus"))
        );
        assert_eq!(
            unix_path("unix:path=/run/user/1/bus,guid=abc"),
            Some(PathBuf::from("/run/user/1/bus"))
        );
        assert_eq!(unix_path("unix:abstract=/tmp/dbus-x"), None);
        assert!(bus_alive("tcp:host=localhost,port=1"));
        assert!(!bus_alive("unix:path=/nonexistent/bus"));
    }

    #[test]
    fn a_keeper_is_seen_by_its_lock_then_its_state() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path()).unwrap();
        assert_eq!(keeper(&p).unwrap(), Keeper::None);
        let held = open_lock(&p.keeper).unwrap();
        assert!(lock(&held, FlockOperation::NonBlockingLockExclusive).unwrap());
        assert_eq!(keeper(&p).unwrap(), Keeper::Starting);
        let st = State {
            pid: 1,
            systemd: false,
            bus: "unix:path=/x".into(),
            system_bus: None,
        };
        fs::write(&p.state, serde_json::to_vec(&st).unwrap()).unwrap();
        assert_eq!(keeper(&p).unwrap(), Keeper::Ready(st));
        drop(held);
        assert_eq!(
            keeper(&p).unwrap(),
            Keeper::None,
            "a stale state file is no keeper"
        );
    }

    #[test]
    fn supervisors_share_the_lease_and_block_a_leaving_keeper() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path()).unwrap();
        let a = open_lock(&p.lease).unwrap();
        let b = open_lock(&p.lease).unwrap();
        assert!(lock(&a, FlockOperation::NonBlockingLockShared).unwrap());
        assert!(lock(&b, FlockOperation::NonBlockingLockShared).unwrap());
        let k = open_lock(&p.lease).unwrap();
        assert!(!lock(&k, FlockOperation::NonBlockingLockExclusive).unwrap());
        drop(a);
        assert!(!lock(&k, FlockOperation::NonBlockingLockExclusive).unwrap());
        drop(b);
        assert!(lock(&k, FlockOperation::NonBlockingLockExclusive).unwrap());
        let late = open_lock(&p.lease).unwrap();
        assert!(
            !lock(&late, FlockOperation::NonBlockingLockShared).unwrap(),
            "a supervisor waits while the keeper decides"
        );
    }

    #[test]
    fn keeper_command_carries_the_mode() {
        std::env::set_var("DROIDC_USER_SERVICES", "/bin/stub");
        assert_eq!(
            keeper_argv(SystemdMode::Auto),
            vec!["/bin/stub", "user-services", "--systemd=auto"]
        );
        std::env::remove_var("DROIDC_USER_SERVICES");
    }
}
