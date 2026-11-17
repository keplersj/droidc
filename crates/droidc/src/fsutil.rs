//! Small filesystem helpers.

use crate::error::{Context, Error, Result};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Remove a tree without following symlinks, fixing permissions that block removal.
pub fn rmtree(p: &Path) -> Result<()> {
    let md = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context(p.display()),
    };
    if md.is_dir() {
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
        for e in fs::read_dir(p).context(p.display())? {
            rmtree(&e?.path())?;
        }
        fs::remove_dir(p).context(p.display())
    } else {
        fs::remove_file(p).context(p.display())
    }
}

/// du-like: allocated bytes of a tree (hard-linked inodes counted once, symlinks not followed).
pub fn disk_usage(p: &Path) -> u64 {
    fn walk(p: &Path, seen: &mut HashSet<(u64, u64)>, total: &mut u64) {
        let Ok(md) = fs::symlink_metadata(p) else {
            return;
        };
        if md.nlink() > 1 && !md.is_dir() && !seen.insert((md.dev(), md.ino())) {
            return;
        }
        *total += md.blocks() * 512;
        if md.is_dir() {
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path(), seen, total);
                }
            }
        }
    }
    let mut total = 0;
    walk(p, &mut HashSet::new(), &mut total);
    total
}

/// Free bytes available to unprivileged users on the filesystem holding `p`.
pub fn free_space(p: &Path) -> Result<u64> {
    let s = rustix::fs::statvfs(p)
        .map_err(|e| Error::internal(format!("statvfs {}: {e}", p.display())))?;
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}

pub fn is_empty_dir(p: &Path) -> bool {
    match fs::read_dir(p) {
        Ok(mut rd) => rd.next().is_none(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rmtree_handles_readonly_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"x").unwrap();
        let t = d.path().join("t");
        fs::create_dir_all(t.join("a/b")).unwrap();
        fs::write(t.join("a/b/f"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, t.join("a/link")).unwrap();
        fs::set_permissions(t.join("a/b"), fs::Permissions::from_mode(0o500)).unwrap();
        fs::set_permissions(t.join("a"), fs::Permissions::from_mode(0o500)).unwrap();
        rmtree(&t).unwrap();
        assert!(!t.exists());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn du_counts_hardlinks_once() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a"), vec![1u8; 100_000]).unwrap();
        let one = disk_usage(d.path());
        fs::hard_link(d.path().join("a"), d.path().join("b")).unwrap();
        assert_eq!(disk_usage(d.path()), one);
        assert!(free_space(d.path()).unwrap() > 0);
    }
}
