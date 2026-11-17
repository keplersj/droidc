//! `droidc supervise -- cmd...`: the seed of the in-guest session supervisor.
//!
//! MVP behaviour: make sure `XDG_RUNTIME_DIR` exists (0700), start a D-Bus session bus when
//! `dbus-daemon` is installed in the guest and export `DBUS_SESSION_BUS_ADDRESS`, run `cmd` as a
//! child and reap every child (including orphans re-parented to us) until `cmd` exits, then exit
//! with its status. Later versions add the other session services here.

use crate::error::{Context, Error, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
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
        return Err(Error::internal(format!("dbus-daemon exited with {}", out.status)));
    }
    let addr = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if addr.is_empty() {
        return Err(Error::internal("dbus-daemon printed no address"));
    }
    Ok(addr)
}

/// Socket-activate `dbus-broker-launch --scope user` by hand (systemd --user cannot run under
/// proot): bind+listen `<dir>/bus`, pass it as fd 3 with `LISTEN_FDS=1`, `LISTEN_PID=<child pid>`.
/// Returns the bus address and the running broker.
fn start_broker(dir: &std::path::Path, bin: &std::path::Path, args: &[&str]) -> Result<(String, Child)> {
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
        return Err(Error::internal(format!("{} exited with {st}", bin.display())));
    }
    Ok((format!("unix:path={}", path.display()), child))
}

fn start_session_bus(dir: &std::path::Path) -> Option<Child> {
    if let Some(bin) = find_in_path("dbus-broker-launch") {
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
    let broker = if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() { start_session_bus(&dir) } else { None };
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
                        let _ = rustix::process::kill_process(rustix::process::Pid::from_raw(b.id() as i32).unwrap(), rustix::process::Signal::TERM);
                    }
                    return Ok(st.exit_status().map(|c| c as i32).or_else(|| st.terminating_signal().map(|s| 128 + s as i32)).unwrap_or(1));
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
        assert_eq!(supervise(&["sh".into(), "-c".into(), "exit 7".into()]).unwrap(), 7);
        assert_eq!(supervise(&["sh".into(), "-c".into(), "test -n \"$XDG_RUNTIME_DIR\"".into()]).unwrap(), 0);
        assert_eq!(supervise(&["definitely-not-a-command".into()]).unwrap_err().kind, crate::error::ErrorKind::NotFound);
    }
}
