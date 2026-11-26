//! 3-way `/etc` merge: old pristine (`usr/etc` of the current deployment), old live (its `etc`) and
//! new pristine (`usr/etc` of the new image) into the new live `etc`.
//!
//! Per path: unchanged locally (old live == old pristine) -> new pristine; modified or added locally
//! -> keep local; deleted locally -> stays deleted. Files compare by mode + bytes, symlinks by target.

use crate::error::{Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct MergeStats {
    pub taken_new: u64,
    pub kept_local: u64,
    pub kept_deleted: u64,
}

#[derive(Debug)]
enum Ent {
    None,
    Dir(u32),
    File(PathBuf, u32),
    Link(PathBuf),
    Other,
}

fn is_l2s_target(t: &Path) -> bool {
    t.file_name()
        .map(|n| {
            n.to_string_lossy().starts_with(".l2s.")
                || n.to_string_lossy().starts_with(".proot.l2s.")
        })
        .unwrap_or(false)
}

fn stat(p: Option<PathBuf>) -> Ent {
    let Some(p) = p else { return Ent::None };
    let Ok(md) = fs::symlink_metadata(&p) else {
        return Ent::None;
    };
    let ft = md.file_type();
    if ft.is_dir() {
        Ent::Dir(md.mode() & 0o777)
    } else if ft.is_symlink() {
        match fs::read_link(&p) {
            // A faked hard link made by proot in the guest: it is a regular file for the user.
            Ok(t) if is_l2s_target(&t) => match fs::metadata(&p) {
                Ok(m) => Ent::File(p, m.mode() & 0o777),
                Err(_) => Ent::Link(t),
            },
            Ok(t) => Ent::Link(t),
            Err(_) => Ent::Other,
        }
    } else if ft.is_file() {
        Ent::File(p, md.mode() & 0o777)
    } else {
        Ent::Other
    }
}

fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let na = match read_full(&mut fa, &mut ba) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let nb = match read_full(&mut fb, &mut bb) {
            Ok(n) => n,
            Err(_) => return false,
        };
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// An empty regular file with mode 000: what proot leaves where it binds a file the rootfs lacked.
fn is_bind_placeholder(e: &Ent) -> bool {
    match e {
        Ent::File(p, 0) => fs::symlink_metadata(p)
            .map(|m| m.len() == 0)
            .unwrap_or(false),
        _ => false,
    }
}

/// Equality of two non-directory entries (None == None).
fn same(a: &Ent, b: &Ent) -> bool {
    match (a, b) {
        (Ent::None, Ent::None) => true,
        (Ent::Link(x), Ent::Link(y)) => x == y,
        (Ent::File(pa, ma), Ent::File(pb, mb)) => ma == mb && same_content(pa, pb),
        (Ent::Dir(_), Ent::Dir(_)) => true,
        _ => false,
    }
}

fn copy_ent(e: &Ent, src: &Path, dst: &Path) -> Result<()> {
    match e {
        Ent::None | Ent::Other => Ok(()),
        Ent::Dir(_) => copy_tree(src, dst),
        Ent::File(p, mode) => {
            crate::fsutil::copy_file(p, dst).context(dst.display())?;
            fs::set_permissions(dst, fs::Permissions::from_mode(mode | 0o600))
                .context(dst.display())?;
            Ok(())
        }
        Ent::Link(t) => {
            symlink(t, dst).context(dst.display())?;
            Ok(())
        }
    }
}

/// Recursive copy preserving modes; faked hard links (l2s) are materialised as regular files.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let md = fs::metadata(src).context(src.display())?;
    fs::create_dir_all(dst).context(dst.display())?;
    for ent in fs::read_dir(src).context(src.display())? {
        let ent = ent?;
        let s = ent.path();
        let d = dst.join(ent.file_name());
        let e = stat(Some(s.clone()));
        copy_ent(&e, &s, &d)?;
    }
    fs::set_permissions(dst, fs::Permissions::from_mode((md.mode() & 0o777) | 0o700))
        .context(dst.display())?;
    Ok(())
}

fn names(dirs: &[Option<&Path>]) -> BTreeSet<OsString> {
    let mut s = BTreeSet::new();
    for d in dirs.iter().flatten() {
        if let Ok(rd) = fs::read_dir(d) {
            for e in rd.flatten() {
                s.insert(e.file_name());
            }
        }
    }
    s
}

fn trees_equal(a: &Path, b: &Path) -> bool {
    let (ea, eb) = (stat(Some(a.to_path_buf())), stat(Some(b.to_path_buf())));
    match (&ea, &eb) {
        (Ent::Dir(ma), Ent::Dir(mb)) => {
            if ma != mb {
                return false;
            }
            names(&[Some(a), Some(b)])
                .iter()
                .all(|n| trees_equal(&a.join(n), &b.join(n)))
        }
        _ => same(&ea, &eb),
    }
}

struct Merger {
    st: MergeStats,
}

impl Merger {
    fn dir(&mut self, op: Option<&Path>, ol: &Path, np: Option<&Path>, out: &Path) -> Result<()> {
        for n in names(&[op, Some(ol), np]) {
            let (cop, col, cnp) = (op.map(|p| p.join(&n)), ol.join(&n), np.map(|p| p.join(&n)));
            let eop = stat(cop.clone());
            let eol = stat(Some(col.clone()));
            // A file the image never had that is empty with mode 000 is proot's placeholder for a bind
            // target (Selkie binds its own /etc/hostname): it is no local change, and unreadable anyway.
            let eol = if matches!(eop, Ent::None) && is_bind_placeholder(&eol) {
                Ent::None
            } else {
                eol
            };
            let enp = stat(cnp.clone());
            let dst = out.join(&n);
            match (&eol, &enp) {
                (Ent::Dir(mol), Ent::Dir(mnp)) => {
                    fs::create_dir(&dst).context(dst.display())?;
                    let mode = match &eop {
                        Ent::Dir(mop) if mop == mol => *mnp,
                        _ => *mol,
                    };
                    let sub_op = if matches!(eop, Ent::Dir(_)) {
                        cop.as_deref()
                    } else {
                        None
                    };
                    self.dir(sub_op, &col, cnp.as_deref(), &dst)?;
                    fs::set_permissions(&dst, fs::Permissions::from_mode(mode | 0o700))?;
                }
                (Ent::Dir(_), _) => {
                    let unchanged = matches!(eop, Ent::Dir(_))
                        && cop
                            .as_deref()
                            .map(|p| trees_equal(p, &col))
                            .unwrap_or(false);
                    if unchanged {
                        copy_ent(&enp, cnp.as_deref().unwrap_or(Path::new("")), &dst)?;
                        self.st.taken_new += 1;
                    } else {
                        copy_tree(&col, &dst)?;
                        self.st.kept_local += 1;
                    }
                }
                (Ent::None, _) => {
                    if matches!(eop, Ent::None) {
                        // new in the image
                        copy_ent(&enp, cnp.as_deref().unwrap_or(Path::new("")), &dst)?;
                        self.st.taken_new += 1;
                    } else {
                        self.st.kept_deleted += 1; // deleted locally: stays deleted
                    }
                }
                _ => {
                    if same(&eol, &eop) {
                        copy_ent(&enp, cnp.as_deref().unwrap_or(Path::new("")), &dst)?;
                        self.st.taken_new += 1;
                    } else {
                        copy_ent(&eol, &col, &dst)?;
                        self.st.kept_local += 1;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Merge into `out` (must not exist). With no old live tree this is a plain copy of `new_pristine`.
pub fn merge(
    old_pristine: Option<&Path>,
    old_live: Option<&Path>,
    new_pristine: &Path,
    out: &Path,
) -> Result<MergeStats> {
    let mut m = Merger {
        st: MergeStats::default(),
    };
    match old_live {
        Some(ol) if ol.is_dir() => {
            fs::create_dir(out).context(out.display())?;
            let op = old_pristine.filter(|p| p.is_dir());
            let np = if new_pristine.is_dir() {
                Some(new_pristine)
            } else {
                None
            };
            m.dir(op, ol, np, out)?;
            let mode = fs::metadata(ol)
                .map(|md| md.mode() & 0o777)
                .unwrap_or(0o755);
            fs::set_permissions(out, fs::Permissions::from_mode(mode | 0o700))?;
        }
        _ => {
            if new_pristine.is_dir() {
                copy_tree(new_pristine, out)?;
            } else {
                fs::create_dir(out)?;
            }
        }
    }
    Ok(m.st)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct T {
        _d: tempfile::TempDir,
        op: PathBuf,
        ol: PathBuf,
        np: PathBuf,
        out: PathBuf,
    }
    fn t() -> T {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        for n in ["op", "ol", "np"] {
            fs::create_dir(r.join(n)).unwrap();
        }
        T {
            op: r.join("op"),
            ol: r.join("ol"),
            np: r.join("np"),
            out: r.join("out"),
            _d: d,
        }
    }
    fn w(root: &Path, p: &str, c: &str, mode: u32) {
        let f = root.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, c).unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(mode)).unwrap();
    }
    fn rd(root: &Path, p: &str) -> Option<String> {
        fs::read_to_string(root.join(p)).ok()
    }

    #[test]
    fn three_way_cases() {
        let t = t();
        // unchanged locally -> new wins
        for r in [&t.op, &t.ol] {
            w(r, "same.conf", "v1", 0o644);
            w(r, "edited.conf", "v1", 0o644);
            w(r, "deleted.conf", "v1", 0o644);
            w(r, "removed-upstream", "v1", 0o644);
            w(r, "sub/x", "v1", 0o644);
        }
        w(&t.ol, "edited.conf", "LOCAL", 0o644);
        w(&t.ol, "added-local", "mine", 0o600);
        fs::remove_file(t.ol.join("deleted.conf")).unwrap();
        w(&t.np, "same.conf", "v2", 0o644);
        w(&t.np, "edited.conf", "v2", 0o644);
        w(&t.np, "deleted.conf", "v2", 0o644);
        w(&t.np, "brand-new", "n", 0o644);
        w(&t.np, "sub/x", "v2", 0o644);
        w(&t.np, "sub/y", "v2y", 0o644);
        let st = merge(Some(&t.op), Some(&t.ol), &t.np, &t.out).unwrap();
        assert_eq!(rd(&t.out, "same.conf").unwrap(), "v2");
        assert_eq!(rd(&t.out, "edited.conf").unwrap(), "LOCAL");
        assert_eq!(rd(&t.out, "deleted.conf"), None);
        assert_eq!(rd(&t.out, "removed-upstream"), None);
        assert_eq!(rd(&t.out, "added-local").unwrap(), "mine");
        assert_eq!(
            fs::metadata(t.out.join("added-local")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(rd(&t.out, "brand-new").unwrap(), "n");
        assert_eq!(rd(&t.out, "sub/x").unwrap(), "v2");
        assert_eq!(rd(&t.out, "sub/y").unwrap(), "v2y");
        assert_eq!(st.kept_deleted, 1);
        assert!(st.kept_local >= 2);
    }

    #[test]
    fn mode_only_change_counts_as_local_edit() {
        let t = t();
        w(&t.op, "f", "x", 0o644);
        w(&t.ol, "f", "x", 0o600);
        w(&t.np, "f", "y", 0o644);
        merge(Some(&t.op), Some(&t.ol), &t.np, &t.out).unwrap();
        assert_eq!(rd(&t.out, "f").unwrap(), "x");
        assert_eq!(fs::metadata(t.out.join("f")).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn symlinks_and_dirs() {
        let t = t();
        symlink("a", t.op.join("l1")).unwrap();
        symlink("a", t.ol.join("l1")).unwrap();
        symlink("b", t.np.join("l1")).unwrap(); // unchanged locally -> new target
        symlink("a", t.op.join("l2")).unwrap();
        symlink("LOCAL", t.ol.join("l2")).unwrap();
        symlink("b", t.np.join("l2")).unwrap(); // local change kept
                                                // directory added locally with content; directory removed upstream but locally modified
        w(&t.ol, "mydir/f", "1", 0o644);
        w(&t.op, "olddir/f", "1", 0o644);
        w(&t.ol, "olddir/f", "1", 0o644);
        w(&t.ol, "olddir/extra", "2", 0o644);
        // directory removed upstream, untouched locally -> gone
        w(&t.op, "cleandir/f", "1", 0o644);
        w(&t.ol, "cleandir/f", "1", 0o644);
        // file replaced by a directory upstream, untouched locally
        w(&t.op, "f2d", "x", 0o644);
        w(&t.ol, "f2d", "x", 0o644);
        w(&t.np, "f2d/inner", "i", 0o644);
        // symlink-to-file locally changed to a regular file
        merge(Some(&t.op), Some(&t.ol), &t.np, &t.out).unwrap();
        assert_eq!(fs::read_link(t.out.join("l1")).unwrap(), PathBuf::from("b"));
        assert_eq!(
            fs::read_link(t.out.join("l2")).unwrap(),
            PathBuf::from("LOCAL")
        );
        assert_eq!(rd(&t.out, "mydir/f").unwrap(), "1");
        assert_eq!(rd(&t.out, "olddir/extra").unwrap(), "2");
        assert_eq!(rd(&t.out, "olddir/f").unwrap(), "1");
        assert!(!t.out.join("cleandir").exists());
        assert_eq!(rd(&t.out, "f2d/inner").unwrap(), "i");
    }

    #[test]
    fn proot_bind_placeholder_is_not_a_local_file() {
        let t = t();
        w(&t.op, "keep", "k", 0o644);
        w(&t.ol, "keep", "k", 0o644);
        // proot's placeholder for a bind target the image lacks: empty, mode 000, unreadable
        w(&t.ol, "hostname", "", 0o000);
        let st = merge(Some(&t.op), Some(&t.ol), &t.np, &t.out).unwrap();
        assert!(!t.out.join("hostname").exists());
        assert_eq!(st.kept_local, 0);
        // a new image that ships the file gets its own
        let t = super::tests::t();
        w(&t.ol, "hostname", "", 0o000);
        w(&t.np, "hostname", "img\n", 0o644);
        merge(Some(&t.op), Some(&t.ol), &t.np, &t.out).unwrap();
        assert_eq!(rd(&t.out, "hostname").unwrap(), "img\n");
    }

    #[test]
    fn first_deployment_is_plain_copy() {
        let t = t();
        w(&t.np, "a/b", "x", 0o640);
        symlink("a/b", t.np.join("lnk")).unwrap();
        let st = merge(None, None, &t.np, &t.out).unwrap();
        assert_eq!(rd(&t.out, "a/b").unwrap(), "x");
        assert_eq!(
            fs::metadata(t.out.join("a/b")).unwrap().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::read_link(t.out.join("lnk")).unwrap(),
            PathBuf::from("a/b")
        );
        assert_eq!(st, MergeStats::default());
    }

    #[test]
    fn dir_mode_follows_local_only_if_changed() {
        let t = t();
        for r in [&t.op, &t.ol, &t.np] {
            fs::create_dir(r.join("d")).unwrap();
        }
        fs::set_permissions(t.op.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(t.ol.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(t.np.join("d"), fs::Permissions::from_mode(0o750)).unwrap();
        merge(Some(&t.op), Some(&t.ol), &t.np, &t.out).unwrap();
        assert_eq!(fs::metadata(t.out.join("d")).unwrap().mode() & 0o777, 0o750);
    }
}
