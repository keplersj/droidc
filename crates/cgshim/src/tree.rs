//! The fake cgroup tree: a plain directory that droidc's helper keeps current (see `cgroups.rs` in
//! the droidc crate). Membership and "populated" accounting are NOT done here.

use crate::paths::CGROUP_FILES;
use std::fs;
use std::io::Write;
use std::path::Path;

const MAX_DEPTH: usize = 8;

/// The cgroup directory (relative to `root`, with a leading `/`) whose `cgroup.procs` lists `pid`,
/// deepest first. Real cgroupfs answers this from the kernel.
pub fn find_cgroup(root: &Path, pid: i32) -> Option<String> {
    find_in(root, root, pid, 0)
}

fn find_in(root: &Path, dir: &Path, pid: i32, depth: usize) -> Option<String> {
    if depth < MAX_DEPTH {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            if entry.file_name().as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let sub = entry.path();
            if sub.is_dir() {
                if let Some(found) = find_in(root, &sub, pid, depth + 1) {
                    return Some(found);
                }
            }
        }
    }
    let procs = fs::read_to_string(dir.join("cgroup.procs")).ok()?;
    let listed = procs
        .split_whitespace()
        .filter_map(|v| v.parse::<i32>().ok())
        .any(|v| v == pid);
    if !listed {
        return None;
    }
    let rel = dir.strip_prefix(root).ok()?;
    Some(format!("/{}", rel.display()))
}

/// Create the `cgroup.*` files real cgroupfs makes on its own in a new cgroup directory.
/// Existing files are truncated except `cgroup.procs`' content is not preserved either: this is
/// for new or repaired directories. Returns the files that could not be made, with the error.
pub fn populate(dir: &Path) -> Vec<(String, std::io::Error)> {
    let mut failed = Vec::new();
    for name in CGROUP_FILES {
        // `open(O_CREAT)`, never `creat()`: that is a legacy syscall which fails with ENOSYS under
        // proot on x86_64 Android (found on the API 34 emulator).
        let made = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join(name))
            .and_then(|mut f| {
                if name == "cgroup.events" {
                    f.write_all(b"populated 1\nfrozen 0\n")?;
                }
                Ok(())
            });
        if let Err(e) = made {
            failed.push((name.to_string(), e));
        }
    }
    failed
}

/// Remove those files so the directory can go (real cgroupfs lets a "populated" tree directory go).
pub fn unpopulate(dir: &Path) {
    for name in CGROUP_FILES {
        let _ = fs::remove_file(dir.join(name));
    }
}

/// A directory that exists without its `cgroup.*` files (made unseen, or left by an earlier run)
/// makes systemd's attach fail with ENOENT. Make the files; false when `dir` is not a directory.
pub fn repair(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    populate(dir);
    true
}

/// Append a pid to a cgroup's `cgroup.procs`, which is how droidc's helper learns of it.
pub fn attach(cgroup: &Path, pid: i32) {
    if let Ok(mut f) = fs::OpenOptions::new()
        .append(true)
        .open(cgroup.join("cgroup.procs"))
    {
        let _ = writeln!(f, "{pid}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempdir::Dir {
        tempdir::Dir::new()
    }

    /// A throwaway directory without a dev-dependency.
    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let p = std::env::temp_dir().join(format!(
                    "cgshim-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn populate_makes_every_file_and_events_starts_populated() {
        let d = tmp();
        assert!(populate(&d.0).is_empty());
        for f in CGROUP_FILES {
            assert!(d.0.join(f).is_file(), "{f}");
        }
        assert_eq!(
            fs::read_to_string(d.0.join("cgroup.events")).unwrap(),
            "populated 1\nfrozen 0\n"
        );
        unpopulate(&d.0);
        assert_eq!(fs::read_dir(&d.0).unwrap().count(), 0);
    }

    #[test]
    fn repair_only_touches_directories() {
        let d = tmp();
        assert!(!repair(&d.0.join("missing")));
        let sub = d.0.join("init.scope");
        fs::create_dir(&sub).unwrap();
        assert!(repair(&sub));
        assert!(sub.join("cgroup.procs").is_file());
    }

    #[test]
    fn find_cgroup_reports_the_deepest_listing_directory() {
        let d = tmp();
        let svc = d.0.join("session.slice/a.service");
        fs::create_dir_all(&svc).unwrap();
        populate(&d.0);
        populate(&d.0.join("session.slice"));
        populate(&svc);
        fs::write(svc.join("cgroup.procs"), "10\n42\n").unwrap();
        fs::write(d.0.join("cgroup.procs"), "1\n42\n").unwrap();
        assert_eq!(
            find_cgroup(&d.0, 42).as_deref(),
            Some("/session.slice/a.service")
        );
        assert_eq!(find_cgroup(&d.0, 1).as_deref(), Some("/"));
        assert_eq!(find_cgroup(&d.0, 999), None);
    }

    #[test]
    fn attach_appends() {
        let d = tmp();
        populate(&d.0);
        attach(&d.0, 5);
        attach(&d.0, 6);
        assert_eq!(
            fs::read_to_string(d.0.join("cgroup.procs")).unwrap(),
            "5\n6\n"
        );
    }
}
