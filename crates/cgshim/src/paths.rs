//! Which paths are the (fake) cgroup tree, and what `/proc/<pid>/cgroup` paths look like. Pure
//! functions over bytes and paths, so they are tested without loading the shim.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Where systemd expects the cgroup2 hierarchy. `CGSHIM_ROOT` moves it (for tests).
pub const DEFAULT_ROOT: &str = "/sys/fs/cgroup";

/// The statfs magic of cgroup2.
pub const CGROUP2_MAGIC: i64 = 0x6367_7270;

/// The files real cgroupfs makes in every cgroup directory by itself.
pub const CGROUP_FILES: [&str; 9] = [
    "cgroup.procs",
    "cgroup.controllers",
    "cgroup.subtree_control",
    "cgroup.events",
    "cgroup.type",
    "cgroup.freeze",
    "cgroup.kill",
    "cgroup.max.depth",
    "cgroup.max.descendants",
];

/// The tree's root, read once.
pub fn root() -> &'static Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        std::env::var_os("CGSHIM_ROOT")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT))
    })
}

/// Is `p` the root or something below it? (A prefix test on whole components, so
/// `/sys/fs/cgroupfoo` is not under `/sys/fs/cgroup`.)
pub fn under(root: &Path, p: &Path) -> bool {
    p.starts_with(root)
}

/// For `/proc/self/cgroup`, `/proc/thread-self/cgroup`, `/proc/<pid>/cgroup` and
/// `/proc/<pid>/task/<tid>/cgroup`: the pid the file is about (`0` = the caller).
pub fn proc_cgroup_pid(p: &[u8]) -> Option<i32> {
    let rest = p.strip_prefix(b"/proc/")?;
    let rest = rest.strip_suffix(b"/cgroup")?;
    if rest == b"self" || rest == b"thread-self" {
        return Some(0);
    }
    let mut parts = rest.split(|&b| b == b'/');
    let pid = digits(parts.next()?)?;
    match (parts.next(), parts.next(), parts.next()) {
        (None, ..) => Some(pid),
        (Some(b"task"), Some(tid), None) if digits(tid).is_some() => Some(pid),
        _ => None,
    }
}

fn digits(s: &[u8]) -> Option<i32> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// systemd writes each pid to `cgroup.procs` at offset 0 of a plain file: successive writes would
/// overwrite each other. Opening it for append keeps every pid (droidc's helper reads them).
pub fn wants_append(root: &Path, p: &Path, flags: i32) -> bool {
    under(root, p)
        && p.file_name() == Some(OsStr::new("cgroup.procs"))
        && (flags & libc::O_ACCMODE) != libc::O_RDONLY
}

/// The body of `/proc/<pid>/cgroup`: the cgroup directory (relative to the root, `/` for the root
/// itself) in the tree whose `cgroup.procs` lists the pid.
pub fn cgroup_file_body(relative: Option<&str>) -> String {
    format!("0::{}\n", relative.filter(|r| !r.is_empty()).unwrap_or("/"))
}

/// `Some(dir)` when `p` is a `cgroup.*` file whose directory could be repaired.
pub fn cgroup_file_dir(p: &Path) -> Option<&Path> {
    let name = p.file_name()?.as_bytes();
    if !name.starts_with(b"cgroup.") {
        return None;
    }
    p.parent()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_is_by_component() {
        let r = Path::new("/sys/fs/cgroup");
        assert!(under(r, Path::new("/sys/fs/cgroup")));
        assert!(under(r, Path::new("/sys/fs/cgroup/init.scope")));
        assert!(!under(r, Path::new("/sys/fs/cgroupfoo")));
        assert!(!under(r, Path::new("/sys/fs")));
        assert!(!under(r, Path::new("relative/sys/fs/cgroup")));
    }

    #[test]
    fn proc_cgroup_paths() {
        assert_eq!(proc_cgroup_pid(b"/proc/self/cgroup"), Some(0));
        assert_eq!(proc_cgroup_pid(b"/proc/thread-self/cgroup"), Some(0));
        assert_eq!(proc_cgroup_pid(b"/proc/123/cgroup"), Some(123));
        assert_eq!(proc_cgroup_pid(b"/proc/123/task/456/cgroup"), Some(123));
        for no in [
            &b"/proc/cgroup"[..],
            b"/proc/self/cgroups",
            b"/proc/12a/cgroup",
            b"/proc/123/task/cgroup",
            b"/proc/123/task/4x/cgroup",
            b"/proc/123/fd/cgroup",
            b"/proc//cgroup",
            b"/proc/123/task/4/5/cgroup",
            b"/etc/self/cgroup",
        ] {
            assert_eq!(proc_cgroup_pid(no), None, "{}", String::from_utf8_lossy(no));
        }
    }

    #[test]
    fn cgroup_procs_is_appended_when_written() {
        let r = Path::new("/sys/fs/cgroup");
        let f = Path::new("/sys/fs/cgroup/a/cgroup.procs");
        assert!(wants_append(r, f, libc::O_WRONLY));
        assert!(wants_append(r, f, libc::O_RDWR | libc::O_CREAT));
        assert!(!wants_append(r, f, libc::O_RDONLY));
        assert!(!wants_append(
            r,
            Path::new("/sys/fs/cgroup/a/cgroup.events"),
            libc::O_WRONLY
        ));
        assert!(!wants_append(
            r,
            Path::new("/tmp/cgroup.procs"),
            libc::O_WRONLY
        ));
    }

    #[test]
    fn cgroup_file_bodies() {
        assert_eq!(cgroup_file_body(None), "0::/\n");
        assert_eq!(cgroup_file_body(Some("")), "0::/\n");
        assert_eq!(
            cgroup_file_body(Some("/session.slice/a.service")),
            "0::/session.slice/a.service\n"
        );
    }

    #[test]
    fn repairable_files() {
        assert_eq!(
            cgroup_file_dir(Path::new("/x/y/cgroup.procs")),
            Some(Path::new("/x/y"))
        );
        assert_eq!(cgroup_file_dir(Path::new("/x/y/other")), None);
    }
}
