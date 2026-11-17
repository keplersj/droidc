//! On-disk layout under `--root`: paths, lock, PENDING marker, atomic symlink swaps.

use crate::error::{Context, Error, ErrorKind, Result};
use crate::record::{now_rfc3339, Deployment};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Root {
    pub path: PathBuf,
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
    pub fn new(path: impl Into<PathBuf>) -> Root {
        Root { path: path.into() }
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
        self.path.join("deployments")
    }
    pub fn deployment(&self, id: &str) -> PathBuf {
        self.deployments().join(id)
    }
    pub fn staging(&self) -> PathBuf {
        self.path.join("staging")
    }
    pub fn pending_path(&self) -> PathBuf {
        self.staging().join("PENDING")
    }
    pub fn var(&self) -> PathBuf {
        self.path.join("var")
    }
    pub fn home(&self) -> PathBuf {
        self.path.join("home")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.path.join("lock")
    }
    pub fn current_link(&self) -> PathBuf {
        self.path.join("current")
    }
    pub fn previous_link(&self) -> PathBuf {
        self.path.join("previous")
    }

    /// Create the directory skeleton (idempotent).
    pub fn init(&self) -> Result<()> {
        for d in [
            self.blobs(),
            self.blob_tmp(),
            self.deployments(),
            self.staging(),
            self.var(),
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

    /// Atomically point `<root>/<name>` at the relative target `deployments/<id>`.
    pub fn set_link(&self, name: &str, id: &str) -> Result<()> {
        let tmp = self.path.join(format!(".{name}.tmp"));
        let _ = fs::remove_file(&tmp);
        symlink(format!("deployments/{id}"), &tmp).context("create symlink")?;
        fs::rename(&tmp, self.path.join(name)).context("swap symlink")?;
        fsync_dir(&self.path);
        Ok(())
    }

    pub fn remove_link(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.path.join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn fsync_dir(p: &Path) {
    if let Ok(f) = File::open(p) {
        let _ = f.sync_all();
    }
}

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
        match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(RootLock { _file: f }),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(Error::new(
                ErrorKind::Locked,
                "another droidc command holds the root lock",
            )),
            Err(e) => Err(Error::internal(format!("flock: {e}"))),
        }
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
