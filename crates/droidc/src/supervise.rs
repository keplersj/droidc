//! `droidc supervise -- cmd...`: the seed of the in-guest session supervisor.
//!
//! MVP behaviour: make sure `XDG_RUNTIME_DIR` exists (0700), start a D-Bus session bus when
//! `dbus-daemon` is installed in the guest and export `DBUS_SESSION_BUS_ADDRESS`, run `cmd` as a
//! child and reap every child (including orphans re-parented to us) until `cmd` exits, then exit
//! with its status. Later versions add the other session services here.

use crate::error::{Context, Error, Result};
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

/// Start a session bus; returns its address.
fn start_dbus(dir: &std::path::Path, bin: &std::path::Path) -> Result<String> {
    let out = Command::new(bin)
        .arg("--session")
        .arg(format!("--address=unix:path={}/bus", dir.display()))
        .arg("--fork")
        .arg("--print-address")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("dbus-daemon")?;
    if !out.status.success() {
        return Err(Error::internal(format!(
            "dbus-daemon exited with {}",
            out.status
        )));
    }
    let addr = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if addr.is_empty() {
        return Err(Error::internal("dbus-daemon printed no address"));
    }
    // `--fork --print-address` reports the address before the daemon finishes starting; if the
    // daemon then dies (e.g. SELinux AVC init denied) the socket never appears.
    std::thread::sleep(Duration::from_millis(250));
    if !dir.join("bus").exists() {
        return Err(Error::internal(
            "dbus-daemon exited after printing its address",
        ));
    }
    Ok(addr)
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
    dir: &std::path::Path,
    bin: &std::path::Path,
    args: &[&str],
) -> Result<(String, Child)> {
    let path = dir.join("bus");
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).context(path.display())?;
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
        let _ = fs::remove_file(&path);
        return Err(Error::internal(format!(
            "{} exited with {st}",
            bin.display()
        )));
    }
    Ok((format!("unix:path={}", path.display()), child))
}

fn start_session_bus(dir: &std::path::Path) -> Option<Child> {
    if let Some(bin) = find_in_path("dbus-broker-launch") {
        start_journal_sink();
        match start_broker(dir, &bin, &["--scope", "user"]) {
            Ok((addr, child)) => {
                std::env::set_var("DBUS_SESSION_BUS_ADDRESS", addr);
                return Some(child);
            }
            Err(e) => eprintln!("warn: dbus-broker-launch failed: {e}"),
        }
    }
    if let Some(bin) = find_in_path("dbus-daemon") {
        match start_dbus(dir, &bin) {
            Ok(a) => std::env::set_var("DBUS_SESSION_BUS_ADDRESS", a),
            Err(e) => eprintln!("warn: could not start session bus: {e}"),
        }
    }
    None
}

pub fn supervise(cmd: &[String]) -> Result<i32> {
    if cmd.is_empty() {
        return Err(Error::invalid("supervise needs a command after --"));
    }
    let dir = runtime_dir()?;
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    if let Err(e) = ensure_machine_id(std::path::Path::new(MACHINE_ID)) {
        eprintln!("warn: could not initialise {MACHINE_ID}: {e}");
    }
    let broker = if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        start_session_bus(&dir)
    } else {
        None
    };
    let child = Command::new(&cmd[0]).args(&cmd[1..]).spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::not_found(format!("{}: command not found", cmd[0]))
        } else {
            Error::internal(format!("{}: {e}", cmd[0]))
        }
    })?;
    let pid = child.id() as i32;
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::empty()) {
            Ok(Some((p, st))) => {
                if p.as_raw_nonzero().get() == pid {
                    if let Some(b) = &broker {
                        let _ = rustix::process::kill_process(
                            rustix::process::Pid::from_raw(b.id() as i32).unwrap(),
                            rustix::process::Signal::TERM,
                        );
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
        let (addr, mut child) = start_broker(d.path(), &script, &[]).unwrap();
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
        assert!(start_broker(d.path(), &bad, &[]).is_err());
    }

    #[test]
    fn exit_status_is_propagated() {
        std::env::set_var("XDG_RUNTIME_DIR", tempfile::tempdir().unwrap().keep());
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent"); // skip dbus
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
