//! Clearing what dead graphical sessions leave behind (#12): Wayland sockets and their locks in
//! `$XDG_RUNTIME_DIR`, X sockets in `/tmp/.X11-unix` with their `/tmp/.X<n>-lock` files, and Mutter's
//! Xwayland auth files.
//!
//! The guest shares Android's PID namespace, so Xwayland's own staleness check (`kill(pid, 0)` on the
//! pid in the lock) can take an unrelated app that reused the pid for a live server, and the next session
//! moves on to `:1`, `:2`, ... Here nothing is judged by pid:
//! - a Wayland socket is stale when `connect` is refused and nobody holds its `.lock` (libwayland keeps
//!   an exclusive `flock` on it for as long as the compositor runs);
//! - an X display is stale when its socket is missing or refuses connections;
//! - the auth files go once no X display is live.
//!
//! Live displays, the host compositor's among them, are never touched.

use rustix::fs::{flock, FlockOperation};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Whether something accepts connections on `p`. A missing file, a refused connection or a file that
/// is not a socket all count as dead; any other error (permissions) as live, to be safe.
fn socket_live(p: &Path) -> bool {
    match UnixStream::connect(p) {
        Ok(_) => true,
        Err(e) => {
            let dead = [rustix::io::Errno::NOTSOCK, rustix::io::Errno::CONNREFUSED]
                .iter()
                .any(|c| e.raw_os_error() == Some(c.raw_os_error()));
            !(dead || matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused))
        }
    }
}

/// Whether some process holds `lock` (an exclusive `flock`). False when it cannot be opened.
fn lock_held(lock: &Path) -> bool {
    let Ok(f) = fs::OpenOptions::new().read(true).open(lock) else {
        return false;
    };
    loop {
        match flock(&f, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return false, // released when `f` drops
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return true,
        }
    }
}

fn remove(p: &Path, out: &mut Vec<PathBuf>) {
    match fs::remove_file(p) {
        Ok(()) => out.push(p.to_path_buf()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => eprintln!("warn: could not remove {}: {e}", p.display()),
    }
}

fn names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// `wayland-N` (not `wayland-N.lock`), the names libwayland's `wl_display_add_socket_auto` picks.
fn wayland_name(n: &str) -> bool {
    n.strip_prefix("wayland-")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// The display number of `X<n>` (a socket) or `.X<n>-lock`.
fn x_display(n: &str) -> Option<u32> {
    n.strip_prefix(".X")
        .and_then(|r| r.strip_suffix("-lock"))
        .or_else(|| n.strip_prefix('X'))
        .and_then(|d| d.parse().ok())
}

/// Removes the stale displays' files under `runtime` (`$XDG_RUNTIME_DIR`) and `tmp` (`/tmp`).
/// Returns what was removed.
pub fn sweep(runtime: &Path, tmp: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();

    // Wayland: a socket and its lock, or a lock alone.
    let rt = names(runtime);
    let mut seen: Vec<&str> = Vec::new();
    for n in &rt {
        let base = n.strip_suffix(".lock").unwrap_or(n);
        if !wayland_name(base) || seen.contains(&base) {
            continue;
        }
        seen.push(base);
        let sock = runtime.join(base);
        let lock = runtime.join(format!("{base}.lock"));
        // Both tests: a compositor that answers is live even if it keeps no lock (or not as libwayland
        // does), and one holding its lock is live even while it is not accepting yet.
        let stale = !(lock.exists() && lock_held(&lock)) && !socket_live(&sock);
        if stale {
            remove(&sock, &mut out);
            remove(&lock, &mut out);
        }
    }

    // X: a display is live only if its socket answers.
    let xdir = tmp.join(".X11-unix");
    let mut displays: Vec<u32> = names(&xdir)
        .iter()
        .chain(names(tmp).iter())
        .filter_map(|n| x_display(n))
        .collect();
    displays.sort_unstable();
    displays.dedup();
    let mut any_live = false;
    for d in displays {
        let sock = xdir.join(format!("X{d}"));
        if socket_live(&sock) {
            any_live = true;
            continue;
        }
        remove(&sock, &mut out);
        remove(&tmp.join(format!(".X{d}-lock")), &mut out);
    }

    // Mutter's per-session Xwayland auth files, useless once no X server runs.
    if !any_live {
        for n in rt.iter().filter(|n| n.starts_with(".mutter-Xwaylandauth.")) {
            remove(&runtime.join(n), &mut out);
        }
    }
    out
}

/// Where X keeps its sockets and locks: `/tmp` (`DROIDC_X11_TMP` replaces it in tests).
pub fn x_tmp() -> PathBuf {
    std::env::var_os("DROIDC_X11_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// [`sweep`] with a one-line report of what went.
pub fn sweep_and_report(runtime: &Path) {
    let gone = sweep(runtime, &x_tmp());
    if !gone.is_empty() {
        let list: Vec<String> = gone.iter().map(|p| p.display().to_string()).collect();
        eprintln!(
            "note: removed {} stale display file(s): {}",
            gone.len(),
            list.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn dead_socket(p: &Path) {
        drop(UnixListener::bind(p).unwrap()); // the file stays, nothing listens
    }

    #[test]
    fn names_are_recognised() {
        assert!(wayland_name("wayland-0"));
        assert!(wayland_name("wayland-12"));
        assert!(!wayland_name("wayland-"));
        assert!(!wayland_name("wayland-0.lock"));
        assert!(!wayland_name("wayland-x"));
        assert_eq!(x_display("X0"), Some(0));
        assert_eq!(x_display(".X11-lock"), Some(11));
        assert_eq!(x_display(".X11-unix"), None);
        assert_eq!(x_display("Xauthority"), None);
    }

    #[test]
    fn stale_displays_go_and_live_ones_stay() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        let tmp = d.path().join("tmp");
        fs::create_dir_all(&run).unwrap();
        fs::create_dir_all(tmp.join(".X11-unix")).unwrap();

        // live compositor: socket listening, lock held (as libwayland does)
        let _host = UnixListener::bind(run.join("wayland-0")).unwrap();
        fs::write(run.join("wayland-0.lock"), "").unwrap();
        let held = fs::File::open(run.join("wayland-0.lock")).unwrap();
        flock(&held, FlockOperation::NonBlockingLockExclusive).unwrap();
        // dead session: socket left, lock free
        dead_socket(&run.join("wayland-1"));
        fs::write(run.join("wayland-1.lock"), "").unwrap();
        // a lock without its socket
        fs::write(run.join("wayland-2.lock"), "").unwrap();
        // a dead socket without a lock (Selkie's compositor makes none)
        dead_socket(&run.join("wayland-3"));
        // a live one without a lock
        let _live4 = UnixListener::bind(run.join("wayland-4")).unwrap();
        // not ours
        fs::write(run.join("bus"), "").unwrap();

        // X: :0 dead (pid in the lock is irrelevant), :1 has a lock but no socket
        dead_socket(&tmp.join(".X11-unix/X0"));
        fs::write(tmp.join(".X0-lock"), "     21901\n").unwrap();
        fs::write(tmp.join(".X1-lock"), "     21901\n").unwrap();
        fs::write(run.join(".mutter-Xwaylandauth.03XOY3"), "").unwrap();

        let mut gone = sweep(&run, &tmp);
        gone.sort();
        let mut want = vec![
            run.join("wayland-1"),
            run.join("wayland-1.lock"),
            run.join("wayland-2.lock"),
            run.join("wayland-3"),
            tmp.join(".X11-unix/X0"),
            tmp.join(".X0-lock"),
            tmp.join(".X1-lock"),
            run.join(".mutter-Xwaylandauth.03XOY3"),
        ];
        want.sort();
        assert_eq!(gone, want);
        for keep in ["wayland-0", "wayland-0.lock", "wayland-4", "bus"] {
            assert!(run.join(keep).exists(), "{keep} must stay");
        }
        assert!(sweep(&run, &tmp).is_empty(), "nothing left the second time");
    }

    #[test]
    fn auth_files_stay_while_an_x_server_runs() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        let tmp = d.path().join("tmp");
        fs::create_dir_all(&run).unwrap();
        fs::create_dir_all(tmp.join(".X11-unix")).unwrap();
        let _x = UnixListener::bind(tmp.join(".X11-unix/X0")).unwrap();
        fs::write(tmp.join(".X0-lock"), "1\n").unwrap();
        fs::write(run.join(".mutter-Xwaylandauth.AAAA"), "").unwrap();
        assert!(sweep(&run, &tmp).is_empty());
    }
}
