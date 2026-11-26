//! switch / upgrade / rollback / gc / crash recovery.

use crate::error::{Context, Error, Result};
use crate::etcmerge;
use crate::events::Emitter;
use crate::fsutil;
use crate::layout::{fsync_dir, Pending, Root, RootLock};
use crate::link::{make_strategy, LinkKind, PathMap, DEFAULT_L2S_PREFIX};
use crate::platform::Platform;
use crate::record::{deployment_id, now_rfc3339, Deployment, SignatureInfo, Stats};
use crate::registry::{Descriptor, HttpConfig, ImageRef, Manifest, Registry, Resolved};
use crate::unpack::{open_layer, write_metadata, Unpacker};
use crate::verify_glue::{self, Policy};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

pub const SESSION_LABEL: &str = "containers.droidc.session";

#[derive(Debug, Clone)]
pub struct SwitchOpts {
    pub reference: String,
    pub insecure: bool,
    pub platform: Option<Platform>,
    pub links: LinkKind,
    pub http: HttpConfig,
    pub l2s_prefix: String,
    /// Resolve and verify the signature only; change nothing.
    pub verify_only: bool,
    /// (root) Apply the image's uid/gid/mode/xattrs to the deployment it activates, as
    /// `fix-ownership` does.
    pub apply_ownership: bool,
}

impl SwitchOpts {
    pub fn new(reference: &str) -> Self {
        SwitchOpts {
            reference: reference.into(),
            insecure: false,
            platform: None,
            links: LinkKind::Auto,
            http: HttpConfig::default(),
            l2s_prefix: DEFAULT_L2S_PREFIX.into(),
            verify_only: false,
            apply_ownership: false,
        }
    }
}

/// The root lock, plus (when droidc runs as root) handing back what the command created: see
/// [`hand_back`]. Runs before the lock is released.
pub(crate) struct RootGuard {
    root: Root,
    _lock: RootLock,
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        if rustix::process::geteuid().is_root() {
            if let Err(e) = hand_back(&self.root) {
                eprintln!("warn: could not hand the root back to its owner: {e}");
            }
        }
    }
}

/// Give everything under the root that belongs to uid 0 to the owner of the root directory (the
/// app uid), so non-root commands and proot keep working after a command ran through su: the lock,
/// policy.json, blobs, staging, records, and deployments made by a root `switch` without
/// `--apply-ownership`. Left alone: the `rootfs` of image-owned deployments, and `var`/`home` while
/// any deployment is image-owned (they carry image and guest owners then). Nothing to do when the
/// root directory itself belongs to root. Never follows symlinks. Returns the entries changed.
pub(crate) fn hand_back(root: &Root) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::metadata(&root.path).context(root.path.display())?;
    if md.uid() == 0 {
        return Ok(0);
    }
    let owner = Owner {
        uid: rustix::process::Uid::from_raw(md.uid()),
        gid: rustix::process::Gid::from_raw(md.gid()),
        label: fsutil::selinux_label(&root.path, false),
    };
    let mut n = 0;
    // The top of the root: shared state plus the `default` container.
    let top = Root::new(&root.path);
    hand_back_dir(&top, &top.path, &owner, &image_owned_ids(&top), &mut n)?;
    // Named containers: their own deployments, var and home, with the same image-owned rules.
    let cdirs = crate::layout::containers_dir(&root.path);
    if cdirs.symlink_metadata().is_ok() {
        give_root_entries(&cdirs, &owner, &mut n, false)?;
        for c in root
            .all_containers()
            .into_iter()
            .filter(|c| !c.is_default())
        {
            let image = image_owned_ids(&c);
            give_root_entries(&c.cdir, &owner, &mut n, false)?;
            hand_back_dir(&c, &c.cdir, &owner, &image, &mut n)?;
        }
    }
    Ok(n)
}

fn image_owned_ids(root: &Root) -> HashSet<String> {
    fs::read_dir(root.deployments())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|id| {
            root.read_record(id)
                .is_ok_and(|r| r.ownership.as_deref() == Some(crate::record::OWNERSHIP_IMAGE))
        })
        .collect()
}

/// Hand back the entries of one state directory (the root itself for `default`).
fn hand_back_dir(
    root: &Root,
    dir: &Path,
    owner: &Owner,
    image: &HashSet<String>,
    n: &mut u64,
) -> Result<()> {
    for e in fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        match name.as_str() {
            "containers" if dir == root.path.as_path() => {}
            "var" | "home" if !image.is_empty() => {}
            "deployments" => {
                give_root_entries(&p, owner, n, false)?;
                for d in fs::read_dir(&p)?.flatten() {
                    let id = d.file_name().to_string_lossy().into_owned();
                    if image.contains(&id) {
                        give_root_entries(&d.path(), owner, n, false)?;
                        for f in fs::read_dir(d.path())?.flatten() {
                            if f.file_name() != "rootfs" {
                                give_root_entries(&f.path(), owner, n, true)?;
                            }
                        }
                    } else {
                        give_root_entries(&d.path(), owner, n, true)?;
                    }
                }
            }
            _ => give_root_entries(&p, owner, n, true)?,
        }
    }
    Ok(())
}

/// The root directory's owner and SELinux label: what everything droidc creates should carry.
struct Owner {
    uid: rustix::process::Uid,
    gid: rustix::process::Gid,
    label: Option<Vec<u8>>,
}

/// lchown `p` (and with `recurse`, everything below it) to `owner` where it belongs to uid 0, and
/// give it the root's SELinux label.
fn give_root_entries(p: &Path, owner: &Owner, n: &mut u64, recurse: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let md = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::from(e)),
    };
    if md.uid() == 0 {
        rustix::fs::chownat(
            rustix::fs::CWD,
            p,
            Some(owner.uid),
            Some(owner.gid),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|e| Error::internal(format!("chown {}: {e}", p.display())))?;
        fsutil::inherit_selinux_label(p, owner.label.as_deref());
        *n += 1;
    }
    if recurse && md.is_dir() {
        for e in fs::read_dir(p)?.flatten() {
            give_root_entries(&e.path(), owner, n, true)?;
        }
    }
    Ok(())
}

/// Create the root (if needed), canonicalize it and take the lock. The lock is root-wide, so
/// mutating commands on different containers of one root run one at a time.
pub(crate) fn open_root(target: &Root) -> Result<(Root, RootGuard)> {
    let path = &target.path;
    fs::create_dir_all(path).context(path.display())?;
    let canon = fs::canonicalize(path).context(path.display())?;
    let root = target.rebase(canon);
    let lock = RootGuard {
        root: root.clone(),
        _lock: RootLock::acquire(&root)?,
    };
    if !rustix::process::geteuid().is_root() {
        refuse_image_owned(&root)?;
    }
    root.init()?;
    if !root.policy().exists() {
        // First run: write the default trust policy.
        fs::write(root.policy(), Policy::default_policy().to_json())
            .context("write policy.json")?;
    }
    Ok((root, lock))
}

// ------------------------------------------------------------------ recovery

pub fn recover(root: &Root, em: &Emitter) -> Result<()> {
    let Some(p) = root.read_pending()? else {
        return Ok(());
    };
    em.warn(&format!(
        "recovering interrupted operation '{}' ({})",
        p.op, p.id
    ));
    let committed = matches!(p.op.as_str(), "switch" | "rollback")
        && !p.id.is_empty()
        && root.deployment(&p.id).join("deployment.json").is_file();
    if committed {
        // The deployment was complete and renamed into place: finish the symlink swaps.
        finish_switch(root, &p)?;
    } else {
        clear_staging(root)?;
        root.clear_pending()?;
    }
    Ok(())
}

fn clear_staging(root: &Root) -> Result<()> {
    if let Ok(rd) = fs::read_dir(root.staging()) {
        for e in rd.flatten() {
            if e.file_name() != "PENDING" {
                fsutil::rmtree(&e.path())?;
            }
        }
    }
    Ok(())
}

/// Move the image's seeded /var content into `<root>/var` if that is still empty. Idempotent.
fn seed_var(root: &Root, id: &str) -> Result<()> {
    let src = root.deployment(id).join("rootfs/var");
    let dst = root.var();
    fs::create_dir_all(&dst)?;
    if fsutil::is_empty_dir(&dst) && !fsutil::is_empty_dir(&src) {
        for e in fs::read_dir(&src)? {
            let e = e?;
            fs::rename(e.path(), dst.join(e.file_name())).context("seed var")?;
        }
    }
    Ok(())
}

/// `/usr` is read-only on a bootc host. Here only the write bits can say so: clearing them makes
/// `pip install`, `make install` and the like fail with EACCES instead of silently changing a
/// verified deployment (the true modes are in `metadata.jsonl.zst`). Not a security boundary: the
/// app uid owns the tree. `rmtree` puts the bits back when gc deletes it. Done on every switch, so a
/// deployment unpacked before this is sealed the next time it becomes current; image-owned trees
/// (fix-ownership, root-owned) are skipped by the kernel and need nothing. `/usr/etc` is left as
/// unpack made it: the next switch's /etc merge compares its modes with the live /etc.
fn seal_usr(root: &Root, id: &str) {
    let usr = root.deployment(id).join("rootfs/usr");
    let (_, failed) = fsutil::clear_write_bits(&usr, &[&usr.join("etc")]);
    if failed > 0 && !rustix::process::getuid().is_root() {
        eprintln!("warn: {failed} entries under /usr of {id} stay writable");
    }
}

/// Commit: seed var, `previous` := old current, `current` := new, drop PENDING.
fn finish_switch(root: &Root, p: &Pending) -> Result<()> {
    seed_var(root, &p.id)?;
    seal_usr(root, &p.id);
    if let Some(prev) = &p.prev {
        if prev != &p.id && root.deployment(prev).join("deployment.json").is_file() {
            root.set_link("previous", prev)?;
        }
    }
    root.set_link("current", &p.id)?;
    clear_staging(root)?;
    root.clear_pending()?;
    Ok(())
}

// ------------------------------------------------------------------ install (unpack + commit)

pub struct InstallInput<'a> {
    pub record: Deployment,
    /// Layer files (gzip, zstd or plain tar), bottom first.
    pub layers: &'a [PathBuf],
    pub links: LinkKind,
    pub l2s_prefix: &'a str,
}

/// Unpack layers into staging, merge /etc, write the record, rename into `deployments/<id>`, then
/// switch `current`. Caller holds the lock and has run recovery.
pub fn install(root: &Root, em: &Emitter, input: InstallInput<'_>) -> Result<Deployment> {
    let mut rec = input.record;
    let id = rec.id.clone();
    let prev = root.current_id();
    root.write_pending(&Pending {
        op: "switch".into(),
        id: id.clone(),
        prev: prev.clone(),
        started: now_rfc3339(),
    })?;

    let stage = root.staging().join(&id);
    fsutil::rmtree(&stage)?;
    let rootfs = stage.join("rootfs");
    fs::create_dir_all(&rootfs).context(rootfs.display())?;

    let seed = fs::read_dir(root.var())
        .map(|mut r| r.next().is_none())
        .unwrap_or(true);
    let final_rootfs = root.deployment(&id).join("rootfs");
    let map = PathMap {
        phys_root: rootfs.clone(),
        final_root: final_rootfs,
        var: if seed {
            Some((rootfs.join("var"), root.var()))
        } else {
            None
        },
    };
    let started = Instant::now();
    let mut u = Unpacker::new(
        rootfs.clone(),
        map,
        make_strategy(input.links, input.l2s_prefix),
        !seed,
        em,
    );
    let n = input.layers.len() as u64;
    for (i, l) in input.layers.iter().enumerate() {
        em.progress(
            "unpack",
            i as u64,
            n,
            "layers",
            &format!("layer {}/{}", i + 1, n),
        );
        let r = open_layer(l)?;
        u.apply_layer(r).context(format!("layer {}", i + 1))?;
    }
    em.progress("unpack", n, n, "layers", &format!("layer {n}/{n}"));
    let (meta, ustats, used) = u.finish()?;

    // /etc 3-way merge
    em.progress("etc-merge", 0, 1, "files", "merging /etc");
    let cur_rootfs = root.current_link().join("rootfs");
    let have_cur = prev.is_some() && cur_rootfs.join("etc").is_dir();
    let (op, ol) = if have_cur {
        (
            Some(cur_rootfs.join("usr/etc")),
            Some(cur_rootfs.join("etc")),
        )
    } else {
        (None, None)
    };
    let ms = etcmerge::merge(
        op.as_deref(),
        ol.as_deref(),
        &rootfs.join("usr/etc"),
        &rootfs.join("etc"),
    )?;
    em.progress(
        "etc-merge",
        1,
        1,
        "files",
        &format!(
            "/etc: {} taken from image, {} local changes kept, {} local deletions kept",
            ms.taken_new, ms.kept_local, ms.kept_deleted
        ),
    );
    fs::create_dir_all(rootfs.join("var"))?;

    write_metadata(&stage.join("metadata.jsonl.zst"), &meta)?;
    rec.stats = Stats {
        download_bytes: rec.stats.download_bytes,
        files: ustats.files,
        bytes_on_disk: fsutil::disk_usage(&stage),
        hardlinks: ustats.hardlinks,
        link_strategy: input.links.as_str().into(),
        link_strategy_used: used,
        unpack_seconds: (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
    };
    write_record(&stage.join("deployment.json"), &rec)?;

    em.progress("deploy", 0, 1, "layers", "committing");
    let dest = root.deployment(&id);
    fsutil::rmtree(&dest)?; // an incomplete leftover (no deployment.json) or stale copy
    fsync_dir(&stage);
    fs::rename(&stage, &dest).context("rename staging into deployments")?;
    fsync_dir(&root.deployments());
    let p = root.read_pending()?.expect("pending written above");
    finish_switch(root, &p)?;
    em.progress("deploy", 1, 1, "layers", "done");
    Ok(rec)
}

/// Without root, nothing may touch a deployment whose image ownership was applied: etcmerge cannot
/// read a `0000` /etc/shadow and gc cannot delete root-owned trees (it would stop half way).
fn refuse_image_owned(root: &Root) -> Result<()> {
    for e in fs::read_dir(root.deployments())
        .into_iter()
        .flatten()
        .flatten()
    {
        let id = e.file_name().to_string_lossy().into_owned();
        if let Ok(r) = root.read_record(&id) {
            if r.ownership.as_deref() == Some(crate::record::OWNERSHIP_IMAGE) {
                return Err(Error::invalid(format!(
                    "deployment {id} has image ownership: run droidc as root, or `fix-ownership --undo` as root first"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn write_record(path: &Path, rec: &Deployment) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(rec)?).context(tmp.display())?;
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

// ------------------------------------------------------------------ policy + signatures

pub fn load_policy(root: &Root) -> Result<Policy> {
    match fs::read_to_string(root.policy()) {
        Ok(s) => Ok(Policy::from_json(&s)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let p = Policy::default_policy();
            fs::write(root.policy(), p.to_json()).context("write policy.json")?;
            Ok(p)
        }
        Err(e) => Err(e).context("read policy.json"),
    }
}

/// Look for cosign signatures on the platform manifest digest, then the index digest.
pub fn verify_image(
    reg: &Registry,
    policy: &Policy,
    res: &Resolved,
    em: &Emitter,
) -> Result<SignatureInfo> {
    let mut candidates = vec![res.manifest_digest.clone()];
    if let Some(i) = &res.index_digest {
        candidates.push(i.clone());
    }
    let mut last: Option<Error> = None;
    for d in candidates {
        let sigs = reg.fetch_signatures(&res.image, &d)?;
        if sigs.is_empty() {
            em.info(&format!("no cosign signature for {d}"));
            continue;
        }
        match verify_glue::verify(policy, &res.image.full_name(), &d, &sigs, SystemTime::now()) {
            Ok(v) => {
                return Ok(SignatureInfo {
                    verified: true,
                    mode: v.mode,
                    subject: v.subject,
                    issuer: v.issuer,
                    policy_match: Some(v.policy_match),
                });
            }
            Err(e) => last = Some(e.into()),
        }
    }
    Err(last.unwrap_or_else(|| {
        Error::signature(format!(
            "no cosign signature found for {} (use --insecure-allow-unsigned to override)",
            res.image.original
        ))
    }))
}

// ------------------------------------------------------------------ switch / upgrade

fn store_bytes(root: &Root, digest: &str, bytes: &[u8]) -> Result<()> {
    let p = root.blob(digest);
    if p.is_file() {
        return Ok(());
    }
    fs::create_dir_all(root.blobs())?;
    let tmp = root
        .blob_tmp()
        .join(format!("{}.meta", digest.trim_start_matches("sha256:")));
    fs::create_dir_all(root.blob_tmp())?;
    fs::write(&tmp, bytes).context(tmp.display())?;
    fs::rename(&tmp, &p).context(p.display())?;
    Ok(())
}

fn check_space(root: &Root, layers: &[Descriptor]) -> Result<()> {
    let total: u64 = layers.iter().map(|l| l.size).sum();
    let uncached: u64 = layers
        .iter()
        .filter(|l| !root.blob(&l.digest).is_file())
        .map(|l| l.size)
        .sum();
    let need = total / 2 * 5 + uncached; // 2.5x compressed, plus the download if not cached
    let free = fsutil::free_space(&root.path)?;
    if free < need {
        return Err(Error::storage(format!(
            "not enough free space under {}: need about {} MiB, have {} MiB",
            root.path.display(),
            need >> 20,
            free >> 20
        )));
    }
    Ok(())
}

/// Download all layers with a few parallel workers. Returns bytes transferred over the network.
fn download_layers(root: &Root, em: &Emitter, reg: &Registry, res: &Resolved) -> Result<u64> {
    let layers = &res.manifest.layers;
    let total: u64 = layers.iter().map(|l| l.size).sum();
    let done = AtomicU64::new(0);
    let layers_done = AtomicUsize::new(0);
    let net = AtomicU64::new(0);
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let err: Mutex<Option<Error>> = Mutex::new(None);
    let nl = layers.len();
    // Cached layers count as already done.
    let workers = 3.min(nl.max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= nl {
                    return;
                }
                let d = &layers[i];
                let on = |n: u64| {
                    let cur = done.fetch_add(n, Ordering::Relaxed) + n;
                    em.progress_throttled(
                        "download",
                        cur.min(total),
                        total,
                        "bytes",
                        &format!("layer {}/{}", layers_done.load(Ordering::Relaxed) + 1, nl),
                    );
                };
                match reg.fetch_blob(&res.image, d, &root.blob(&d.digest), &root.blob_tmp(), &on) {
                    Ok(got) => {
                        net.fetch_add(got, Ordering::Relaxed);
                        if got == 0 {
                            done.fetch_add(d.size, Ordering::Relaxed);
                        }
                        layers_done.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        failed.store(true, Ordering::Relaxed);
                        err.lock().unwrap().get_or_insert(e);
                        return;
                    }
                }
            });
        }
    });
    if let Some(e) = err.into_inner().unwrap() {
        return Err(e);
    }
    em.progress(
        "download",
        total,
        total,
        "bytes",
        &format!("layer {nl}/{nl}"),
    );
    Ok(net.load(Ordering::Relaxed))
}

fn build_record(
    res: &Resolved,
    id: &str,
    sig: SignatureInfo,
    insecure: bool,
    image_ref: &str,
) -> Deployment {
    let labels = res.config.config.labels.clone().unwrap_or_default();
    Deployment {
        id: id.into(),
        image_ref: image_ref.into(),
        manifest_digest: res.manifest_digest.clone(),
        index_digest: res.index_digest.clone(),
        config_digest: res.manifest.config.digest.clone(),
        platform: res.platform.to_string(),
        created: res.config.created.clone(),
        deployed_at: now_rfc3339(),
        session_label: labels.get(SESSION_LABEL).cloned(),
        labels,
        signature: sig,
        insecure,
        stats: Stats::default(),
        ownership: None,
    }
}

pub fn switch(target: &Root, em: &Emitter, opts: &SwitchOpts) -> Result<Value> {
    let (root, _lock) = open_root(target)?;
    recover(&root, em)?;
    switch_locked(&root, em, opts)
}

pub fn switch_locked(root: &Root, em: &Emitter, opts: &SwitchOpts) -> Result<Value> {
    if opts.apply_ownership {
        // Before any download.
        crate::ownership::require_root("--apply-ownership")?;
    } else if rustix::process::geteuid().is_root() && has_image_owned(root) {
        em.warn("other deployments have image ownership; pass --apply-ownership to keep this one consistent");
    }
    let reg = Registry::new(&opts.http)?;
    let plat = match &opts.platform {
        Some(p) => p.clone(),
        None => Platform::detect()?,
    };
    let image = ImageRef::parse(&opts.reference)?;
    em.progress(
        "resolve",
        0,
        1,
        "layers",
        &format!("resolving {}", opts.reference),
    );
    let res = reg.resolve(&image, &plat)?;
    let id = deployment_id(&res.manifest_digest);
    em.progress(
        "resolve",
        1,
        1,
        "layers",
        &format!(
            "{} -> {} ({} layers)",
            opts.reference,
            res.manifest_digest,
            res.manifest.layers.len()
        ),
    );

    let sig = if opts.insecure {
        em.warn("signature verification skipped (--insecure-allow-unsigned)");
        SignatureInfo::none()
    } else {
        em.progress("verify", 0, 1, "layers", "checking cosign signature");
        let policy = load_policy(root)?;
        let s = verify_image(&reg, &policy, &res, em)?;
        em.progress("verify", 1, 1, "layers", &format!("verified ({})", s.mode));
        s
    };
    if opts.verify_only {
        return Ok(
            json!({"changed": false, "verify_only": true, "id": id, "manifest_digest": res.manifest_digest,
                         "index_digest": res.index_digest, "platform": res.platform.to_string(), "signature": sig}),
        );
    }

    if root.current_id().as_deref() == Some(id.as_str()) {
        // Idempotent: `switch --apply-ownership` on the current image still applies it once.
        let own = apply_ownership_if_needed(root, em, opts, &id)?;
        let rec = root.read_record(&id).ok();
        return Ok(
            json!({"changed": false, "id": id, "manifest_digest": res.manifest_digest, "deployment": rec, "ownership": own}),
        );
    }

    store_bytes(root, &res.manifest_digest, &res.manifest_bytes)?;
    store_bytes(root, &res.manifest.config.digest, &res.config_bytes)?;
    if let (Some(d), Some(b)) = (&res.index_digest, &res.index_bytes) {
        store_bytes(root, d, b)?;
    }

    let prev = root.current_id();
    let rec = if root.deployment(&id).join("deployment.json").is_file() {
        // Already deployed (it is `previous`): just activate it.
        em.info(&format!("deployment {id} already present, activating"));
        root.write_pending(&Pending {
            op: "switch".into(),
            id: id.clone(),
            prev: prev.clone(),
            started: now_rfc3339(),
        })?;
        let p = root.read_pending()?.unwrap();
        finish_switch(root, &p)?;
        let mut rec = root.read_record(&id)?;
        if rec.image_ref != opts.reference {
            rec.image_ref = opts.reference.clone();
            write_record(&root.deployment(&id).join("deployment.json"), &rec)?;
        }
        rec
    } else {
        check_space(root, &res.manifest.layers)?;
        let dl = download_layers(root, em, &reg, &res)?;
        let mut rec = build_record(&res, &id, sig, opts.insecure, &opts.reference);
        rec.stats.download_bytes = dl;
        let layer_paths: Vec<PathBuf> = res
            .manifest
            .layers
            .iter()
            .map(|l| root.blob(&l.digest))
            .collect();
        install(
            root,
            em,
            InstallInput {
                record: rec,
                layers: &layer_paths,
                links: opts.links,
                l2s_prefix: &opts.l2s_prefix,
            },
        )?
    };
    // After activation: `<root>/var` is seeded by then, and its rows need it.
    let own = apply_ownership_if_needed(root, em, opts, &id)?;
    let rec = if own.is_null() {
        rec
    } else {
        root.read_record(&id)?
    };
    let g = gc_locked(root, em)?;
    Ok(
        json!({"changed": true, "id": id, "previous": prev, "deployment": rec, "gc": g, "ownership": own}),
    )
}

fn has_image_owned(root: &Root) -> bool {
    fs::read_dir(root.deployments())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| root.read_record(&e.file_name().to_string_lossy()).ok())
        .any(|r| r.ownership.as_deref() == Some(crate::record::OWNERSHIP_IMAGE))
}

/// The `fix-ownership` report, or null when not requested or already applied.
fn apply_ownership_if_needed(
    root: &Root,
    em: &Emitter,
    opts: &SwitchOpts,
    id: &str,
) -> Result<Value> {
    if !opts.apply_ownership
        || root.read_record(id)?.ownership.as_deref() == Some(crate::record::OWNERSHIP_IMAGE)
    {
        return Ok(Value::Null);
    }
    em.progress("ownership", 0, 1, "files", "applying image ownership");
    let r = crate::ownership::apply_image(root, id, &opts.l2s_prefix)?;
    em.progress(
        "ownership",
        1,
        1,
        "files",
        &format!("applied to {} paths", r["applied"]),
    );
    Ok(r)
}

pub fn upgrade(target: &Root, em: &Emitter, http: HttpConfig, l2s_prefix: &str) -> Result<Value> {
    let (root, _lock) = open_root(target)?;
    recover(&root, em)?;
    let cur = root
        .current_id()
        .ok_or_else(|| Error::not_found("no current deployment to upgrade"))?;
    let rec = root.read_record(&cur)?;
    let opts = SwitchOpts {
        reference: rec.image_ref.clone(),
        insecure: rec.insecure,
        platform: Some(Platform::parse(&rec.platform)?),
        links: LinkKind::parse(&rec.stats.link_strategy).unwrap_or(LinkKind::Auto),
        http,
        l2s_prefix: l2s_prefix.into(),
        verify_only: false,
        // Keep the root's mode: an image-owned current deployment upgrades to an image-owned one.
        apply_ownership: rec.ownership.as_deref() == Some(crate::record::OWNERSHIP_IMAGE),
    };
    switch_locked(&root, em, &opts)
}

// ------------------------------------------------------------------ rollback

pub fn rollback(target: &Root, em: &Emitter) -> Result<Value> {
    let (root, _lock) = open_root(target)?;
    recover(&root, em)?;
    let cur = root
        .current_id()
        .ok_or_else(|| Error::not_found("no current deployment"))?;
    let prev = root
        .previous_id()
        .ok_or_else(|| Error::not_found("no previous deployment to roll back to"))?;
    if !root.deployment(&prev).join("deployment.json").is_file() {
        return Err(Error::not_found(format!(
            "previous deployment {prev} is missing"
        )));
    }
    let p = Pending {
        op: "rollback".into(),
        id: prev.clone(),
        prev: Some(cur.clone()),
        started: now_rfc3339(),
    };
    root.write_pending(&p)?;
    finish_switch(&root, &p)?;
    Ok(json!({"changed": true, "current": prev, "previous": cur}))
}

// ------------------------------------------------------------------ containers

/// Every container under the root: name, current and previous deployment (id and image).
pub fn list_containers(target: &Root) -> Value {
    let dep = |c: &Root, id: Option<String>| match id {
        Some(id) => {
            let image = c.read_record(&id).ok().map(|r| r.image_ref);
            json!({"id": id, "image_ref": image})
        }
        None => Value::Null,
    };
    let list: Vec<Value> = target
        .all_containers()
        .iter()
        .map(|c| {
            json!({
                "name": c.container,
                "current": dep(c, c.current_id()),
                "previous": dep(c, c.previous_id()),
            })
        })
        .collect();
    json!({"containers": list})
}

/// Delete a named container's state, then GC the images no other container uses. `default` is
/// the root itself and can't be removed this way.
pub fn remove_container(target: &Root, em: &Emitter) -> Result<Value> {
    if target.is_default() {
        return Err(Error::invalid(
            "the default container is the root itself; remove the root directory instead",
        ));
    }
    let (root, _lock) = open_root_existing(target)?;
    if root.cdir.symlink_metadata().is_err() {
        return Err(Error::not_found(format!(
            "no container named {}",
            root.container
        )));
    }
    let freed = fsutil::disk_usage(&root.cdir);
    fsutil::rmtree(&root.cdir)?;
    fsync_dir(&crate::layout::containers_dir(&root.path));
    let g = gc_locked(&root, em)?;
    Ok(json!({"removed": root.container, "bytes_freed": freed, "gc": g}))
}

/// Like [`open_root`], but for a container that is about to go: nothing is created for it.
fn open_root_existing(target: &Root) -> Result<(Root, RootGuard)> {
    let canon = fs::canonicalize(&target.path).context(target.path.display())?;
    let root = target.rebase(canon);
    let lock = RootGuard {
        root: root.clone(),
        _lock: RootLock::acquire(&root)?,
    };
    if !rustix::process::geteuid().is_root() {
        refuse_image_owned(&root)?;
    }
    Ok((root, lock))
}

// ------------------------------------------------------------------ gc

pub fn gc(target: &Root, em: &Emitter) -> Result<Value> {
    let (root, _lock) = open_root(target)?;
    recover(&root, em)?;
    gc_locked(&root, em)
}

fn referenced_blobs(root: &Root, ids: &[String]) -> Option<HashSet<String>> {
    let mut set = HashSet::new();
    for id in ids {
        let rec = root.read_record(id).ok()?;
        let h = |d: &str| d.strip_prefix("sha256:").unwrap_or(d).to_string();
        set.insert(h(&rec.manifest_digest));
        set.insert(h(&rec.config_digest));
        if let Some(i) = &rec.index_digest {
            set.insert(h(i));
        }
        let mb = fs::read(root.blob(&rec.manifest_digest)).ok()?;
        let m: Manifest = serde_json::from_slice(&mb).ok()?;
        for l in m.layers {
            set.insert(h(&l.digest));
        }
    }
    Some(set)
}

pub fn gc_locked(root: &Root, em: &Emitter) -> Result<Value> {
    em.progress("gc", 0, 1, "files", "collecting garbage");
    let mut keep: Vec<String> = Vec::new();
    keep.extend(root.current_id());
    keep.extend(root.previous_id());
    let mut removed = Vec::new();
    let mut freed = 0u64;
    if let Ok(rd) = fs::read_dir(root.deployments()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) {
                freed += fsutil::disk_usage(&e.path());
                fsutil::rmtree(&e.path())?;
                removed.push(name);
            }
        }
    }
    if root.read_pending()?.is_none() {
        clear_staging(root)?;
    }
    let mut blobs_removed = 0u64;
    // Blobs are shared: keep what any container's current or previous deployment needs. This
    // container is added even if it has no state left to be listed by.
    let mut containers = root.all_containers();
    if !containers.iter().any(|c| c.cdir == root.cdir) {
        containers.push(root.clone());
    }
    let refs = containers.iter().try_fold(HashSet::new(), |mut acc, c| {
        let ids: Vec<String> = c.current_id().into_iter().chain(c.previous_id()).collect();
        acc.extend(referenced_blobs(c, &ids)?);
        Some(acc)
    });
    match refs {
        Some(refs) => {
            if let Ok(rd) = fs::read_dir(root.blobs()) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !refs.contains(&name) {
                        freed += e.metadata().map(|m| m.len()).unwrap_or(0);
                        fs::remove_file(e.path())?;
                        blobs_removed += 1;
                    }
                }
            }
            if let Ok(rd) = fs::read_dir(root.blob_tmp()) {
                for e in rd.flatten() {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        None => em.warn("could not read manifests of kept deployments; blob GC skipped"),
    }
    em.progress(
        "gc",
        1,
        1,
        "files",
        &format!(
            "removed {} deployments, {} blobs",
            removed.len(),
            blobs_removed
        ),
    );
    Ok(
        json!({"deployments_removed": removed, "blobs_removed": blobs_removed, "bytes_freed": freed}),
    )
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use crate::events::testutil::capture;
    use crate::status::Launcher;
    use crate::unpack::tests::Layer;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    fn rec(id: &str) -> Deployment {
        Deployment {
            id: id.into(),
            image_ref: format!("test/{id}:latest"),
            manifest_digest: format!("sha256:{id}{}", "0".repeat(64 - id.len())),
            index_digest: None,
            config_digest: format!("sha256:{}", "c".repeat(64)),
            platform: "linux/amd64".into(),
            created: None,
            deployed_at: now_rfc3339(),
            session_label: Some("gnome".into()),
            labels: Default::default(),
            signature: SignatureInfo::none(),
            insecure: true,
            stats: Stats::default(),
            ownership: None,
        }
    }

    fn write_layers(dir: &Path, name: &str, layers: Vec<Vec<u8>>) -> Vec<PathBuf> {
        layers
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let p = dir.join(format!("{name}-{i}.tar.gz"));
                let mut e = flate2::write::GzEncoder::new(
                    fs::File::create(&p).unwrap(),
                    flate2::Compression::fast(),
                );
                e.write_all(&l).unwrap();
                e.finish().unwrap();
                p
            })
            .collect()
    }

    fn image(os: &str, extra: &str) -> Vec<Vec<u8>> {
        vec![
            Layer::new()
                .dir("etc/", 0o755)
                .file("etc/os-release", 0o644, os.as_bytes())
                .file("etc/motd", 0o644, b"hello v1")
                .dir("usr/", 0o755)
                .dir("usr/bin/", 0o755)
                .file("usr/bin/tool", 0o755, b"tool")
                .hardlink("usr/bin/tool2", "usr/bin/tool")
                .dir("var/", 0o755)
                .dir("var/lib/", 0o755)
                .file("var/lib/seed", 0o644, b"seed")
                .bytes(),
            Layer::new()
                .file("etc/motd", 0o644, extra.as_bytes())
                .bytes(),
        ]
    }

    fn setup() -> (tempfile::TempDir, Root, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = Root::new(d.path().canonicalize().unwrap().join("root"));
        fs::create_dir_all(&root.path).unwrap();
        root.init().unwrap();
        let ld = d.path().join("layers");
        fs::create_dir_all(&ld).unwrap();
        (d, root, ld)
    }

    fn do_install(
        root: &Root,
        ld: &Path,
        id: &str,
        layers: Vec<Vec<u8>>,
        kind: LinkKind,
    ) -> Deployment {
        let (em, _) = capture();
        let paths = write_layers(ld, id, layers);
        install(
            root,
            &em,
            InstallInput {
                record: rec(id),
                layers: &paths,
                links: kind,
                l2s_prefix: ".l2s.",
            },
        )
        .unwrap()
    }

    #[test]
    fn install_switch_rollback_gc_cycle() {
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        let a = do_install(
            &root,
            &ld,
            "aaaa",
            image("A", "motd-A"),
            LinkKind::Link2Symlink,
        );
        assert_eq!(root.current_id().as_deref(), Some("aaaa"));
        assert!(root.previous_id().is_none());
        assert_eq!(a.stats.hardlinks, 1);
        assert_eq!(a.stats.link_strategy, "link2symlink");
        assert!(root.read_pending().unwrap().is_none());
        let rfs = root.path.join("current/rootfs");
        assert_eq!(fs::read(rfs.join("usr/etc/os-release")).unwrap(), b"A");
        assert_eq!(fs::read(rfs.join("etc/motd")).unwrap(), b"motd-A");
        assert_eq!(fs::read(rfs.join("usr/bin/tool2")).unwrap(), b"tool");
        // var seeded from the first image, moved out of the deployment
        assert_eq!(fs::read(root.var().join("lib/seed")).unwrap(), b"seed");
        assert!(!rfs.join("var/lib").exists());
        assert!(root.deployment("aaaa").join("metadata.jsonl.zst").is_file());
        // relative symlinks
        assert_eq!(
            fs::read_link(root.current_link()).unwrap(),
            PathBuf::from("deployments/aaaa")
        );
        // l2s chain points at the final (deployments/) location
        let t = fs::read_link(rfs.join("usr/bin/tool2")).unwrap();
        assert!(
            t.starts_with(root.deployment("aaaa").join("rootfs/usr/bin")),
            "{t:?}"
        );

        // user edits live /etc and var, then deploy B
        fs::write(rfs.join("etc/motd"), "my motd").unwrap();
        fs::write(root.var().join("lib/mine"), "m").unwrap();
        fs::write(root.var().join("lib/seed"), "changed").unwrap();
        let b = do_install(&root, &ld, "bbbb", image("B", "motd-B"), LinkKind::Hardlink);
        assert_eq!(root.current_id().as_deref(), Some("bbbb"));
        assert_eq!(root.previous_id().as_deref(), Some("aaaa"));
        let rfs_b = root.path.join("current/rootfs");
        assert_eq!(fs::read(rfs_b.join("etc/motd")).unwrap(), b"my motd"); // kept local edit
        assert_eq!(fs::read(rfs_b.join("etc/os-release")).unwrap(), b"B"); // untouched -> new
        assert_eq!(fs::read(rfs_b.join("usr/etc/motd")).unwrap(), b"motd-B");
        assert_eq!(fs::read(root.var().join("lib/seed")).unwrap(), b"changed"); // var not re-seeded
        assert!(!rfs_b.join("var/lib").exists());
        assert_eq!(fs::metadata(rfs_b.join("usr/bin/tool")).unwrap().nlink(), 2);
        assert_eq!(b.stats.link_strategy_used, "hardlink");

        // rollback swaps
        let r = rollback(&root, &em).unwrap();
        assert_eq!(r["current"], "aaaa");
        assert_eq!(root.current_id().as_deref(), Some("aaaa"));
        assert_eq!(root.previous_id().as_deref(), Some("bbbb"));
        rollback(&root, &em).unwrap();
        assert_eq!(root.current_id().as_deref(), Some("bbbb"));

        // a third deployment: A becomes unreferenced and is collected
        do_install(&root, &ld, "cccc", image("C", "motd-C"), LinkKind::Copy);
        assert_eq!(root.previous_id().as_deref(), Some("bbbb"));
        assert!(root.deployment("aaaa").exists());
        let g = gc(&root, &em).unwrap();
        assert_eq!(g["deployments_removed"], json!(["aaaa"]));
        assert!(!root.deployment("aaaa").exists());
        assert!(root.deployment("bbbb").exists() && root.deployment("cccc").exists());
    }

    #[test]
    fn gc_removes_unreferenced_blobs_only() {
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        // a manifest blob for the kept deployment referencing layer L1
        let l1 = "1".repeat(64);
        let man = json!({"config": {"digest": format!("sha256:{}", "c".repeat(64)), "size": 1},
                         "layers": [{"digest": format!("sha256:{l1}"), "size": 1}]});
        let mdig = rec("aaaa").manifest_digest;
        fs::write(root.blob(&mdig), serde_json::to_vec(&man).unwrap()).unwrap();
        fs::write(root.blob(&l1), b"layer").unwrap();
        fs::write(root.blobs().join("2".repeat(64)), b"orphan").unwrap();
        let g = gc(&root, &em).unwrap();
        assert_eq!(g["blobs_removed"], 1);
        assert!(root.blob(&l1).exists());
        assert!(!root.blobs().join("2".repeat(64)).exists());
    }

    /// A manifest blob for `id` whose only layer is `layer` (64 hex chars).
    fn write_manifest(root: &Root, id: &str, layer: &str) {
        let man = json!({"config": {"digest": format!("sha256:{}", "c".repeat(64)), "size": 1},
                         "layers": [{"digest": format!("sha256:{layer}"), "size": 1}]});
        fs::write(
            root.blob(&rec(id).manifest_digest),
            serde_json::to_vec(&man).unwrap(),
        )
        .unwrap();
        fs::write(root.blob(layer), b"layer").unwrap();
    }

    fn container(root: &Root, name: &str) -> Root {
        let c = Root::with_container(&root.path, name).unwrap();
        c.init().unwrap();
        c
    }

    #[test]
    fn named_containers_have_their_own_state() {
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        let dev = container(&root, "dev");
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        // Same image in a second container: its own deployment, /var seeded on its own.
        do_install(&dev, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        do_install(&dev, &ld, "bbbb", image("B", "y"), LinkKind::Copy);
        assert!(dev.deployment("aaaa").join("deployment.json").is_file());
        assert!(root.deployment("aaaa").join("deployment.json").is_file());
        assert_ne!(dev.deployment("aaaa"), root.deployment("aaaa"));
        assert_eq!(fs::read(dev.var().join("lib/seed")).unwrap(), b"seed");
        fs::write(root.var().join("lib/mine"), "m").unwrap();
        assert!(!dev.var().join("lib/mine").exists());
        assert_eq!(root.current_id().as_deref(), Some("aaaa"));
        assert!(root.previous_id().is_none());
        assert_eq!(dev.current_id().as_deref(), Some("bbbb"));
        assert_eq!(dev.previous_id().as_deref(), Some("aaaa"));
        // Rolling one container back leaves the other alone.
        rollback(&dev, &em).unwrap();
        assert_eq!(dev.current_id().as_deref(), Some("aaaa"));
        assert_eq!(root.current_id().as_deref(), Some("aaaa"));
        assert!(root.previous_id().is_none());
        let names: Vec<_> = list_containers(&root)["containers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["default", "dev"]);
    }

    #[test]
    fn gc_keeps_blobs_any_container_uses() {
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        let dev = container(&root, "dev");
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        do_install(&dev, &ld, "bbbb", image("B", "y"), LinkKind::Copy);
        let (l1, l2) = ("1".repeat(64), "2".repeat(64));
        write_manifest(&root, "aaaa", &l1);
        write_manifest(&root, "bbbb", &l2);
        fs::write(root.blobs().join("3".repeat(64)), b"orphan").unwrap();
        // GC from either container keeps both containers' layers.
        for c in [&dev, &root] {
            let g = gc(c, &em).unwrap();
            assert!(root.blob(&l1).exists() && root.blob(&l2).exists());
            assert!(g["deployments_removed"].as_array().unwrap().is_empty());
        }
        assert!(!root.blobs().join("3".repeat(64)).exists());
    }

    #[test]
    fn removing_a_container_frees_only_its_images() {
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        let dev = container(&root, "dev");
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        do_install(&dev, &ld, "bbbb", image("B", "y"), LinkKind::Copy);
        let (l1, l2) = ("1".repeat(64), "2".repeat(64));
        write_manifest(&root, "aaaa", &l1);
        write_manifest(&root, "bbbb", &l2);
        let r = remove_container(&dev, &em).unwrap();
        assert_eq!(r["removed"], "dev");
        assert!(!dev.cdir.exists());
        assert!(root.blob(&l1).exists());
        assert!(!root.blob(&l2).exists());
        assert_eq!(root.current_id().as_deref(), Some("aaaa"));
        assert_eq!(
            remove_container(&dev, &em).unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            remove_container(&root, &em).unwrap_err().kind,
            ErrorKind::Invalid
        );
    }

    #[test]
    fn shell_spec_for_a_named_container() {
        let (_d, root, ld) = setup();
        let dev = container(&root, "dev");
        do_install(&dev, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        let sp = serde_json::to_value(crate::status::shell_spec(&dev, Launcher::Proot).unwrap())
            .unwrap();
        let host = |v: &Value| v.as_str().unwrap().to_string();
        assert!(host(&sp["rootfs"]).ends_with("/root/containers/dev/current/rootfs"));
        assert_eq!(sp["binds"][0]["guest"], "/var");
        assert!(host(&sp["binds"][0]["host"]).ends_with("/root/containers/dev/var"));
        assert!(host(&sp["binds"][1]["host"]).ends_with("/root/containers/dev/home"));
        assert_eq!(sp["binds"][2]["guest"], "/run/droidc/root");
        assert!(host(&sp["binds"][2]["host"]).ends_with("/root"));
        assert!(host(&sp["systemd"]["binds"][0]["host"]).ends_with("/root/containers/dev/cgroup"));
        assert_eq!(sp["env"]["DROIDC_CONTAINER"], "dev");
        // The guest reads it back through the bound root and DROIDC_CONTAINER.
        let s = crate::status::status(&dev).unwrap();
        assert_eq!(s["container"], "dev");
        assert_eq!(s["current"]["id"], "aaaa");
        assert!(crate::status::shell_spec(&root, Launcher::Proot).is_err());
    }

    #[test]
    fn recovery_discards_uncommitted_staging() {
        let (_d, root, ld) = setup();
        let (em, buf) = capture();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        // interrupted mid-unpack: PENDING + half-built staging, nothing renamed
        root.write_pending(&Pending {
            op: "switch".into(),
            id: "bbbb".into(),
            prev: Some("aaaa".into()),
            started: now_rfc3339(),
        })
        .unwrap();
        fs::create_dir_all(root.staging().join("bbbb/rootfs/usr")).unwrap();
        fs::write(root.staging().join("bbbb/rootfs/usr/half"), b"x").unwrap();
        recover(&root, &em).unwrap();
        assert!(root.read_pending().unwrap().is_none());
        assert!(!root.staging().join("bbbb").exists());
        assert_eq!(root.current_id().as_deref(), Some("aaaa"));
        assert!(root.previous_id().is_none());
        assert!(buf.lines().iter().any(|l| l["level"] == "warn"));
    }

    #[test]
    fn recovery_rolls_forward_when_deployment_was_renamed_in() {
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        // simulate a crash after the staging->deployments rename but before any symlink swap:
        // build "bbbb" then rewind the links and re-create PENDING.
        do_install(&root, &ld, "bbbb", image("B", "x"), LinkKind::Copy);
        root.set_link("current", "aaaa").unwrap();
        fs::remove_file(root.previous_link()).unwrap();
        root.write_pending(&Pending {
            op: "switch".into(),
            id: "bbbb".into(),
            prev: Some("aaaa".into()),
            started: now_rfc3339(),
        })
        .unwrap();
        recover(&root, &em).unwrap();
        assert_eq!(root.current_id().as_deref(), Some("bbbb"));
        assert_eq!(root.previous_id().as_deref(), Some("aaaa"));
        assert!(root.read_pending().unwrap().is_none());
        // and a crash between the two symlink swaps (previous already set) is also fine
        root.set_link("current", "aaaa").unwrap();
        root.set_link("previous", "aaaa").unwrap();
        root.write_pending(&Pending {
            op: "switch".into(),
            id: "bbbb".into(),
            prev: Some("aaaa".into()),
            started: now_rfc3339(),
        })
        .unwrap();
        recover(&root, &em).unwrap();
        assert_eq!(
            (root.current_id().as_deref(), root.previous_id().as_deref()),
            (Some("bbbb"), Some("aaaa"))
        );
    }

    #[test]
    fn interrupted_rollback_completes() {
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        do_install(&root, &ld, "bbbb", image("B", "x"), LinkKind::Copy);
        root.write_pending(&Pending {
            op: "rollback".into(),
            id: "aaaa".into(),
            prev: Some("bbbb".into()),
            started: now_rfc3339(),
        })
        .unwrap();
        root.set_link("previous", "bbbb").unwrap(); // first half done... (already was)
        recover(&root, &em).unwrap();
        assert_eq!(
            (root.current_id().as_deref(), root.previous_id().as_deref()),
            (Some("aaaa"), Some("bbbb"))
        );
    }

    #[test]
    fn status_and_shell_spec_use_relative_links() {
        let (_d, root, ld) = setup();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        let s = crate::status::status(&root).unwrap();
        assert_eq!(s["current"]["id"], "aaaa");
        assert!(s["previous"].is_null());
        assert!(s["staged"].is_null());
        let sp = serde_json::to_value(crate::status::shell_spec(&root, Launcher::Proot).unwrap())
            .unwrap();
        assert_eq!(sp["session_label"], "gnome");
        assert_eq!(sp["launcher"]["kind"], "proot");
        assert_eq!(sp["launcher"]["link2symlink"], false);
        assert_eq!(sp["cwd"], "/root");
        assert_eq!(sp["binds"][2]["guest"], "/run/droidc/root");
        assert_eq!(sp["systemd"]["binds"][0]["guest"], "/sys/fs/cgroup");
        assert!(sp["systemd"]["binds"][0]["host"]
            .as_str()
            .unwrap()
            .ends_with("/root/cgroup"));
        assert!(sp["rootfs"]
            .as_str()
            .unwrap()
            .ends_with("/root/current/rootfs"));
        // under proot the launcher binds the guest droidc over sudo/su/pkexec (the spawner shims)
        assert_eq!(
            sp["shims"],
            serde_json::json!(["/usr/bin/sudo", "/usr/bin/su", "/usr/bin/pkexec"])
        );
        assert_eq!(sp["binds"].as_array().unwrap().len(), 3);
        // reading through a different absolute path (as the guest does) works
        let alias = _d.path().join("alias");
        std::os::unix::fs::symlink(&root.path, &alias).unwrap();
        let s2 = crate::status::status(&Root::new(&alias)).unwrap();
        assert_eq!(s2["current"]["id"], "aaaa");
    }

    #[test]
    fn chroot_shell_spec_binds_root_for_link2symlink() {
        let (_d, root, ld) = setup();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Link2Symlink);
        let sp = serde_json::to_value(crate::status::shell_spec(&root, Launcher::Chroot).unwrap())
            .unwrap();
        assert_eq!(
            sp["shims"],
            serde_json::json!([]),
            "chroot keeps the real setuid sudo"
        );
        let l = &sp["launcher"];
        assert_eq!(l["kind"], "chroot");
        assert_eq!(l["requires_root"], true);
        assert!(l["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["fstype"] == "proc"));
        let canon = fs::canonicalize(&root.path).unwrap().display().to_string();
        let binds = sp["binds"].as_array().unwrap();
        assert_eq!(binds.len(), 4);
        assert_eq!(binds[3]["host"], canon.as_str());
        assert_eq!(binds[3]["guest"], canon.as_str());
        // unpacked without root: ownership warning, plus the link2symlink one
        assert_eq!(l["warnings"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn enter_plan_pins_deployment_and_applies_extras() {
        use crate::enter::{plan, EnterOpts};
        let (_d, root, ld) = setup();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        let o = EnterOpts {
            binds: vec!["/data/app/x/libdroidc-guest.so:/usr/bin/droidc".into()],
            env: vec!["TERM=dumb".into(), "WAYLAND_DISPLAY=wayland-0".into()],
            hostname: Some("selkie".into()),
        };
        let (p, _warnings) = plan(&root, &o).unwrap();
        let canon = fs::canonicalize(&root.path).unwrap();
        assert_eq!(
            p.rootfs,
            canon.join("deployments/aaaa/rootfs").display().to_string()
        );
        assert_eq!(p.binds.last().unwrap().guest, "/usr/bin/droidc");
        assert_eq!(p.env["TERM"], "dumb");
        assert_eq!(p.env["WAYLAND_DISPLAY"], "wayland-0");
        assert_eq!(p.env["HOME"], "/root");
        assert!(p
            .mounts
            .iter()
            .any(|m| m.fstype.as_deref() == Some("devpts")));
        for bad in ["nocolon", ":/x", "/host:relative"] {
            let o = EnterOpts {
                binds: vec![bad.into()],
                ..Default::default()
            };
            assert!(plan(&root, &o).is_err(), "{bad}");
        }
        let o = EnterOpts {
            env: vec!["NOEQUALS".into()],
            ..Default::default()
        };
        assert!(plan(&root, &o).is_err());
    }

    /// Real chown/chmod: runs only as root (CI under sudo, or locally in
    /// `unshare --user --map-root-user --map-auto`).
    #[test]
    fn fix_ownership_then_undo_as_root() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if !rustix::process::geteuid().is_root() {
            eprintln!("skipped: needs root");
            return;
        }
        let (_d, root, ld) = setup();
        // the app uid the root belongs to
        std::os::unix::fs::chown(&root.path, Some(1000), Some(1000)).unwrap();
        let layers = vec![Layer::new()
            .dir_as("etc/", 0o755, 0, 0)
            .file_as("etc/shadow", 0o000, 0, 0, b"s")
            .dir_as("usr/", 0o755, 0, 0)
            .dir_as("usr/bin/", 0o755, 0, 0)
            .file_as("usr/bin/su", 0o4755, 0, 0, b"su")
            .hardlink("usr/bin/sudo", "usr/bin/su") // header says 1000:1000 0644
            .symlink("usr/share", "/") // hostile: rows below it must not reach the host
            .dir_as("srv/", 0o750, 999, 998)
            .dir_as("var/", 0o755, 0, 0)
            .dir_as("var/lib/", 0o700, 999, 999)
            .bytes()];
        do_install(&root, &ld, "aaaa", layers, LinkKind::Link2Symlink);
        root.set_link("current", "aaaa").unwrap();
        let rf = root.deployment("aaaa").join("rootfs");
        // A guest that ran as root swapped a recorded directory for a symlink to a host path.
        let sentinel = _d.path().join("sentinel");
        fs::create_dir(&sentinel).unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(rf.join("opt")).unwrap();
        fs::write(rf.join("opt/keep"), "").unwrap();
        fs::rename(rf.join("srv"), rf.join("opt/srv")).unwrap();
        std::os::unix::fs::symlink(&sentinel, rf.join("srv")).unwrap();
        let r = crate::ownership::fix(&root, ".l2s.").unwrap();
        assert_eq!(r["ownership"], "image");
        let su = fs::metadata(rf.join("usr/bin/su")).unwrap();
        assert_eq!((su.uid(), su.mode() & 0o7777), (0, 0o4755), "{r}");
        let sudo = fs::metadata(rf.join("usr/bin/sudo")).unwrap(); // l2s chain, same inode
        assert_eq!(sudo.ino(), su.ino());
        assert_eq!(
            sudo.mode() & 0o7777,
            0o4755,
            "h row must not override the f row"
        );
        let host = fs::metadata(&sentinel).unwrap();
        assert_eq!(
            (host.uid(), host.mode() & 0o777),
            (0, 0o755),
            "host path untouched"
        );
        assert_eq!(r["skipped"]["type mismatch"], 1, "{r}");
        for shadow in ["usr/etc/shadow", "etc/shadow"] {
            let m = fs::metadata(rf.join(shadow)).unwrap();
            assert_eq!((m.uid(), m.mode() & 0o777), (0, 0), "{shadow}");
        }
        assert_eq!(fs::metadata(root.var().join("lib")).unwrap().uid(), 999);
        assert_eq!(
            root.read_record("aaaa").unwrap().ownership.as_deref(),
            Some("image")
        );
        let spec = crate::status::shell_spec(&root, Launcher::Chroot).unwrap();
        let crate::status::LauncherSpec::Chroot { warnings, .. } = spec.launcher else {
            panic!()
        };
        assert!(
            !warnings.iter().any(|w| w.contains("ownership")),
            "{warnings:?}"
        );

        let u = crate::ownership::undo(&root).unwrap();
        assert_eq!(u["deployments"], json!(["aaaa"]));
        for p in [
            rf.join("usr/bin/su"),
            rf.join("etc/shadow"),
            rf.join("opt/srv"),
            root.var().join("lib"),
        ] {
            let m = fs::metadata(&p).unwrap();
            assert_eq!(m.uid(), 1000, "{}", p.display());
            assert_eq!(m.mode() & 0o7000, 0, "{}", p.display());
            // /usr is sealed again: readable, not writable.
            let want = if p.starts_with(rf.join("usr")) {
                0o400
            } else {
                0o600
            };
            assert_eq!(m.mode() & 0o600, want, "{}", p.display());
        }
        assert!(root.read_record("aaaa").unwrap().ownership.is_none());
        let host = fs::metadata(&sentinel).unwrap();
        assert_eq!(host.uid(), 0, "undo does not follow symlinks");
        let _ = fs::set_permissions(&root.path, fs::Permissions::from_mode(0o755));
    }

    #[test]
    fn image_owned_deployments_need_root() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Copy);
        root.set_link("current", "aaaa").unwrap();
        let mut r = root.read_record("aaaa").unwrap();
        r.ownership = Some("image".into());
        write_record(&root.deployment("aaaa").join("deployment.json"), &r).unwrap();
        let e = gc(&root, &em).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Invalid);
        assert!(e.message.contains("fix-ownership --undo"), "{}", e.message);
        assert!(root.deployment("aaaa").is_dir());
        // read-only commands still work
        crate::status::status(&root).unwrap();
    }

    #[test]
    fn apply_ownership_needs_root_before_any_network() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let (_d, root, _ld) = setup();
        let (em, _) = capture();
        let mut o = SwitchOpts::new("registry.invalid/never/fetched:latest");
        o.apply_ownership = true;
        let e = switch_locked(&root, &em, &o).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Invalid, "{}", e.message);
        assert!(e.message.contains("root"), "{}", e.message);
    }

    /// Runs only as root, like `fix_ownership_then_undo_as_root`.
    #[test]
    fn apply_ownership_applies_once_as_root() {
        use std::os::unix::fs::MetadataExt;
        if !rustix::process::geteuid().is_root() {
            eprintln!("skipped: needs root");
            return;
        }
        let (_d, root, ld) = setup();
        let (em, _) = capture();
        let layers = vec![Layer::new()
            .dir_as("usr/", 0o755, 0, 0)
            .dir_as("usr/bin/", 0o755, 0, 0)
            .file_as("usr/bin/su", 0o4755, 0, 0, b"su")
            .bytes()];
        do_install(&root, &ld, "aaaa", layers, LinkKind::Copy);
        root.set_link("current", "aaaa").unwrap();
        let mut o = SwitchOpts::new("test/aaaa:latest");
        let skipped = apply_ownership_if_needed(&root, &em, &o, "aaaa").unwrap();
        assert!(skipped.is_null(), "not requested");
        o.apply_ownership = true;
        let r = apply_ownership_if_needed(&root, &em, &o, "aaaa").unwrap();
        assert_eq!(r["ownership"], "image");
        let su = root.deployment("aaaa").join("rootfs/usr/bin/su");
        assert_eq!(fs::metadata(&su).unwrap().mode() & 0o7777, 0o4755);
        assert!(apply_ownership_if_needed(&root, &em, &o, "aaaa")
            .unwrap()
            .is_null());
    }

    /// Runs only as root, like `fix_ownership_then_undo_as_root`.
    #[test]
    fn root_commands_hand_back_named_containers() {
        use std::os::unix::fs::MetadataExt;
        if !rustix::process::geteuid().is_root() {
            eprintln!("skipped: needs root");
            return;
        }
        let uid = |p: &Path| fs::symlink_metadata(p).unwrap().uid();
        let (em, _) = capture();
        let d = tempfile::tempdir().unwrap();
        let rp = d.path().canonicalize().unwrap().join("root");
        fs::create_dir(&rp).unwrap();
        std::os::unix::fs::chown(&rp, Some(1000), Some(1000)).unwrap();
        let dev = Root::with_container(&rp, "dev").unwrap();
        gc(&dev, &em).unwrap();
        for p in [
            "containers",
            "containers/dev",
            "containers/dev/var",
            "containers/dev/home/root",
        ] {
            assert_eq!(uid(&rp.join(p)), 1000, "{p}");
        }
        let ld = d.path().join("layers");
        fs::create_dir(&ld).unwrap();
        let layers = vec![Layer::new()
            .dir_as("usr/", 0o755, 0, 0)
            .dir_as("usr/bin/", 0o755, 0, 0)
            .file_as("usr/bin/su", 0o4755, 0, 0, b"su")
            .dir("var/", 0o755)
            .dir_as("var/lib/", 0o700, 0, 0)
            .bytes()];
        do_install(&dev, &ld, "aaaa", layers, LinkKind::Copy);
        let su = dev.deployment("aaaa").join("rootfs/usr/bin/su");
        gc(&dev, &em).unwrap();
        assert_eq!(uid(&su), 1000);
        assert_eq!(uid(&dev.current_link()), 1000);
        // Image-owned in the named container: its rootfs and var keep image owners.
        crate::ownership::fix(&dev, ".l2s.").unwrap();
        assert_eq!(uid(&su), 0);
        gc(&Root::new(&rp), &em).unwrap();
        assert_eq!(uid(&su), 0);
        assert_eq!(uid(&dev.var().join("lib")), 0);
        assert_eq!(uid(&dev.deployment("aaaa").join("deployment.json")), 1000);
    }

    #[test]
    fn root_commands_hand_back_what_they_create() {
        use std::os::unix::fs::MetadataExt;
        if !rustix::process::geteuid().is_root() {
            eprintln!("skipped: needs root");
            return;
        }
        let uid = |p: &Path| fs::symlink_metadata(p).unwrap().uid();
        let (em, _) = capture();
        // 1. a fresh root the app created: everything droidc adds as root is handed back
        let d = tempfile::tempdir().unwrap();
        let rp = d.path().canonicalize().unwrap().join("root");
        fs::create_dir(&rp).unwrap();
        std::os::unix::fs::chown(&rp, Some(1000), Some(1000)).unwrap();
        gc(&Root::new(&rp), &em).unwrap();
        for p in [
            "lock",
            "policy.json",
            "blobs",
            "blobs/tmp",
            "deployments",
            "staging",
            "var",
            "home/root",
        ] {
            assert_eq!(uid(&rp.join(p)), 1000, "{p}");
        }
        // 2. a deployment made as root without --apply-ownership is handed back too
        let root = Root::new(&rp);
        let ld = d.path().join("layers");
        fs::create_dir(&ld).unwrap();
        let layers = vec![Layer::new()
            .dir_as("usr/", 0o755, 0, 0)
            .dir_as("usr/bin/", 0o755, 0, 0)
            .file_as("usr/bin/su", 0o4755, 0, 0, b"su")
            .dir("var/", 0o755)
            .dir_as("var/lib/", 0o700, 0, 0)
            .bytes()];
        do_install(&root, &ld, "aaaa", layers, LinkKind::Copy);
        root.set_link("current", "aaaa").unwrap();
        let su = root.deployment("aaaa").join("rootfs/usr/bin/su");
        assert_eq!(uid(&su), 0, "made as root");
        gc(&Root::new(&rp), &em).unwrap();
        assert_eq!(uid(&su), 1000);
        assert_eq!(uid(&root.current_link()), 1000);
        // 3. image-owned: rootfs keeps image owners, droidc's own files are handed back, var too
        crate::ownership::fix(&Root::new(&rp), ".l2s.").unwrap();
        assert_eq!(uid(&su), 0);
        assert_eq!(uid(&root.deployment("aaaa").join("deployment.json")), 1000);
        assert_eq!(
            uid(&root.var().join("lib")),
            0,
            "var keeps image owners in image mode"
        );
        fs::write(root.blobs().join("made-as-root"), "").unwrap();
        gc(&Root::new(&rp), &em).unwrap();
        assert_eq!(uid(&su), 0);
        assert_eq!(uid(&root.var().join("lib")), 0);
        // (gc removed the unreferenced blob; the blobs dir itself stays the app's)
        assert_eq!(uid(&root.blobs()), 1000);
        // 4. a root directory that belongs to root: nothing to hand back to
        std::os::unix::fs::chown(&rp, Some(0), Some(0)).unwrap();
        assert_eq!(hand_back(&root).unwrap(), 0);
        crate::ownership::undo(&Root::new(&rp)).unwrap_err();
    }

    #[test]
    fn insufficient_space_is_storage_error() {
        let (_d, root, _ld) = setup();
        let big = Descriptor {
            media_type: String::new(),
            digest: "sha256:ff".into(),
            size: u64::MAX / 4,
            annotations: Default::default(),
            platform: None,
        };
        let e = check_space(&root, &[big]).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Storage);
    }
}
