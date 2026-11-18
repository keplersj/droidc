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
        }
    }
}

/// Create the root (if needed), canonicalize it and take the lock.
fn open_root(path: &Path) -> Result<(Root, RootLock)> {
    fs::create_dir_all(path).context(path.display())?;
    let canon = fs::canonicalize(path).context(path.display())?;
    let root = Root::new(canon);
    let lock = RootLock::acquire(&root)?;
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

/// Commit: seed var, `previous` := old current, `current` := new, drop PENDING.
fn finish_switch(root: &Root, p: &Pending) -> Result<()> {
    seed_var(root, &p.id)?;
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

fn write_record(path: &Path, rec: &Deployment) -> Result<()> {
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
    }
}

pub fn switch(root_path: &Path, em: &Emitter, opts: &SwitchOpts) -> Result<Value> {
    let (root, _lock) = open_root(root_path)?;
    recover(&root, em)?;
    switch_locked(&root, em, opts)
}

pub fn switch_locked(root: &Root, em: &Emitter, opts: &SwitchOpts) -> Result<Value> {
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
        let rec = root.read_record(&id).ok();
        return Ok(
            json!({"changed": false, "id": id, "manifest_digest": res.manifest_digest, "deployment": rec}),
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
    let g = gc_locked(root, em)?;
    Ok(json!({"changed": true, "id": id, "previous": prev, "deployment": rec, "gc": g}))
}

pub fn upgrade(
    root_path: &Path,
    em: &Emitter,
    http: HttpConfig,
    l2s_prefix: &str,
) -> Result<Value> {
    let (root, _lock) = open_root(root_path)?;
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
    };
    switch_locked(&root, em, &opts)
}

// ------------------------------------------------------------------ rollback

pub fn rollback(root_path: &Path, em: &Emitter) -> Result<Value> {
    let (root, _lock) = open_root(root_path)?;
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

// ------------------------------------------------------------------ gc

pub fn gc(root_path: &Path, em: &Emitter) -> Result<Value> {
    let (root, _lock) = open_root(root_path)?;
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
    match referenced_blobs(root, &keep) {
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
        let r = rollback(&root.path, &em).unwrap();
        assert_eq!(r["current"], "aaaa");
        assert_eq!(root.current_id().as_deref(), Some("aaaa"));
        assert_eq!(root.previous_id().as_deref(), Some("bbbb"));
        rollback(&root.path, &em).unwrap();
        assert_eq!(root.current_id().as_deref(), Some("bbbb"));

        // a third deployment: A becomes unreferenced and is collected
        do_install(&root, &ld, "cccc", image("C", "motd-C"), LinkKind::Copy);
        assert_eq!(root.previous_id().as_deref(), Some("bbbb"));
        assert!(root.deployment("aaaa").exists());
        let g = gc(&root.path, &em).unwrap();
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
        let g = gc(&root.path, &em).unwrap();
        assert_eq!(g["blobs_removed"], 1);
        assert!(root.blob(&l1).exists());
        assert!(!root.blobs().join("2".repeat(64)).exists());
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
        let s = crate::status::status(&root.path).unwrap();
        assert_eq!(s["current"]["id"], "aaaa");
        assert!(s["previous"].is_null());
        assert!(s["staged"].is_null());
        let sp = crate::status::shell_spec(&root.path, Launcher::Proot).unwrap();
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
        // reading through a different absolute path (as the guest does) works
        let alias = _d.path().join("alias");
        std::os::unix::fs::symlink(&root.path, &alias).unwrap();
        let s2 = crate::status::status(&alias).unwrap();
        assert_eq!(s2["current"]["id"], "aaaa");
    }

    #[test]
    fn chroot_shell_spec_binds_root_for_link2symlink() {
        let (_d, root, ld) = setup();
        do_install(&root, &ld, "aaaa", image("A", "x"), LinkKind::Link2Symlink);
        let sp = crate::status::shell_spec(&root.path, Launcher::Chroot).unwrap();
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
