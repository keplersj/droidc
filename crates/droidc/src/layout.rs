//! On-disk layout under `--root`: paths, lock, PENDING marker, atomic symlink swaps.
//!
//! One root holds any number of named containers. Shared by all of them: `blobs/` (so an image is
//! downloaded once), `policy.json` and `lock`. Everything else is per container: `deployments/`,
//! `current`, `previous`, `staging/`, `var/`, `home/`, `cgroup/` and `session.json`. The `default`
//! container keeps those at the top of the root, exactly where they were before containers
//! existed; any other container keeps them under `containers/<name>/`.

use crate::error::{Context, Error, ErrorKind, Result};
use crate::record::{now_rfc3339, Deployment};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// Name of the container that lives at the top of the root.
pub const DEFAULT_CONTAINER: &str = "default";

#[derive(Debug, Clone)]
pub struct Root {
    /// The droidc root (shared state).
    pub path: PathBuf,
    /// This container's state directory: `path` for `default`, else `path/containers/<name>`.
    pub cdir: PathBuf,
    pub container: String,
}

/// Contents of `staging/PENDING`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pending {
    /// `switch` or `rollback`
    pub op: String,
    /// The deployment that becomes `current`.
    pub id: String,
    /// The deployment that becomes `previous` (the old `current`), if any.
    #[serde(default)]
    pub prev: Option<String>,
    pub started: String,
}

impl Root {
    /// The `default` container of the root at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Root {
        let path = path.into();
        Root {
            cdir: path.clone(),
            path,
            container: DEFAULT_CONTAINER.into(),
        }
    }

    /// Container `name` of the root at `path`. The name must pass [`validate_container_name`].
    pub fn with_container(path: impl Into<PathBuf>, name: &str) -> Result<Root> {
        validate_container_name(name)?;
        let path = path.into();
        let cdir = if name == DEFAULT_CONTAINER {
            path.clone()
        } else {
            containers_dir(&path).join(name)
        };
        Ok(Root {
            path,
            cdir,
            container: name.into(),
        })
    }

    /// The same container under another root path (e.g. the canonical one).
    pub fn rebase(&self, path: impl Into<PathBuf>) -> Root {
        Root::with_container(path, &self.container).expect("name was validated")
    }

    pub fn is_default(&self) -> bool {
        self.container == DEFAULT_CONTAINER
    }

    /// Every container that exists under this root, `default` first. `default` is listed only
    /// when it has state (a `current` link or a `deployments/` entry).
    pub fn all_containers(&self) -> Vec<Root> {
        let mut out = Vec::new();
        let d = Root::new(&self.path);
        if d.current_link().symlink_metadata().is_ok()
            || fs::read_dir(d.deployments()).is_ok_and(|mut r| r.next().is_some())
        {
            out.push(d);
        }
        let mut names: Vec<String> = fs::read_dir(containers_dir(&self.path))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != DEFAULT_CONTAINER && validate_container_name(n).is_ok())
            .collect();
        names.sort();
        out.extend(
            names
                .iter()
                .map(|n| Root::with_container(&self.path, n).expect("validated above")),
        );
        out
    }
    pub fn policy(&self) -> PathBuf {
        self.path.join("policy.json")
    }
    pub fn blobs(&self) -> PathBuf {
        self.path.join("blobs/sha256")
    }
    pub fn blob_tmp(&self) -> PathBuf {
        self.path.join("blobs/tmp")
    }
    pub fn blob(&self, digest: &str) -> PathBuf {
        self.blobs()
            .join(digest.strip_prefix("sha256:").unwrap_or(digest))
    }
    pub fn deployments(&self) -> PathBuf {
        self.cdir.join("deployments")
    }
    pub fn deployment(&self, id: &str) -> PathBuf {
        self.deployments().join(id)
    }
    pub fn staging(&self) -> PathBuf {
        self.cdir.join("staging")
    }
    pub fn pending_path(&self) -> PathBuf {
        self.staging().join("PENDING")
    }
    pub fn var(&self) -> PathBuf {
        self.cdir.join("var")
    }
    /// Stands in for `/sys/fs/cgroup` when the guest runs `supervise --systemd` (see `crates/cgshim`).
    pub fn cgroup(&self) -> PathBuf {
        self.cdir.join("cgroup")
    }
    pub fn home(&self) -> PathBuf {
        self.cdir.join("home")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.path.join("lock")
    }
    pub fn current_link(&self) -> PathBuf {
        self.cdir.join("current")
    }
    pub fn previous_link(&self) -> PathBuf {
        self.cdir.join("previous")
    }

    /// Create the directory skeleton (idempotent).
    pub fn init(&self) -> Result<()> {
        for d in [
            self.blobs(),
            self.blob_tmp(),
            self.deployments(),
            self.staging(),
            self.var(),
            self.cgroup(),
            self.home().join("root"),
        ] {
            fs::create_dir_all(&d).context(d.display())?;
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(self.home().join("root"), fs::Permissions::from_mode(0o700));
        Ok(())
    }

    /// Deployment id a link (`current`/`previous`) points at. Works from inside the guest too: the
    /// link is relative (`deployments/<id>`), so only its last component is used.
    pub fn link_id(&self, link: &Path) -> Option<String> {
        let t = fs::read_link(link).ok()?;
        t.file_name().map(|s| s.to_string_lossy().into_owned())
    }
    pub fn current_id(&self) -> Option<String> {
        self.link_id(&self.current_link())
    }
    pub fn previous_id(&self) -> Option<String> {
        self.link_id(&self.previous_link())
    }

    pub fn read_record(&self, id: &str) -> Result<Deployment> {
        let p = self.deployment(id).join("deployment.json");
        let b = fs::read(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::not_found(format!("deployment {id} has no deployment.json"))
            } else {
                Error::from(e)
            }
        })?;
        Ok(serde_json::from_slice(&b)?)
    }

    pub fn read_pending(&self) -> Result<Option<Pending>> {
        match fs::read(self.pending_path()) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(p) => Ok(Some(p)),
                // A torn PENDING write: treat as "an operation was interrupted before commit".
                Err(_) => Ok(Some(Pending {
                    op: "unknown".into(),
                    id: String::new(),
                    prev: None,
                    started: now_rfc3339(),
                })),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_pending(&self, p: &Pending) -> Result<()> {
        fs::create_dir_all(self.staging())?;
        let tmp = self.staging().join("PENDING.tmp");
        fs::write(&tmp, serde_json::to_vec(p)?).context("write PENDING")?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, self.pending_path())?;
        fsync_dir(&self.staging());
        Ok(())
    }

    pub fn clear_pending(&self) -> Result<()> {
        match fs::remove_file(self.pending_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        fsync_dir(&self.staging());
        Ok(())
    }

    /// Atomically point `<cdir>/<name>` at the relative target `deployments/<id>`.
    pub fn set_link(&self, name: &str, id: &str) -> Result<()> {
        fs::create_dir_all(&self.cdir).context(self.cdir.display())?;
        let tmp = self.cdir.join(format!(".{name}.tmp"));
        let _ = fs::remove_file(&tmp);
        symlink(format!("deployments/{id}"), &tmp).context("create symlink")?;
        fs::rename(&tmp, self.cdir.join(name)).context("swap symlink")?;
        fsync_dir(&self.cdir);
        Ok(())
    }

    pub fn remove_link(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.cdir.join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// `<root>/containers`: where every container but `default` keeps its state.
pub fn containers_dir(root: &Path) -> PathBuf {
    root.join("containers")
}

/// Container names become directory names and appear in socket and unit names, so keep them
/// short and plain: 1-63 of `a-z`, `0-9`, `-`, `_`, `.`, starting with a letter or digit.
pub fn validate_container_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "invalid container name {name:?}: use 1-63 of a-z, 0-9, '-', '_', '.', starting with a letter or digit"
        )))
    }
}

pub fn fsync_dir(p: &Path) {
    if let Ok(f) = File::open(p) {
        let _ = f.sync_all();
    }
}

/// How often, and how far apart, [`RootLock::acquire`] tries a busy lock before giving up.
const LOCK_TRIES: u32 = 10;
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(20);

/// Exclusive advisory lock on `<root>/lock`, held until dropped.
pub struct RootLock {
    _file: File,
}

impl RootLock {
    pub fn acquire(root: &Root) -> Result<RootLock> {
        fs::create_dir_all(&root.path).context(root.path.display())?;
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.lock_path())
            .context("open lock file")?;
        // A lock that was just released can still be held for a moment by a child forked while it
        // was taken: the child shares the open file until it execs. So give a busy lock a short
        // grace before calling it held by another command.
        for attempt in 0..LOCK_TRIES {
            match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(RootLock { _file: f }),
                Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                    if attempt + 1 < LOCK_TRIES {
                        std::thread::sleep(LOCK_RETRY);
                    }
                }
                Err(e) => return Err(Error::internal(format!("flock: {e}"))),
            }
        }
        Err(Error::new(
            ErrorKind::Locked,
            "another droidc command holds the root lock",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let r = Root::new(d.path());
        let l1 = RootLock::acquire(&r).unwrap();
        let e = RootLock::acquire(&r).err().unwrap();
        assert_eq!(e.kind, ErrorKind::Locked);
        drop(l1);
        RootLock::acquire(&r).unwrap();
    }

    #[test]
    fn symlink_swap_is_relative() {
        let d = tempfile::tempdir().unwrap();
        let r = Root::new(d.path());
        r.init().unwrap();
        r.set_link("current", "aaaa").unwrap();
        r.set_link("current", "bbbb").unwrap();
        assert_eq!(
            fs::read_link(r.current_link()).unwrap(),
            PathBuf::from("deployments/bbbb")
        );
        assert_eq!(r.current_id().as_deref(), Some("bbbb"));
    }

    #[test]
    fn default_container_keeps_the_old_layout() {
        let r = Root::with_container("/r", "default").unwrap();
        assert_eq!(r.cdir, PathBuf::from("/r"));
        assert_eq!(r.current_link(), PathBuf::from("/r/current"));
        assert_eq!(r.var(), PathBuf::from("/r/var"));
        assert_eq!(r.blobs(), PathBuf::from("/r/blobs/sha256"));
    }

    #[test]
    fn named_containers_share_blobs_only() {
        let r = Root::with_container("/r", "dev").unwrap();
        assert_eq!(r.cdir, PathBuf::from("/r/containers/dev"));
        assert_eq!(r.current_link(), PathBuf::from("/r/containers/dev/current"));
        assert_eq!(
            r.deployments(),
            PathBuf::from("/r/containers/dev/deployments")
        );
        assert_eq!(r.var(), PathBuf::from("/r/containers/dev/var"));
        assert_eq!(r.home(), PathBuf::from("/r/containers/dev/home"));
        assert_eq!(r.staging(), PathBuf::from("/r/containers/dev/staging"));
        assert_eq!(r.blobs(), PathBuf::from("/r/blobs/sha256"));
        assert_eq!(r.policy(), PathBuf::from("/r/policy.json"));
        assert_eq!(r.lock_path(), PathBuf::from("/r/lock"));
    }

    #[test]
    fn container_names_are_checked() {
        for ok in ["default", "dev", "bluefin-dx", "a.b_c", "0"] {
            validate_container_name(ok).unwrap();
        }
        let long = "a".repeat(64);
        for bad in ["", "Dev", "-x", ".x", "a/b", "..", "a b", long.as_str()] {
            assert!(validate_container_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn all_containers_lists_default_then_named() {
        let d = tempfile::tempdir().unwrap();
        let def = Root::new(d.path());
        assert!(def.all_containers().is_empty());
        def.init().unwrap();
        def.set_link("current", "aaaa").unwrap();
        for n in ["zeta", "alpha"] {
            let c = Root::with_container(d.path(), n).unwrap();
            c.init().unwrap();
        }
        fs::create_dir_all(containers_dir(d.path()).join("Not Valid")).unwrap();
        let names: Vec<String> = def
            .all_containers()
            .into_iter()
            .map(|r| r.container)
            .collect();
        assert_eq!(names, ["default", "alpha", "zeta"]);
    }

    #[test]
    fn named_container_links_stay_relative() {
        let d = tempfile::tempdir().unwrap();
        let r = Root::with_container(d.path(), "dev").unwrap();
        r.init().unwrap();
        r.set_link("current", "aaaa").unwrap();
        assert_eq!(
            fs::read_link(r.current_link()).unwrap(),
            PathBuf::from("deployments/aaaa")
        );
        assert_eq!(r.current_id().as_deref(), Some("aaaa"));
        assert!(Root::new(d.path()).current_id().is_none());
    }

    #[test]
    fn pending_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let r = Root::new(d.path());
        r.init().unwrap();
        assert!(r.read_pending().unwrap().is_none());
        let p = Pending {
            op: "switch".into(),
            id: "x".into(),
            prev: Some("y".into()),
            started: "t".into(),
        };
        r.write_pending(&p).unwrap();
        assert_eq!(r.read_pending().unwrap().unwrap(), p);
        r.clear_pending().unwrap();
        assert!(r.read_pending().unwrap().is_none());
    }
}
