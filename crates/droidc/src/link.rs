//! Hard-link strategies used while unpacking layers.
//!
//! Android's SELinux policy (and some filesystems) deny `link(2)` for app uids, and proot can only
//! emulate hard links through its `link2symlink` extension. [`Link2Symlink`] therefore produces the
//! exact on-disk structure proot's extension creates, so that a guest running under
//! `proot --link2symlink` sees ordinary hard links (with a correct `st_nlink`).
//!
//! # link2symlink on-disk format (as created by `proot --link2symlink`, prefix `.l2s.`)
//!
//! For a regular file `D/name` that gets a second link `E/other`:
//!
//! ```text
//! D/.l2s.name0001.0002   regular file: the real data (the "final" file)
//! D/.l2s.name0001        symlink -> <abs host path of D/.l2s.name0001.0002>   (the "intermediate")
//! D/name                 symlink -> <abs host path of D/.l2s.name0001>
//! E/other                symlink -> <abs host path of D/.l2s.name0001>
//! ```
//!
//! * `PREFIX` is `.l2s.` for the Termux build of proot (the userland fork uses `.proot.l2s.`).
//! * `NNNN` is a 4 digit decimal suffix (1..=9999) chosen so that `<prefix><name>NNNN.<count>` does not
//!   exist yet in the directory of the first link; it makes the chain unique per name.
//! * `<count>` is the link count, zero padded to at least 4 digits (5 from 10000 on, max 9 digits).
//!   Adding a link renames the final file to `...<count+1>` and re-points the intermediate; removing
//!   a link does the reverse, and deletes intermediate and final when the count would reach zero.
//! * The symlink contents are absolute *host* paths: proot reads them with the host's `readlink`.
//!   They therefore embed the final location of the tree. [`PathMap`] maps the physical path being
//!   written (inside `staging/`) to the path the file will have once the deployment is renamed into
//!   `deployments/<id>/rootfs`, and `etc/` to `usr/etc/` (the image's `/etc` is moved there).
//!   Moving the whole root elsewhere breaks existing chains; they have to be recreated.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_L2S_PREFIX: &str = ".l2s.";

/// Maps physical paths (where files are written now) to their final host path.
#[derive(Debug, Clone)]
pub struct PathMap {
    pub phys_root: PathBuf,
    pub final_root: PathBuf,
    /// (physical var dir, destination the contents will be moved to)
    pub var: Option<(PathBuf, PathBuf)>,
}

impl PathMap {
    pub fn identity(root: &Path) -> PathMap {
        PathMap { phys_root: root.to_path_buf(), final_root: root.to_path_buf(), var: None }
    }

    pub fn final_path(&self, phys: &Path) -> PathBuf {
        if let Some((pv, dest)) = &self.var {
            if let Ok(rest) = phys.strip_prefix(pv) {
                return dest.join(rest);
            }
        }
        let rel = match phys.strip_prefix(&self.phys_root) {
            Ok(r) => r,
            Err(_) => return phys.to_path_buf(),
        };
        let mut comps = rel.components();
        if let Some(Component::Normal(first)) = comps.clone().next() {
            if first == "etc" {
                comps.next();
                return self.final_root.join("usr/etc").join(comps.as_path());
            }
        }
        self.final_root.join(rel)
    }

    /// Inverse of [`final_path`](Self::final_path) while the tree is still being built.
    pub fn phys_path(&self, fin: &Path) -> PathBuf {
        if let Some((pv, dest)) = &self.var {
            if let Ok(rest) = fin.strip_prefix(dest) {
                return pv.join(rest);
            }
        }
        match fin.strip_prefix(&self.final_root) {
            Ok(rel) => {
                if let Ok(rest) = rel.strip_prefix("usr/etc") {
                    if !self.phys_root.join("usr/etc").exists() {
                        return self.phys_root.join("etc").join(rest);
                    }
                }
                self.phys_root.join(rel)
            }
            Err(_) => fin.to_path_buf(),
        }
    }
}

pub trait LinkStrategy: Send {
    /// Requested strategy name.
    fn kind(&self) -> &'static str;
    /// What was actually done (for `auto`).
    fn used(&self) -> String {
        self.kind().to_string()
    }
    /// Make `link` another name for the existing non-directory `target`.
    fn link(&mut self, target: &Path, link: &Path, map: &PathMap) -> io::Result<()>;
    /// Called before a non-directory path is removed or replaced, so link counts stay right.
    fn before_remove(&mut self, _path: &Path, _map: &PathMap) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Auto,
    Hardlink,
    Link2Symlink,
    Copy,
}

impl LinkKind {
    pub fn parse(s: &str) -> Option<LinkKind> {
        Some(match s {
            "auto" => LinkKind::Auto,
            "hardlink" => LinkKind::Hardlink,
            "link2symlink" => LinkKind::Link2Symlink,
            "copy" => LinkKind::Copy,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Auto => "auto",
            LinkKind::Hardlink => "hardlink",
            LinkKind::Link2Symlink => "link2symlink",
            LinkKind::Copy => "copy",
        }
    }
}

pub fn make_strategy(kind: LinkKind, l2s_prefix: &str) -> Box<dyn LinkStrategy> {
    match kind {
        LinkKind::Hardlink => Box::new(HardLink),
        LinkKind::Copy => Box::new(CopyLink),
        LinkKind::Link2Symlink => Box::new(Link2Symlink::new(l2s_prefix)),
        LinkKind::Auto => Box::new(AutoLink::new(l2s_prefix)),
    }
}

pub struct HardLink;

impl LinkStrategy for HardLink {
    fn kind(&self) -> &'static str {
        "hardlink"
    }
    fn link(&mut self, target: &Path, link: &Path, _map: &PathMap) -> io::Result<()> {
        fs::hard_link(target, link)
    }
}

pub struct CopyLink;

impl LinkStrategy for CopyLink {
    fn kind(&self) -> &'static str {
        "copy"
    }
    fn link(&mut self, target: &Path, link: &Path, _map: &PathMap) -> io::Result<()> {
        copy_entry(target, link)
    }
}

fn copy_entry(target: &Path, link: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(target)?;
    if md.file_type().is_symlink() {
        symlink(fs::read_link(target)?, link)
    } else {
        fs::copy(target, link).map(|_| ())
    }
}

pub struct Link2Symlink {
    prefix: String,
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

fn os_err(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

impl Link2Symlink {
    pub fn new(prefix: &str) -> Self {
        Link2Symlink { prefix: prefix.to_string() }
    }

    fn is_chain_content(&self, content: &Path) -> bool {
        content.file_name().map(|n| n.as_bytes().starts_with(self.prefix.as_bytes())).unwrap_or(false)
    }

    fn first_link(&self, target: &Path, link: &Path, map: &PathMap) -> io::Result<()> {
        let dir = target.parent().ok_or_else(|| os_err(22))?;
        let name = target.file_name().ok_or_else(|| os_err(22))?;
        for n in 1..=9999u32 {
            let mut inter = OsString::from(&self.prefix);
            inter.push(name);
            inter.push(format!("{n:04}"));
            let mut fin = inter.clone();
            fin.push(".0002");
            let p_i = dir.join(&inter);
            let p_f = dir.join(&fin);
            if fs::symlink_metadata(&p_f).is_ok() {
                continue;
            }
            match symlink(map.final_path(&p_f), &p_i) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            if let Err(e) = fs::rename(target, &p_f) {
                let _ = fs::remove_file(&p_i);
                return Err(e);
            }
            let c_i = map.final_path(&p_i);
            let undo = |_: ()| {
                let _ = fs::remove_file(target);
                let _ = fs::rename(&p_f, target);
                let _ = fs::remove_file(&p_i);
            };
            if let Err(e) = symlink(&c_i, target) {
                undo(());
                return Err(e);
            }
            if let Err(e) = symlink(&c_i, link) {
                undo(());
                return Err(e);
            }
            return Ok(());
        }
        Err(os_err(31)) // EMLINK
    }

    /// `target` is itself a faked hard link; add one more name to its chain.
    fn add_to_chain(&self, c_i: &Path, link: &Path, map: &PathMap) -> io::Result<()> {
        let p_i = map.phys_path(c_i);
        let c_f = fs::read_link(&p_i)?;
        let p_f = map.phys_path(&c_f);
        let (base, count) = split_count(&p_f).ok_or_else(|| os_err(2))?;
        if count >= 999_999_999 {
            return Err(os_err(31));
        }
        let new_f = with_count(&base, count + 1);
        fs::rename(&p_f, &new_f)?;
        fs::remove_file(&p_i)?;
        symlink(map.final_path(&new_f), &p_i)?;
        symlink(c_i, link)
    }
}

/// Split `<dir>/<prefix><name>NNNN.<count>` into (`<dir>/<prefix><name>NNNN.`, count).
fn split_count(p_f: &Path) -> Option<(PathBuf, u64)> {
    let name = p_f.file_name()?.as_bytes();
    let dot = name.iter().rposition(|&b| b == b'.')?;
    let digits = &name[dot + 1..];
    if digits.len() < 4 || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let count: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    let base = p_f.with_file_name(OsStr::from_bytes(&name[..=dot]));
    Some((base, count))
}

fn with_count(base: &Path, count: u64) -> PathBuf {
    let mut s = base.as_os_str().to_os_string().into_vec();
    s.extend_from_slice(format!("{count:04}").as_bytes());
    PathBuf::from(OsString::from_vec(s))
}

impl LinkStrategy for Link2Symlink {
    fn kind(&self) -> &'static str {
        "link2symlink"
    }

    fn link(&mut self, target: &Path, link: &Path, map: &PathMap) -> io::Result<()> {
        let md = fs::symlink_metadata(target)?;
        if md.is_dir() {
            return Err(os_err(1));
        }
        let res = if md.file_type().is_symlink() {
            let content = fs::read_link(target)?;
            if self.is_chain_content(&content) {
                self.add_to_chain(&content, link, map)
            } else {
                // An ordinary symlink: proot also just duplicates it.
                symlink(&content, link)
            }
        } else {
            self.first_link(target, link, map)
        };
        match res {
            // Name too long for the extra suffixes, or chain full: fall back to a plain copy.
            Err(e) if matches!(errno(&e), 36 | 31) => copy_entry(target, link),
            other => other,
        }
    }

    fn before_remove(&mut self, path: &Path, map: &PathMap) -> io::Result<()> {
        let md = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        if !md.file_type().is_symlink() {
            return Ok(());
        }
        let c_i = fs::read_link(path)?;
        if !self.is_chain_content(&c_i) {
            return Ok(());
        }
        let p_i = map.phys_path(&c_i);
        let c_f = match fs::read_link(&p_i) {
            Ok(c) => c,
            Err(_) => return Ok(()), // broken chain: nothing to maintain
        };
        let p_f = map.phys_path(&c_f);
        let Some((base, count)) = split_count(&p_f) else { return Ok(()) };
        if count > 1 {
            let new_f = with_count(&base, count - 1);
            fs::rename(&p_f, &new_f)?;
            fs::remove_file(&p_i)?;
            symlink(map.final_path(&new_f), &p_i)?;
        } else {
            fs::remove_file(&p_i)?;
            fs::remove_file(&p_f)?;
        }
        Ok(())
    }
}

/// `link(2)` first; on EPERM/EACCES/EXDEV switch (for the rest of the unpack) to link2symlink.
pub struct AutoLink {
    l2s: Link2Symlink,
    fell_back: bool,
    used_hard: bool,
    used_l2s: bool,
}

impl AutoLink {
    pub fn new(prefix: &str) -> Self {
        AutoLink { l2s: Link2Symlink::new(prefix), fell_back: false, used_hard: false, used_l2s: false }
    }
}

impl LinkStrategy for AutoLink {
    fn kind(&self) -> &'static str {
        "auto"
    }
    fn used(&self) -> String {
        match (self.used_hard, self.used_l2s) {
            (true, true) => "hardlink+link2symlink".into(),
            (false, true) => "link2symlink".into(),
            (true, false) => "hardlink".into(),
            (false, false) => "auto".into(),
        }
    }
    fn link(&mut self, target: &Path, link: &Path, map: &PathMap) -> io::Result<()> {
        if !self.fell_back {
            match fs::hard_link(target, link) {
                Ok(()) => {
                    self.used_hard = true;
                    return Ok(());
                }
                Err(e) if matches!(errno(&e), 1 | 13 | 18) => self.fell_back = true,
                Err(e) => return Err(e),
            }
        }
        self.used_l2s = true;
        self.l2s.link(target, link, map)
    }
    fn before_remove(&mut self, path: &Path, map: &PathMap) -> io::Result<()> {
        if self.used_l2s {
            self.l2s.before_remove(path, map)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn setup() -> (tempfile::TempDir, PathMap) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        let map = PathMap::identity(&root);
        (d, map)
    }

    #[test]
    fn l2s_layout_matches_proot() {
        let (_d, map) = setup();
        let r = &map.phys_root;
        fs::write(r.join("bin/foo"), b"DATA").unwrap();
        let mut s = Link2Symlink::new(DEFAULT_L2S_PREFIX);
        s.link(&r.join("bin/foo"), &r.join("lib/bar"), &map).unwrap();

        let fin = r.join("bin/.l2s.foo0001.0002");
        let inter = r.join("bin/.l2s.foo0001");
        assert_eq!(fs::read(&fin).unwrap(), b"DATA");
        assert!(fs::symlink_metadata(&fin).unwrap().file_type().is_file());
        assert_eq!(fs::read_link(&inter).unwrap(), fin);
        assert_eq!(fs::read_link(r.join("bin/foo")).unwrap(), inter);
        assert_eq!(fs::read_link(r.join("lib/bar")).unwrap(), inter);
        assert_eq!(fs::read(r.join("lib/bar")).unwrap(), b"DATA");

        // a third link bumps the count and re-points the intermediate
        s.link(&r.join("bin/foo"), &r.join("lib/baz"), &map).unwrap();
        let fin3 = r.join("bin/.l2s.foo0001.0003");
        assert!(fs::symlink_metadata(&fin).is_err());
        assert_eq!(fs::read(&fin3).unwrap(), b"DATA");
        assert_eq!(fs::read_link(&inter).unwrap(), fin3);
        assert_eq!(fs::read_link(r.join("lib/baz")).unwrap(), inter);
        // linking via another name of the chain works too
        s.link(&r.join("lib/bar"), &r.join("lib/qux"), &map).unwrap();
        assert!(fs::symlink_metadata(r.join("bin/.l2s.foo0001.0004")).is_ok());
    }

    #[test]
    fn l2s_suffix_collision_picks_next() {
        let (_d, map) = setup();
        let r = &map.phys_root;
        fs::write(r.join("bin/foo"), b"1").unwrap();
        fs::write(r.join("bin/.l2s.foo0001.0002"), b"stale").unwrap();
        let mut s = Link2Symlink::new(DEFAULT_L2S_PREFIX);
        s.link(&r.join("bin/foo"), &r.join("lib/bar"), &map).unwrap();
        assert_eq!(fs::read(r.join("bin/.l2s.foo0002.0002")).unwrap(), b"1");
    }

    #[test]
    fn l2s_replacing_target_keeps_other_links() {
        let (_d, map) = setup();
        let r = &map.phys_root;
        fs::write(r.join("bin/foo"), b"OLD").unwrap();
        let mut s = Link2Symlink::new(DEFAULT_L2S_PREFIX);
        s.link(&r.join("bin/foo"), &r.join("lib/bar"), &map).unwrap();
        s.link(&r.join("bin/foo"), &r.join("lib/baz"), &map).unwrap(); // count 3
        // a later layer replaces bin/foo
        s.before_remove(&r.join("bin/foo"), &map).unwrap();
        fs::remove_file(r.join("bin/foo")).unwrap();
        fs::write(r.join("bin/foo"), b"NEW").unwrap();
        assert_eq!(fs::read(r.join("bin/foo")).unwrap(), b"NEW");
        assert_eq!(fs::read(r.join("lib/bar")).unwrap(), b"OLD");
        assert_eq!(fs::read(r.join("lib/baz")).unwrap(), b"OLD");
        assert!(fs::symlink_metadata(r.join("bin/.l2s.foo0001.0002")).is_ok());
        // dropping the remaining names deletes the chain
        for p in ["lib/bar", "lib/baz"] {
            s.before_remove(&r.join(p), &map).unwrap();
            fs::remove_file(r.join(p)).unwrap();
        }
        assert!(fs::symlink_metadata(r.join("bin/.l2s.foo0001")).is_err());
        assert!(fs::symlink_metadata(r.join("bin/.l2s.foo0001.0001")).is_err());
    }

    #[test]
    fn l2s_paths_follow_final_location() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let phys = root.join("staging/x/rootfs");
        let fin = root.join("deployments/x/rootfs");
        fs::create_dir_all(phys.join("etc")).unwrap();
        fs::create_dir_all(phys.join("usr/bin")).unwrap();
        let map = PathMap { phys_root: phys.clone(), final_root: fin.clone(), var: None };
        fs::write(phys.join("etc/a.conf"), b"x").unwrap();
        let mut s = Link2Symlink::new(DEFAULT_L2S_PREFIX);
        s.link(&phys.join("etc/a.conf"), &phys.join("usr/bin/b.conf"), &map).unwrap();
        // contents point at the post-rename location, with etc -> usr/etc
        assert_eq!(fs::read_link(phys.join("usr/bin/b.conf")).unwrap(), fin.join("usr/etc/.l2s.a.conf0001"));
        assert_eq!(fs::read_link(phys.join("etc/a.conf")).unwrap(), fin.join("usr/etc/.l2s.a.conf0001"));
        assert_eq!(map.phys_path(&fin.join("usr/etc/x")), phys.join("etc/x"));
        // a chain can still be extended and unlinked before the move
        s.link(&phys.join("usr/bin/b.conf"), &phys.join("usr/bin/c.conf"), &map).unwrap();
        s.before_remove(&phys.join("usr/bin/c.conf"), &map).unwrap();
        assert!(fs::symlink_metadata(phys.join("etc/.l2s.a.conf0001.0002")).is_ok());
    }

    #[test]
    fn hardlink_and_copy() {
        let (_d, map) = setup();
        let r = &map.phys_root;
        fs::write(r.join("bin/foo"), b"D").unwrap();
        HardLink.link(&r.join("bin/foo"), &r.join("lib/h"), &map).unwrap();
        assert_eq!(fs::metadata(r.join("lib/h")).unwrap().nlink(), 2);
        CopyLink.link(&r.join("bin/foo"), &r.join("lib/c"), &map).unwrap();
        assert_eq!(fs::metadata(r.join("lib/c")).unwrap().nlink(), 1);
        assert_eq!(fs::read(r.join("lib/c")).unwrap(), b"D");
    }

    #[test]
    fn auto_uses_hardlink_when_possible() {
        let (_d, map) = setup();
        let r = &map.phys_root;
        fs::write(r.join("bin/foo"), b"D").unwrap();
        let mut a = AutoLink::new(DEFAULT_L2S_PREFIX);
        a.link(&r.join("bin/foo"), &r.join("lib/h"), &map).unwrap();
        assert_eq!(a.used(), "hardlink");
        assert_eq!(fs::metadata(r.join("lib/h")).unwrap().nlink(), 2);
    }

    #[test]
    fn auto_falls_back_on_eperm() {
        // Simulate an EPERM from link(2) by forcing the fallback state.
        let (_d, map) = setup();
        let r = &map.phys_root;
        fs::write(r.join("bin/foo"), b"D").unwrap();
        let mut a = AutoLink::new(DEFAULT_L2S_PREFIX);
        a.fell_back = true;
        a.link(&r.join("bin/foo"), &r.join("lib/h"), &map).unwrap();
        assert_eq!(a.used(), "link2symlink");
        assert!(fs::symlink_metadata(r.join("bin/.l2s.foo0001")).is_ok());
    }

    #[test]
    fn count_naming() {
        let p = PathBuf::from("/x/.l2s.foo0001.0009");
        let (b, c) = split_count(&p).unwrap();
        assert_eq!(c, 9);
        assert_eq!(with_count(&b, 10), PathBuf::from("/x/.l2s.foo0001.0010"));
        assert_eq!(with_count(&b, 10000), PathBuf::from("/x/.l2s.foo0001.10000"));
    }
}
