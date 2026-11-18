//! `droidc supervise [--session] -- cmd...`: the in-guest session supervisor (no systemd).
//!
//! Makes sure `XDG_RUNTIME_DIR` exists (0700), starts a D-Bus session bus and exports
//! `DBUS_SESSION_BUS_ADDRESS`, runs `cmd` as a child and reaps every child (including orphans
//! re-parented to us) until it exits, then exits with its status.
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
use std::os::unix::net::{UnixDatagram, UnixListener};
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
    let started = if let Some(bin) = find_in_path("dbus-daemon") {
        start_dbus(socket, &bin)
    } else {
        let bin = find_in_path("dbus-broker-launch")?;
        start_journal_sink();
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

const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// `dbus-broker-launch` connects a datagram socket to the journal at startup and exits with ENOENT
/// when `/run/systemd/journal/socket` is missing (no systemd in the guest). Bind a stand-in there
/// and discard what arrives; a detached thread owns it for the life of the process. Best effort:
/// does nothing when something already listens there or `/run` is not writable.
fn start_journal_sink() {
    start_journal_sink_at(std::path::Path::new(JOURNAL_SOCKET));
}

fn start_journal_sink_at(path: &std::path::Path) {
    // A socket file left by a previous session refuses connections; only a live listener is kept.
    if path.exists() {
        let live = UnixDatagram::unbound()
            .and_then(|c| c.connect(path))
            .is_ok();
        if live || fs::remove_file(path).is_err() {
            return;
        }
    }
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let Ok(sock) = UnixDatagram::bind(path) else {
        return;
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while sock.recv(&mut buf).is_ok() {}
    });
}

/// Socket-activate `dbus-broker-launch --scope user` by hand (systemd --user cannot run under
/// proot): bind+listen `<dir>/bus`, pass it as fd 3 with `LISTEN_FDS=1`, `LISTEN_PID=<child pid>`.
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
    let dir = runtime_dir()?;
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    if let Err(e) = ensure_machine_id(std::path::Path::new(MACHINE_ID)) {
        eprintln!("warn: could not initialise {MACHINE_ID}: {e}");
    }
    // Long-running helpers (buses, session services), stopped when the session ends.
    let mut background: Vec<Child> = Vec::new();
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        background.extend(start_session_bus(&dir));
    }
    // The first of these to exit ends the supervisor; the first one's status wins ties.
    let critical = match (&spec, &opts.session) {
        (Some(spec), Some(so)) => start_session(spec, &so.params, &dir, cmd, &mut background)?,
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
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    if let Some(n) = &spec.name {
        std::env::set_var("XDG_CURRENT_DESKTOP", n);
        std::env::set_var("XDG_SESSION_DESKTOP", n.to_lowercase());
    }
    if std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").is_none() {
        background.extend(start_system_bus());
    }
    let host = [("WAYLAND_DISPLAY", Some(host_display.as_str()))];
    for svc in &spec.services {
        match spawn(&session::fill(svc, &params), &host) {
            Ok(c) => background.push(c),
            Err(e) => eprintln!("warn: session service {}: {e}", svc[0]),
        }
    }
    let exec = session::fill(&spec.exec, &params);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_sink_replaces_stale_socket_and_keeps_live_one() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal/socket");
        // stale: bound then dropped, leaving the file behind with no listener
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        drop(UnixDatagram::bind(&p).unwrap());
        assert!(UnixDatagram::unbound().unwrap().connect(&p).is_err());
        start_journal_sink_at(&p);
        let c = UnixDatagram::unbound().unwrap();
        c.connect(&p).expect("sink must accept connections");
        c.send(b"hello").unwrap();
        // live: a second call leaves the working socket alone
        start_journal_sink_at(&p);
        c.send(b"again").unwrap();
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
        }
    }

    /// One test: `wait` reaps any child of the process, so supervisor runs must not overlap.
    #[test]
    fn graphical_sessions() {
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

        // --session without a configured session is not-found
        let empty = tempfile::tempdir().unwrap();
        let opts = Opts {
            session: Some(SessionOpts {
                root: empty.path().to_path_buf(),
                params: Default::default(),
            }),
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
