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

/// Copy a regular file's bytes and permission bits with a plain read/write loop. Deliberately not
/// `std::fs::copy`/`io::copy`, which use `copy_file_range`/`sendfile` through raw `syscall()`: those
/// can be killed (SIGSYS) by Android's app seccomp filter.
pub fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let mut r = fs::File::open(src)?;
    let mode = r.metadata()?.mode();
    let mut w = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
    }
    w.set_permissions(fs::Permissions::from_mode(mode & 0o7777))
}

pub fn is_empty_dir(p: &Path) -> bool {
    match fs::read_dir(p) {
        Ok(mut rd) => rd.next().is_none(),
        Err(_) => true,
    }
}

/// SELinux label (`security.selinux`) of `p`; `follow` resolves a final symlink (needed for
/// `/proc/self/fd/N`). None without SELinux.
pub fn selinux_label(p: &Path, follow: bool) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 512];
    let n = if follow {
        rustix::fs::getxattr(p, "security.selinux", &mut buf[..])
    } else {
        rustix::fs::lgetxattr(p, "security.selinux", &mut buf[..])
    }
    .ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Give `p` (never followed) the SELinux label `label` unless it already has it. On Android a file
/// a root (su) process creates in app storage gets that process's MLS level instead of the app's
/// categories, and the app can then read but not write, rename or delete it; ownership alone does
/// not fix that. Best effort: without SELinux, or without permission, nothing happens.
pub fn inherit_selinux_label(p: &Path, label: Option<&[u8]>) {
    if let Some(l) = label {
        if selinux_label(p, false).as_deref() != Some(l) {
            let _ =
                rustix::fs::lsetxattr(p, "security.selinux", l, rustix::fs::XattrFlags::empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs SELinux (skips without it, e.g. on the CI runners): relabels a file to its
    /// directory's label, never through a symlink.
    #[test]
    fn selinux_label_is_inherited() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let Some(dir_label) = selinux_label(d.path(), false) else {
            eprintln!("skipped: no SELinux");
            return;
        };
        let f = d.path().join("f");
        fs::write(&f, "").unwrap();
        // give it some other valid label: the one of a file in /tmp
        let other = tempfile::NamedTempFile::new().unwrap();
        let Some(other_label) = selinux_label(other.path(), false).filter(|l| *l != dir_label)
        else {
            eprintln!("skipped: no second label to test with");
            return;
        };
        if rustix::fs::lsetxattr(
            &f,
            "security.selinux",
            &other_label,
            rustix::fs::XattrFlags::empty(),
        )
        .is_err()
        {
            eprintln!("skipped: not allowed to relabel");
            return;
        }
        let link = d.path().join("link");
        std::os::unix::fs::symlink(other.path(), &link).unwrap();
        inherit_selinux_label(&f, Some(&dir_label));
        assert_eq!(selinux_label(&f, false).as_deref(), Some(&dir_label[..]));
        inherit_selinux_label(&link, Some(&dir_label));
        assert_eq!(
            selinux_label(other.path(), false).as_deref(),
            Some(&other_label[..]),
            "symlink target untouched"
        );
        inherit_selinux_label(&f, None); // no-op without SELinux
    }

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
