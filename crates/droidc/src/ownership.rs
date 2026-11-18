//! `fix-ownership [--undo]` (root only): apply the image's uid/gid, full mode (setuid, setgid,
//! sticky) and xattrs from `metadata.jsonl.zst` to the current deployment, or give the whole root
//! back to the app uid.
//!
//! Every row is resolved with `openat2(O_PATH | O_NOFOLLOW, RESOLVE_IN_ROOT)` against its tree, so
//! a symlink in the image can never make us chown or chmod a host file. A row must match its
//! recorded type, with one exception: a regular file (`f`/`h`) that link2symlink turned into a
//! symlink chain is followed, and only accepted if it ends at an `<l2s prefix>` regular file inside
//! the canonical root. Rows are mapped to their tree: `usr/etc/X` also to the live `etc/X`, `var/X`
//! to `<root>/var/X` (only the deployment that seeded `<root>/var` has `var/` rows; unpack drops
//! them for the others).
//!
//! Order per row: chown, then chmod (chown clears setuid/setgid), then xattrs (chown clears
//! `security.capability`). `security.selinux` is never applied: labels are Android's business.

use crate::deploy::{open_root, write_record};
use crate::error::{Error, Result};
use crate::layout::Root;
use crate::record::OWNERSHIP_IMAGE;
use crate::unpack::read_metadata;
use base64::Engine;
use rustix::fs::{
    chmod, chownat, fstat, openat2, readlinkat, AtFlags, FileType, Mode, OFlags, ResolveFlags,
    XattrFlags,
};
use rustix::process::{Gid, Uid};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

pub(crate) fn require_root(what: &str) -> Result<()> {
    if rustix::process::geteuid().is_root() {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "{what} needs root (run it through su)"
        )))
    }
}

fn open_dir(p: &Path) -> Result<OwnedFd> {
    rustix::fs::open(
        p,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::from(std::io::Error::from(e)))
}

fn proc_fd(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// A row resolved to an inode we may change.
#[derive(Debug)]
pub struct Resolved {
    pub fd: OwnedFd,
    pub symlink: bool,
    pub via_l2s: bool,
}

/// Resolve `rel` in `tree` for a row of type `t` (`f|h|d|l`); `Err` is the skip reason.
pub fn resolve_row(
    tree: &OwnedFd,
    rel: &str,
    t: &str,
    canon_root: &Path,
    l2s_prefix: &str,
) -> std::result::Result<Resolved, &'static str> {
    let rel = if rel.is_empty() { "." } else { rel };
    let fd = openat2(
        tree,
        rel,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|_| "missing")?;
    let ft = FileType::from_raw_mode(fstat(&fd).map_err(|_| "stat failed")?.st_mode);
    let ok = |fd, symlink| {
        Ok(Resolved {
            fd,
            symlink,
            via_l2s: false,
        })
    };
    match (t, ft) {
        ("d", FileType::Directory) => ok(fd, false),
        ("l", FileType::Symlink) => ok(fd, true),
        ("f" | "h", FileType::RegularFile) => ok(fd, false),
        ("f" | "h", FileType::Symlink) => follow_l2s(&fd, t, canon_root, l2s_prefix),
        _ => Err("type mismatch"),
    }
}

/// Whether `name` is an l2s entry. Both layouts are accepted whatever `DROIDC_L2S_PREFIX` says
/// (`sudo`/`su` usually drop it): Termux `.l2s.` and proot-me `.proot.l2s.`.
fn is_l2s_name(name: &str, prefix: &str) -> bool {
    [prefix, ".l2s.", ".proot.l2s."]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Follow a link2symlink chain (absolute host paths to an l2s intermediate) and accept only an l2s
/// data file inside the canonical root. Any other symlink on an `h` row is a hard link to a
/// symlink (ostree-built images hard-link symlinks too): left alone.
fn follow_l2s(
    link: &OwnedFd,
    t: &str,
    canon_root: &Path,
    prefix: &str,
) -> std::result::Result<Resolved, &'static str> {
    let target = readlinkat(link, "", Vec::new()).map_err(|_| "readlink failed")?;
    let target = Path::new(std::ffi::OsStr::from_bytes(target.as_bytes())).to_path_buf();
    let chain = target.is_absolute()
        && target
            .file_name()
            .is_some_and(|n| is_l2s_name(&n.to_string_lossy(), prefix));
    if !chain {
        return Err(if t == "h" {
            "hard link to a symlink"
        } else {
            "symlink where a file was recorded"
        });
    }
    let fd = rustix::fs::open(&target, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
        .map_err(|_| "l2s chain does not resolve")?;
    let real = fs::read_link(proc_fd(&fd)).map_err(|_| "l2s chain does not resolve")?;
    let st = fstat(&fd).map_err(|_| "stat failed")?;
    let name_ok = real
        .file_name()
        .is_some_and(|n| is_l2s_name(&n.to_string_lossy(), prefix));
    if !real.starts_with(canon_root)
        || !name_ok
        || FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
    {
        return Err("l2s chain leaves the root");
    }
    Ok(Resolved {
        fd,
        symlink: false,
        via_l2s: true,
    })
}

/// The trees a metadata row applies to: `(tree, path inside it)`.
pub fn row_targets(p: &str) -> Vec<(&'static str, String)> {
    if let Some(rest) = p
        .strip_prefix("var/")
        .or(if p == "var" { Some("") } else { None })
    {
        return vec![("var", rest.to_string())];
    }
    let mut v = vec![("rootfs", p.to_string())];
    if let Some(rest) = p.strip_prefix("usr/etc/") {
        v.push(("etc", format!("etc/{rest}")));
    } else if p == "usr/etc" {
        v.push(("etc", "etc".into()));
    }
    v
}

#[derive(Default)]
struct Report {
    applied: u64,
    via_l2s: u64,
    xattr_errors: u64,
    skipped: BTreeMap<String, u64>,
}

fn apply(r: &Resolved, row: &Value, rep: &mut Report) -> Result<()> {
    let sys = |what: &str, e: rustix::io::Errno| Error::internal(format!("{what}: {e}"));
    let uid = Uid::from_raw(row["u"].as_u64().unwrap_or(0) as u32);
    let gid = Gid::from_raw(row["g"].as_u64().unwrap_or(0) as u32);
    chownat(&r.fd, "", Some(uid), Some(gid), AtFlags::EMPTY_PATH).map_err(|e| sys("chown", e))?;
    if r.symlink {
        return Ok(());
    }
    let path = proc_fd(&r.fd);
    let mode = row["m"].as_u64().unwrap_or(0o644) as u32 & 0o7777;
    chmod(path.as_str(), Mode::from_raw_mode(mode)).map_err(|e| sys("chmod", e))?;
    if let Some(x) = row["x"].as_object() {
        for (name, v) in x {
            if name == "security.selinux" {
                continue;
            }
            let Ok(val) =
                base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap_or(""))
            else {
                rep.xattr_errors += 1;
                continue;
            };
            if rustix::fs::setxattr(path.as_str(), name.as_str(), &val, XattrFlags::empty())
                .is_err()
            {
                rep.xattr_errors += 1;
            }
        }
    }
    Ok(())
}

/// Apply the image's ownership to the current deployment.
pub fn fix(root_path: &Path, l2s_prefix: &str) -> Result<Value> {
    require_root("fix-ownership")?;
    let (root, _lock) = open_root(root_path)?;
    let id = root
        .current_id()
        .ok_or_else(|| Error::not_found("no current deployment"))?;
    apply_image(&root, &id, l2s_prefix)
}

/// Apply the image's ownership to deployment `id` (lock held by the caller, `<root>/var` already
/// seeded) and mark its record.
pub(crate) fn apply_image(root: &Root, id: &str, l2s_prefix: &str) -> Result<Value> {
    let dep = root.deployment(id);
    let rows = read_metadata(&dep.join("metadata.jsonl.zst"))?;
    let rootfs = open_dir(&dep.join("rootfs"))?;
    let var = open_dir(&root.var())?;
    let mut rep = Report::default();
    // Hard-link rows carry the link header's own mode/owner; the inode's real one comes from its
    // `f` row. So: `f`/`d`/`l` first, then `h` rows only for inodes no `f` row reached (target
    // replaced in a later layer).
    let mut seen = std::collections::HashSet::new();
    for pass_h in [false, true] {
        for row in &rows {
            let (Some(p), Some(t)) = (row["p"].as_str(), row["t"].as_str()) else {
                if !pass_h {
                    *rep.skipped.entry("malformed row".into()).or_default() += 1;
                }
                continue;
            };
            if (t == "h") != pass_h {
                continue;
            }
            for (tree, rel) in row_targets(p) {
                let fd = if tree == "var" { &var } else { &rootfs };
                let why = match resolve_row(fd, &rel, t, &root.path, l2s_prefix) {
                    Ok(r) => {
                        let st =
                            fstat(&r.fd).map_err(|e| Error::internal(format!("stat {p}: {e}")))?;
                        if !seen.insert((st.st_dev, st.st_ino)) {
                            if pass_h {
                                *rep.skipped
                                    .entry("hard link (inode already set)".into())
                                    .or_default() += 1;
                            }
                            continue;
                        }
                        apply(&r, row, &mut rep)?;
                        rep.applied += 1;
                        rep.via_l2s += r.via_l2s as u64;
                        continue;
                    }
                    Err(why) => why,
                };
                let key = if tree == "rootfs" {
                    why.to_string()
                } else {
                    format!("{tree}: {why}")
                };
                *rep.skipped.entry(key).or_default() += 1;
            }
        }
    }
    let mut rec = root.read_record(id)?;
    rec.ownership = Some(OWNERSHIP_IMAGE.into());
    write_record(&dep.join("deployment.json"), &rec)?;
    Ok(json!({
        "id": id,
        "ownership": OWNERSHIP_IMAGE,
        "applied": rep.applied,
        "via_l2s": rep.via_l2s,
        "xattr_errors": rep.xattr_errors,
        "skipped": rep.skipped,
    }))
}

/// Give everything under the root back to the root directory's owner (the app uid) with unpack's
/// safe modes, so proot sessions and non-root commands work again.
pub fn undo(root_path: &Path) -> Result<Value> {
    require_root("fix-ownership --undo")?;
    let (root, _lock) = open_root(root_path)?;
    let md = fs::metadata(&root.path)?;
    if md.uid() == 0 {
        return Err(Error::invalid(format!(
            "{} is owned by root: no app uid to give it back to",
            root.path.display()
        )));
    }
    let (uid, gid) = (Uid::from_raw(md.uid()), Gid::from_raw(md.gid()));
    let mut entries = 0u64;
    walk_undo(&root.path, uid, gid, &mut entries)?;
    let mut ids = Vec::new();
    for e in fs::read_dir(root.deployments())?.flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        if let Ok(mut rec) = root.read_record(&id) {
            if rec.ownership.take().is_some() {
                write_record(&root.deployment(&id).join("deployment.json"), &rec)?;
                ids.push(id);
            }
        }
    }
    Ok(json!({"entries": entries, "deployments": ids, "uid": md.uid(), "gid": md.gid()}))
}

/// lchown every entry to the app uid; directories `| 0o700`, regular files `| 0o600`, both without
/// setuid/setgid/sticky; drop `security.capability`. Never follows symlinks.
fn walk_undo(p: &Path, uid: Uid, gid: Gid, n: &mut u64) -> Result<()> {
    let md = fs::symlink_metadata(p)?;
    let cwd = rustix::fs::CWD;
    chownat(cwd, p, Some(uid), Some(gid), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::internal(format!("chown {}: {e}", p.display())))?;
    *n += 1;
    let ft = md.file_type();
    if ft.is_dir() {
        fs::set_permissions(p, fs::Permissions::from_mode((md.mode() & 0o777) | 0o700))?;
        for e in fs::read_dir(p)? {
            walk_undo(&e?.path(), uid, gid, n)?;
        }
    } else if ft.is_file() {
        fs::set_permissions(p, fs::Permissions::from_mode((md.mode() & 0o777) | 0o600))?;
        let _ = rustix::fs::removexattr(p, "security.capability");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rows_map_to_their_trees() {
        assert_eq!(row_targets("usr/bin/su"), [("rootfs", "usr/bin/su".into())]);
        assert_eq!(
            row_targets("usr/etc/shadow"),
            [
                ("rootfs", "usr/etc/shadow".into()),
                ("etc", "etc/shadow".into())
            ]
        );
        assert_eq!(
            row_targets("usr/etc"),
            [("rootfs", "usr/etc".into()), ("etc", "etc".into())]
        );
        assert_eq!(row_targets("var/lib/x"), [("var", "lib/x".into())]);
        assert_eq!(row_targets("var"), [("var", "".into())]);
        assert_eq!(row_targets("variety"), [("rootfs", "variety".into())]);
    }

    #[test]
    fn resolution_never_leaves_the_tree() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let host = root.join("host");
        let rootfs = root.join("rootfs");
        fs::create_dir_all(host.join("bin")).unwrap();
        fs::write(host.join("bin/x"), "host").unwrap();
        fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        fs::write(rootfs.join("usr/bin/su"), "su").unwrap();
        symlink(&host, rootfs.join("usr/share")).unwrap(); // absolute, crafted
        symlink("../../../host", rootfs.join("up")).unwrap();
        symlink("su", rootfs.join("usr/bin/alias")).unwrap();
        let fd = open_dir(&rootfs).unwrap();
        let r = |p: &str, t: &str| resolve_row(&fd, p, t, &root, ".l2s.");

        assert!(r("usr/bin/su", "f").is_ok());
        assert!(r("usr/bin", "d").is_ok());
        assert!(r("usr/bin/alias", "l").unwrap().symlink);
        // intermediate symlinks resolve inside the rootfs, where these paths don't exist
        assert_eq!(r("usr/share/bin/x", "f").unwrap_err(), "missing");
        assert_eq!(r("up/bin/x", "f").unwrap_err(), "missing");
        // the last component is never followed except as an l2s chain
        assert_eq!(r("usr/bin/su", "d").unwrap_err(), "type mismatch");
        assert_eq!(r("usr/bin/alias", "d").unwrap_err(), "type mismatch");
        assert_eq!(
            r("usr/bin/alias", "f").unwrap_err(),
            "symlink where a file was recorded"
        );
        assert_eq!(
            r("usr/bin/alias", "h").unwrap_err(),
            "hard link to a symlink"
        );
        // absolute, not l2s-named: never followed
        assert_eq!(
            r("usr/share", "f").unwrap_err(),
            "symlink where a file was recorded"
        );
    }

    #[test]
    fn l2s_chains_are_followed_to_their_data_file() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let dir = root.join("rootfs/usr/bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".l2s.su0001.0002"), "su").unwrap();
        symlink(dir.join(".l2s.su0001.0002"), dir.join(".l2s.su0001")).unwrap();
        symlink(dir.join(".l2s.su0001"), dir.join("su")).unwrap();
        symlink(dir.join(".l2s.su0001"), dir.join("sudo")).unwrap();
        // a link to a non-l2s file inside the root is not followed
        fs::write(dir.join("plain"), "x").unwrap();
        symlink(dir.join("plain"), dir.join("fake")).unwrap();
        // an l2s-named intermediate that ends outside the root (or at a non-l2s file) is refused
        let outside = tempfile::tempdir().unwrap();
        let out = fs::canonicalize(outside.path()).unwrap();
        fs::write(out.join(".l2s.x0001.0001"), "host").unwrap();
        symlink(out.join(".l2s.x0001.0001"), dir.join(".l2s.x0001")).unwrap();
        symlink(dir.join(".l2s.x0001"), dir.join("x")).unwrap();
        symlink(dir.join("plain"), dir.join(".l2s.y0001")).unwrap();
        symlink(dir.join(".l2s.y0001"), dir.join("y")).unwrap();
        let fd = open_dir(&root.join("rootfs")).unwrap();
        for p in ["usr/bin/su", "usr/bin/sudo"] {
            let r = resolve_row(&fd, p, "h", &root, ".l2s.").unwrap();
            assert!(r.via_l2s && !r.symlink);
            assert_eq!(
                fs::read_link(proc_fd(&r.fd)).unwrap(),
                dir.join(".l2s.su0001.0002")
            );
        }
        assert_eq!(
            resolve_row(&fd, "usr/bin/fake", "f", &root, ".l2s.").unwrap_err(),
            "symlink where a file was recorded"
        );
        for p in ["usr/bin/x", "usr/bin/y"] {
            assert_eq!(
                resolve_row(&fd, p, "f", &root, ".l2s.").unwrap_err(),
                "l2s chain leaves the root",
                "{p}"
            );
        }
        // the configured prefix doesn't matter: both layouts are recognised (sudo drops env)
        assert!(
            resolve_row(&fd, "usr/bin/su", "f", &root, ".proot.l2s.")
                .unwrap()
                .via_l2s
        );
    }
}
