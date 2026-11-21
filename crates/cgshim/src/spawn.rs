//! What systemd 257 does when it starts a unit, and the two things that fail under proot on Android.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `POSIX_SPAWN_SETCGROUP` (glibc 2.39: clone3 with `CLONE_INTO_CGROUP`). The kernel rejects a plain
/// directory with EBADF, so every unit would fail with "Failed to spawn executor". The shim drops the
/// flag, remembers the cgroup directory, spawns, and then writes the pid into its `cgroup.procs`.
pub const POSIX_SPAWN_SETCGROUP: i16 = 0x100;

/// Where the cgroup directory of each spawn attribute object is remembered (by address) between
/// `posix_spawnattr_setcgroup_np` and `posix_spawn`.
#[derive(Default)]
pub struct Remembered(Mutex<HashMap<usize, PathBuf>>);

impl Remembered {
    pub fn remember(&self, attr: usize, dir: PathBuf) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(attr, dir);
        }
    }
    pub fn recall(&self, attr: usize) -> Option<PathBuf> {
        self.0.lock().ok()?.get(&attr).cloned()
    }
    pub fn forget(&self, attr: usize) {
        if let Ok(mut m) = self.0.lock() {
            m.remove(&attr);
        }
    }
}

/// systemd 257 starts every service with `posix_spawn("/proc/self/fd/N")` on an fd of its executor (a
/// plain file, or a sealed memfd copy). proot cannot execve through `/proc/self/fd`: its loader dies
/// with status 182 before the program runs, so every unit fails. Run the file the fd names instead;
/// for a memfd, the executor's usual path (`executor_override`, default
/// `/usr/lib/systemd/systemd-executor`). `None` leaves the call as it was.
pub fn exec_path(
    path: &Path,
    readlink: impl Fn(&Path) -> Option<PathBuf>,
    is_executable: impl Fn(&Path) -> bool,
    executor_override: Option<&str>,
) -> Option<PathBuf> {
    if !path.starts_with("/proc/self/fd/") {
        return None;
    }
    let target = readlink(path)?;
    let t = target.to_string_lossy();
    let chosen: PathBuf = if t.starts_with("/memfd:") {
        if !t.contains("executor") {
            return None;
        }
        PathBuf::from(
            executor_override
                .filter(|s| !s.is_empty())
                .unwrap_or("/usr/lib/systemd/systemd-executor"),
        )
    } else if t.starts_with('/') && !t.contains(" (deleted)") {
        target
    } else {
        return None;
    };
    is_executable(&chosen).then_some(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(to: &'static str) -> impl Fn(&Path) -> Option<PathBuf> {
        move |_| Some(PathBuf::from(to))
    }

    #[test]
    fn only_proc_self_fd_paths_are_rewritten() {
        assert_eq!(
            exec_path(Path::new("/usr/bin/x"), link("/y"), |_| true, None),
            None
        );
    }

    #[test]
    fn a_plain_file_runs_by_its_own_path() {
        let got = exec_path(
            Path::new("/proc/self/fd/9"),
            link("/usr/lib/x/y"),
            |_| true,
            None,
        );
        assert_eq!(got, Some(PathBuf::from("/usr/lib/x/y")));
        // a deleted file, or one that cannot be run, is left alone
        assert_eq!(
            exec_path(
                Path::new("/proc/self/fd/9"),
                link("/x (deleted)"),
                |_| true,
                None
            ),
            None
        );
        assert_eq!(
            exec_path(Path::new("/proc/self/fd/9"), link("/x"), |_| false, None),
            None
        );
    }

    #[test]
    fn the_executor_memfd_becomes_the_executor_binary() {
        let p = Path::new("/proc/self/fd/9");
        assert_eq!(
            exec_path(p, link("/memfd:systemd-executor (deleted)"), |_| true, None),
            Some(PathBuf::from("/usr/lib/systemd/systemd-executor"))
        );
        assert_eq!(
            exec_path(p, link("/memfd:systemd-executor"), |_| true, Some("/opt/e")),
            Some(PathBuf::from("/opt/e"))
        );
        assert_eq!(exec_path(p, link("/memfd:other"), |_| true, None), None);
    }

    #[test]
    fn remembered_attrs_are_per_object() {
        let r = Remembered::default();
        r.remember(1, PathBuf::from("/a"));
        r.remember(2, PathBuf::from("/b"));
        assert_eq!(r.recall(1), Some(PathBuf::from("/a")));
        r.forget(1);
        assert_eq!(r.recall(1), None);
        assert_eq!(r.recall(2), Some(PathBuf::from("/b")));
    }
}
