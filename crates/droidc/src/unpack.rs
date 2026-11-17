//! Layer unpacking: whiteouts, drop rules, safe extraction, metadata capture.

use crate::error::{Context, Result};
use crate::events::Emitter;
use crate::link::{LinkStrategy, PathMap};
use base64::Engine;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Directories whose contents are dropped (the directory itself is kept, empty).
const DROP_DIRS: [&str; 4] = [
    "boot",
    "usr/lib/modules",
    "usr/lib/firmware",
    "usr/lib/ostree-boot",
];

/// Whether an image path (relative, no leading slash) is dropped.
pub fn is_dropped(rel: &str) -> bool {
    for d in DROP_DIRS {
        if let Some(rest) = rel.strip_prefix(d) {
            if rest.starts_with('/') {
                return true;
            }
        }
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let in_boot_area = rel.starts_with("boot/") || rel.starts_with("usr/lib/");
    if in_boot_area && (base.starts_with("vmlinuz") || base.starts_with("initramfs")) {
        return true;
    }
    if rel.starts_with("usr/lib/systemd/boot/")
        && (base.ends_with(".efi") || base.ends_with(".efi.signed"))
    {
        return true;
    }
    false
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Meta {
    /// f=file d=dir l=symlink h=hardlink
    pub t: &'static str,
    pub u: u64,
    pub g: u64,
    /// Full mode including setuid/setgid/sticky.
    pub m: u32,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub x: BTreeMap<String, String>,
    /// Symlink target or hard-link target path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UnpackStats {
    pub files: u64,
    pub hardlinks: u64,
    pub skipped: u64,
    pub dropped: u64,
}

pub struct Unpacker<'a> {
    root: PathBuf,
    map: PathMap,
    strategy: Box<dyn LinkStrategy>,
    skip_var: bool,
    pub meta: BTreeMap<String, Meta>,
    pub stats: UnpackStats,
    em: &'a Emitter,
    verified_parent: Option<String>,
}

enum Flow {
    Done,
    Skip(String),
}

/// Open a layer file, sniffing the compression (gzip, zstd, or plain tar).
pub fn open_layer(path: &Path) -> Result<Box<dyn Read>> {
    let mut f = File::open(path).context(path.display())?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    drop(f);
    let f = File::open(path)?;
    let r = BufReader::with_capacity(256 * 1024, f);
    if n >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        Ok(Box::new(flate2::read::GzDecoder::new(r)))
    } else if n >= 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Ok(Box::new(zstd::stream::read::Decoder::with_buffer(r)?))
    } else {
        Ok(Box::new(r))
    }
}

fn normalize(p: &Path) -> std::result::Result<String, String> {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => return Err(format!("path escapes root: {}", p.display())),
            Component::Prefix(_) => return Err("unsupported path prefix".into()),
        }
    }
    Ok(parts.join("/"))
}

impl<'a> Unpacker<'a> {
    pub fn new(
        root: PathBuf,
        map: PathMap,
        strategy: Box<dyn LinkStrategy>,
        skip_var: bool,
        em: &'a Emitter,
    ) -> Self {
        Unpacker {
            root,
            map,
            strategy,
            skip_var,
            meta: BTreeMap::new(),
            stats: UnpackStats::default(),
            em,
            verified_parent: None,
        }
    }

    pub fn strategy_used(&self) -> String {
        self.strategy.used()
    }

    fn phys(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Verify (creating missing directories) that no component of `rel`'s parent is a symlink or
    /// a non-directory. Returns Err(reason) for entries that must be skipped.
    fn ensure_parent(
        &mut self,
        rel: &str,
        create: bool,
    ) -> Result<std::result::Result<(), String>> {
        let parent = match rel.rfind('/') {
            Some(i) => &rel[..i],
            None => return Ok(Ok(())),
        };
        if self.verified_parent.as_deref() == Some(parent) {
            return Ok(Ok(()));
        }
        let mut cur = self.root.clone();
        for comp in parent.split('/') {
            cur.push(comp);
            match fs::symlink_metadata(&cur) {
                Ok(md) if md.is_dir() => {}
                Ok(md) if md.file_type().is_symlink() => {
                    return Ok(Err(format!("parent '{}' is a symlink", cur.display())));
                }
                Ok(_) => {
                    return Ok(Err(format!(
                        "parent '{}' is not a directory",
                        cur.display()
                    )))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !create {
                        return Ok(Err("parent missing".into()));
                    }
                    fs::create_dir(&cur).context(cur.display())?;
                    fs::set_permissions(&cur, fs::Permissions::from_mode(0o755))?;
                }
                Err(e) => return Err(e).context(cur.display()),
            }
        }
        self.verified_parent = Some(parent.to_string());
        Ok(Ok(()))
    }

    fn forget_meta(&mut self, rel: &str) {
        self.meta.remove(rel);
        let prefix = format!("{rel}/");
        let keys: Vec<String> = self
            .meta
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.meta.remove(&k);
        }
    }

    /// Remove a path (file, symlink or whole directory tree) without following symlinks.
    fn remove_any(&mut self, path: &Path) -> Result<()> {
        self.verified_parent = None;
        let md = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).context(path.display()),
        };
        if md.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            for ent in fs::read_dir(path).context(path.display())? {
                let ent = ent?;
                self.remove_any(&ent.path())?;
            }
            fs::remove_dir(path).context(path.display())?;
        } else {
            self.strategy
                .before_remove(path, &self.map)
                .context(path.display())?;
            fs::remove_file(path).context(path.display())?;
        }
        Ok(())
    }

    fn clear_dir(&mut self, dir: &Path) -> Result<()> {
        match fs::symlink_metadata(dir) {
            Ok(md) if md.is_dir() => {}
            _ => return Ok(()),
        }
        for ent in fs::read_dir(dir).context(dir.display())? {
            let ent = ent?;
            self.remove_any(&ent.path())?;
        }
        Ok(())
    }

    /// Apply one layer (an already decompressed tar stream).
    pub fn apply_layer<R: Read>(&mut self, r: R) -> Result<()> {
        let mut ar = tar::Archive::new(r);
        ar.set_ignore_zeros(true);
        for entry in ar.entries().context("read tar")? {
            let entry = entry.context("read tar entry")?;
            match self.apply_entry(entry)? {
                Flow::Done => {}
                Flow::Skip(why) => {
                    self.stats.skipped += 1;
                    self.em.warn(&format!("skipped entry: {why}"));
                }
            }
        }
        Ok(())
    }

    fn apply_entry<R: Read>(&mut self, mut entry: tar::Entry<'_, R>) -> Result<Flow> {
        let raw = entry.path().context("entry path")?.into_owned();
        let rel = match normalize(&raw) {
            Ok(r) => r,
            Err(e) => return Ok(Flow::Skip(e)),
        };
        if rel.is_empty() {
            return Ok(Flow::Done);
        }
        let base = rel.rsplit('/').next().unwrap().to_string();
        let parent_rel = rel
            .rfind('/')
            .map(|i| rel[..i].to_string())
            .unwrap_or_default();

        // Whiteouts.
        if let Some(victim) = base.strip_prefix(".wh.") {
            if base == ".wh..wh..opq" {
                if let Err(why) = self.ensure_parent(&format!("{parent_rel}/x"), false)? {
                    let _ = why;
                    return Ok(Flow::Done);
                }
                let dir = self.phys(&parent_rel);
                self.clear_dir(&dir)?;
                let pr = parent_rel.clone();
                let prefix = format!("{pr}/");
                let keys: Vec<String> = self
                    .meta
                    .range(prefix.clone()..)
                    .take_while(|(k, _)| k.starts_with(&prefix))
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in keys {
                    self.meta.remove(&k);
                }
            } else {
                let vrel = if parent_rel.is_empty() {
                    victim.to_string()
                } else {
                    format!("{parent_rel}/{victim}")
                };
                if self.ensure_parent(&vrel, false)?.is_ok() {
                    let p = self.phys(&vrel);
                    self.remove_any(&p)?;
                    self.forget_meta(&vrel);
                }
            }
            return Ok(Flow::Done);
        }

        let etype = entry.header().entry_type();
        if is_dropped(&rel) || (self.skip_var && rel.starts_with("var/")) {
            self.stats.dropped += 1;
            return Ok(Flow::Done);
        }
        if let Err(why) = self.ensure_parent(&rel, true)? {
            return Ok(Flow::Skip(format!("{rel}: {why}")));
        }
        let path = self.phys(&rel);
        let hdr = entry.header();
        let mode = hdr.mode().unwrap_or(0o644);
        let uid = hdr.uid().unwrap_or(0);
        let gid = hdr.gid().unwrap_or(0);
        let mtime = hdr.mtime().unwrap_or(0);
        let mut xattrs = BTreeMap::new();
        if let Ok(Some(exts)) = entry.pax_extensions() {
            for e in exts.flatten() {
                if let Ok(k) = e.key() {
                    if let Some(name) = k.strip_prefix("SCHILY.xattr.") {
                        xattrs.insert(
                            name.to_string(),
                            base64::engine::general_purpose::STANDARD.encode(e.value_bytes()),
                        );
                    }
                }
            }
        }
        let mk = |t: &'static str, l: Option<String>| Meta {
            t,
            u: uid,
            g: gid,
            m: mode & 0o7777,
            x: xattrs.clone(),
            l,
        };

        use tar::EntryType as T;
        match etype {
            T::Directory => {
                match fs::symlink_metadata(&path) {
                    Ok(md) if md.is_dir() => {}
                    Ok(_) => {
                        self.remove_any(&path)?;
                        self.forget_meta(&rel);
                        fs::create_dir(&path).context(path.display())?;
                    }
                    Err(_) => fs::create_dir(&path).context(path.display())?,
                }
                fs::set_permissions(&path, fs::Permissions::from_mode((mode & 0o777) | 0o700))
                    .context(path.display())?;
                self.meta.insert(rel, mk("d", None));
            }
            T::Regular | T::Continuous | T::GNUSparse => {
                self.remove_any(&path)?;
                self.forget_meta(&rel);
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .context(path.display())?;
                std::io::copy(&mut entry, &mut f).context(path.display())?;
                f.set_permissions(fs::Permissions::from_mode((mode & 0o777) | 0o600))?;
                let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime));
                self.stats.files += 1;
                self.meta.insert(rel, mk("f", None));
            }
            T::Symlink => {
                let target = match entry.link_name().context("link name")? {
                    Some(t) => t.into_owned(),
                    None => return Ok(Flow::Skip(format!("{rel}: symlink without target"))),
                };
                self.remove_any(&path)?;
                self.forget_meta(&rel);
                symlink(&target, &path).context(path.display())?;
                self.stats.files += 1;
                self.meta
                    .insert(rel, mk("l", Some(target.to_string_lossy().into_owned())));
            }
            T::Link => {
                let traw = match entry.link_name().context("link name")? {
                    Some(t) => t.into_owned(),
                    None => return Ok(Flow::Skip(format!("{rel}: hardlink without target"))),
                };
                let trel = match normalize(&traw) {
                    Ok(t) => t,
                    Err(e) => return Ok(Flow::Skip(e)),
                };
                if is_dropped(&trel) || (self.skip_var && trel.starts_with("var/")) {
                    self.stats.dropped += 1;
                    return Ok(Flow::Done);
                }
                if let Err(why) = self.ensure_parent(&trel, false)? {
                    return Ok(Flow::Skip(format!("{rel}: link target {why}")));
                }
                // ensure_parent cached the *target's* parent; the entry's parent is re-verified next time.
                self.verified_parent = None;
                let tphys = self.phys(&trel);
                match fs::symlink_metadata(&tphys) {
                    Ok(md) if !md.is_dir() => {}
                    _ => return Ok(Flow::Skip(format!("{rel}: hardlink target {trel} missing"))),
                }
                self.remove_any(&path)?;
                self.forget_meta(&rel);
                self.strategy
                    .link(&tphys, &path, &self.map)
                    .context(format!("link {rel} -> {trel}"))?;
                self.stats.files += 1;
                self.stats.hardlinks += 1;
                self.meta.insert(rel, mk("h", Some(trel)));
            }
            _ => {
                return Ok(Flow::Skip(format!(
                    "{rel}: unsupported entry type {etype:?}"
                )))
            }
        }
        Ok(Flow::Done)
    }

    /// Finish: move the image's `/etc` to `usr/etc` and return the collected metadata.
    pub fn finish(mut self) -> Result<(BTreeMap<String, Meta>, UnpackStats, String)> {
        let etc = self.root.join("etc");
        if fs::symlink_metadata(&etc)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            let dest = self.root.join("usr/etc");
            fs::create_dir_all(self.root.join("usr")).context("usr")?;
            if fs::symlink_metadata(&dest).is_ok() {
                for ent in fs::read_dir(&etc)? {
                    let ent = ent?;
                    let d = dest.join(ent.file_name());
                    self.remove_any(&d)?;
                    fs::rename(ent.path(), d)?;
                }
                fs::remove_dir(&etc)?;
            } else {
                fs::rename(&etc, &dest).context("move etc to usr/etc")?;
            }
            let moved: Vec<(String, Meta)> = self
                .meta
                .iter()
                .filter(|(k, _)| *k == "etc" || k.starts_with("etc/"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (k, v) in moved {
                self.meta.remove(&k);
                self.meta.insert(format!("usr/{k}"), v);
            }
        }
        let used = self.strategy.used();
        Ok((self.meta, self.stats, used))
    }
}

/// Write `metadata.jsonl.zst`: one JSON object per path, sorted by path.
pub fn write_metadata(path: &Path, meta: &BTreeMap<String, Meta>) -> Result<()> {
    let f = File::create(path).context(path.display())?;
    let mut enc = zstd::stream::write::Encoder::new(f, 3)?;
    for (p, m) in meta {
        let mut v = serde_json::to_value(m)?;
        v.as_object_mut()
            .unwrap()
            .insert("p".into(), serde_json::Value::String(p.clone()));
        writeln!(enc, "{v}")?;
    }
    enc.finish()?.sync_all()?;
    Ok(())
}

pub fn read_metadata(path: &Path) -> Result<Vec<serde_json::Value>> {
    let f = File::open(path).context(path.display())?;
    let mut s = String::new();
    zstd::stream::read::Decoder::new(f)?.read_to_string(&mut s)?;
    s.lines().map(|l| Ok(serde_json::from_str(l)?)).collect()
}

#[allow(dead_code)]
fn _unused(_: SystemTime) {}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::events::testutil::capture;
    use crate::link::{make_strategy, LinkKind, DEFAULT_L2S_PREFIX};
    use std::os::unix::fs::MetadataExt;

    /// Tiny tar builder for synthetic layers.
    pub struct Layer(pub tar::Builder<Vec<u8>>);
    impl Default for Layer {
        fn default() -> Self {
            Self::new()
        }
    }
    impl Layer {
        pub fn new() -> Self {
            Layer(tar::Builder::new(Vec::new()))
        }
        fn hdr(t: tar::EntryType, mode: u32, size: u64) -> tar::Header {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(t);
            h.set_mode(mode);
            h.set_size(size);
            h.set_uid(1000);
            h.set_gid(1000);
            h.set_mtime(1_600_000_000);
            h
        }
        pub fn dir(mut self, p: &str, mode: u32) -> Self {
            let mut h = Self::hdr(tar::EntryType::Directory, mode, 0);
            self.0.append_data(&mut h, p, std::io::empty()).unwrap();
            self
        }
        pub fn file(mut self, p: &str, mode: u32, data: &[u8]) -> Self {
            let mut h = Self::hdr(tar::EntryType::Regular, mode, data.len() as u64);
            self.0.append_data(&mut h, p, data).unwrap();
            self
        }
        pub fn symlink(mut self, p: &str, target: &str) -> Self {
            let mut h = Self::hdr(tar::EntryType::Symlink, 0o777, 0);
            self.0.append_link(&mut h, p, target).unwrap();
            self
        }
        pub fn hardlink(mut self, p: &str, target: &str) -> Self {
            let mut h = Self::hdr(tar::EntryType::Link, 0o644, 0);
            self.0.append_link(&mut h, p, target).unwrap();
            self
        }
        pub fn bytes(self) -> Vec<u8> {
            self.0.into_inner().unwrap()
        }
    }

    pub fn unpack(
        layers: Vec<Vec<u8>>,
        kind: LinkKind,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        BTreeMap<String, Meta>,
        UnpackStats,
    ) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap().join("rootfs");
        fs::create_dir_all(&root).unwrap();
        let (em, _b) = capture();
        let map = PathMap::identity(&root);
        let mut u = Unpacker::new(
            root.clone(),
            map,
            make_strategy(kind, DEFAULT_L2S_PREFIX),
            false,
            &em,
        );
        for l in layers {
            u.apply_layer(&l[..]).unwrap();
        }
        let (meta, stats, _) = u.finish().unwrap();
        (d, root, meta, stats)
    }

    #[test]
    fn basic_and_etc_move_and_modes() {
        let l = Layer::new()
            .dir("etc/", 0o755)
            .file("etc/os-release", 0o644, b"NAME=x\n")
            .dir("usr/", 0o555)
            .dir("usr/bin/", 0o555)
            .file("usr/bin/su", 0o4755, b"bin")
            .file("usr/secret", 0o000, b"s")
            .symlink("bin", "usr/bin")
            .bytes();
        let (_d, root, meta, _) = unpack(vec![l], LinkKind::Hardlink);
        assert_eq!(
            fs::read(root.join("usr/etc/os-release")).unwrap(),
            b"NAME=x\n"
        );
        assert!(!root.join("etc").exists());
        // setuid dropped on disk, recorded in metadata
        let md = fs::metadata(root.join("usr/bin/su")).unwrap();
        assert_eq!(md.mode() & 0o7777, 0o755);
        assert_eq!(meta["usr/bin/su"].m, 0o4755);
        assert_eq!(meta["usr/bin/su"].u, 1000);
        // dirs forced owner-writable, files owner-readable
        assert_eq!(
            fs::metadata(root.join("usr/bin")).unwrap().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(root.join("usr/secret")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_link(root.join("bin")).unwrap(),
            PathBuf::from("usr/bin")
        );
        assert!(meta.contains_key("usr/etc/os-release"));
        assert!(!meta.contains_key("etc/os-release"));
    }

    #[test]
    fn whiteouts_and_opaque_dirs() {
        let l1 = Layer::new()
            .dir("d/", 0o755)
            .file("d/a", 0o644, b"a")
            .file("d/b", 0o644, b"b")
            .dir("o/", 0o755)
            .file("o/old", 0o644, b"old")
            .dir("gone/", 0o755)
            .file("gone/x", 0o644, b"x")
            .bytes();
        let l2 = Layer::new()
            .file("d/.wh.a", 0o644, b"")
            .dir("o/", 0o755)
            .file("o/.wh..wh..opq", 0o644, b"")
            .file("o/new", 0o644, b"new")
            .file(".wh.gone", 0o644, b"")
            .bytes();
        let (_d, root, meta, _) = unpack(vec![l1, l2], LinkKind::Hardlink);
        assert!(!root.join("d/a").exists());
        assert!(root.join("d/b").exists());
        assert!(!root.join("o/old").exists());
        assert_eq!(fs::read(root.join("o/new")).unwrap(), b"new");
        assert!(!root.join("gone").exists());
        assert!(!meta.contains_key("gone/x") && !meta.contains_key("gone"));
        assert!(!meta.contains_key("d/a") && !meta.contains_key("o/old"));
    }

    #[test]
    fn drop_rules() {
        assert!(is_dropped("boot/vmlinuz"));
        assert!(is_dropped("usr/lib/modules/6.1/kernel/x.ko"));
        assert!(is_dropped("usr/lib/firmware/a.bin"));
        assert!(is_dropped("usr/lib/ostree-boot/efi"));
        assert!(is_dropped("usr/lib/systemd/boot/efi/systemd-bootx64.efi"));
        assert!(is_dropped("usr/lib/vmlinuz-6.1"));
        assert!(!is_dropped("boot"));
        assert!(!is_dropped("usr/lib/modules"));
        assert!(!is_dropped("usr/lib/modules-load.d/x.conf"));
        assert!(!is_dropped("usr/lib64/foo.so"));
        assert!(!is_dropped("usr/share/doc/vmlinuz"));
        let l = Layer::new()
            .dir("boot/", 0o755)
            .file("boot/vmlinuz", 0o644, b"k")
            .dir("usr/", 0o755)
            .dir("usr/lib/", 0o755)
            .dir("usr/lib/modules/", 0o755)
            .file("usr/lib/modules/x", 0o644, b"m")
            .file("usr/keep", 0o644, b"k")
            .bytes();
        let (_d, root, _, st) = unpack(vec![l], LinkKind::Hardlink);
        assert!(root.join("boot").is_dir());
        assert!(!root.join("boot/vmlinuz").exists());
        assert!(!root.join("usr/lib/modules/x").exists());
        assert!(root.join("usr/keep").exists());
        assert_eq!(st.dropped, 2);
    }

    #[test]
    fn path_traversal_and_symlink_parents_are_safe() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let root = d.path().join("rootfs");
        fs::create_dir_all(&root).unwrap();
        let (em, _) = capture();
        let mut u = Unpacker::new(
            root.clone(),
            PathMap::identity(&root),
            make_strategy(LinkKind::Hardlink, ".l2s."),
            false,
            &em,
        );
        // hand-craft a header with ../ (the builder refuses, so write the bytes directly)
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        let name = b"../outside/evil";
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
        h.set_cksum();
        b.append(&h, &b"x"[..]).unwrap();
        // symlink pointing outside, then a file written "through" it
        let mut l = Layer::new();
        l = l
            .symlink("link", outside.to_str().unwrap())
            .file("link/pwn", 0o644, b"x");
        let mut hl = Layer::new();
        hl = hl.hardlink("hl", "../outside/evil");
        u.apply_layer(&b.into_inner().unwrap()[..]).unwrap();
        u.apply_layer(&l.bytes()[..]).unwrap();
        u.apply_layer(&hl.bytes()[..]).unwrap();
        assert!(!outside.join("evil").exists());
        assert!(!outside.join("pwn").exists());
        assert!(fs::symlink_metadata(root.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(u.stats.skipped >= 2);
    }

    #[test]
    fn replacing_file_does_not_write_through_hardlinks() {
        let l1 = Layer::new()
            .file("a", 0o644, b"OLD")
            .hardlink("b", "a")
            .bytes();
        let l2 = Layer::new().file("a", 0o644, b"NEW").bytes();
        for kind in [LinkKind::Hardlink, LinkKind::Link2Symlink, LinkKind::Copy] {
            let (_d, root, _, st) = unpack(vec![l1.clone(), l2.clone()], kind);
            assert_eq!(fs::read(root.join("a")).unwrap(), b"NEW", "{kind:?}");
            assert_eq!(fs::read(root.join("b")).unwrap(), b"OLD", "{kind:?}");
            assert_eq!(st.hardlinks, 1);
        }
    }

    #[test]
    fn cross_layer_hardlink_with_replaced_target() {
        // layer 1: f; layer 2: g hard-links f; layer 3 replaces f and deletes g's sibling.
        let l1 = Layer::new()
            .dir("x/", 0o755)
            .file("x/f", 0o755, b"ONE")
            .bytes();
        let l2 = Layer::new()
            .dir("y/", 0o755)
            .hardlink("y/g", "x/f")
            .hardlink("y/h", "x/f")
            .bytes();
        let l3 = Layer::new()
            .file("x/f", 0o755, b"TWO")
            .file("y/.wh.h", 0o644, b"")
            .bytes();
        for kind in [LinkKind::Hardlink, LinkKind::Link2Symlink] {
            let (_d, root, meta, st) = unpack(vec![l1.clone(), l2.clone(), l3.clone()], kind);
            assert_eq!(fs::read(root.join("x/f")).unwrap(), b"TWO");
            assert_eq!(fs::read(root.join("y/g")).unwrap(), b"ONE");
            assert!(!root.join("y/h").exists());
            assert_eq!(st.hardlinks, 2);
            assert_eq!(meta["y/g"].t, "h");
            if kind == LinkKind::Link2Symlink {
                // chain count: f,g,h -> 3; replacing f and whiting out h leaves only g (count 1)
                assert!(fs::symlink_metadata(root.join("x/.l2s.f0001.0001")).is_ok());
            }
        }
    }

    #[test]
    fn xattrs_recorded() {
        let mut b = tar::Builder::new(Vec::new());
        b.append_pax_extensions([("SCHILY.xattr.security.capability", &b"\x01\x02"[..])])
            .unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(1);
        h.set_mode(0o755);
        b.append_data(&mut h, "usr/bin/ping", &b"x"[..]).unwrap();
        let (_d, _root, meta, _) = unpack(vec![b.into_inner().unwrap()], LinkKind::Hardlink);
        assert_eq!(meta["usr/bin/ping"].x["security.capability"], "AQI=");
    }

    #[test]
    fn metadata_file_roundtrip_and_compression_sniff() {
        let l = Layer::new().file("a", 0o644, b"hello").bytes();
        let d = tempfile::tempdir().unwrap();
        let gz = d.path().join("l.tar.gz");
        let zs = d.path().join("l.tar.zst");
        let raw = d.path().join("l.tar");
        let mut e =
            flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::fast());
        e.write_all(&l).unwrap();
        e.finish().unwrap();
        fs::write(&zs, zstd::encode_all(&l[..], 3).unwrap()).unwrap();
        fs::write(&raw, &l).unwrap();
        for p in [gz, zs, raw] {
            let mut v = Vec::new();
            open_layer(&p).unwrap().read_to_end(&mut v).unwrap();
            assert_eq!(v, l);
        }
        let (_d2, _r, meta, _) = unpack(vec![l], LinkKind::Hardlink);
        let mp = d.path().join("metadata.jsonl.zst");
        write_metadata(&mp, &meta).unwrap();
        let rows = read_metadata(&mp).unwrap();
        assert_eq!(rows[0]["p"], "a");
        assert_eq!(rows[0]["m"], 0o644);
    }
}
