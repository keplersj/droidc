//! Network integration checks. Skipped unless `DROIDC_NET_TESTS=1` (CI sets it).
//! They fetch only manifests and cosign `.sig` manifests, never layers.

use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> (i32, Vec<serde_json::Value>) {
    let out = Command::new(env!("CARGO_BIN_EXE_droidc"))
        .arg("--root")
        .arg(root)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    let lines: Vec<serde_json::Value> = String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    (out.status.code().unwrap(), lines)
}

fn enabled() -> bool {
    std::env::var("DROIDC_NET_TESTS").as_deref() == Ok("1")
}

#[test]
fn bluefin_lts_keyless_signature_verifies_without_downloading_layers() {
    if !enabled() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let (code, lines) = run(d.path(), &["switch", "ghcr.io/projectbluefin/bluefin-lts:stable", "--verify-only"]);
    let last = lines.last().unwrap();
    assert_eq!(code, 0, "{last}");
    assert_eq!(last["event"], "result");
    assert_eq!(last["ok"], true);
    assert_eq!(last["data"]["signature"]["mode"], "keyless");
    assert_eq!(last["data"]["signature"]["verified"], true);
    assert!(last["data"]["signature"]["subject"].as_str().unwrap().starts_with("https://github.com/projectbluefin/"));
    // nothing but the policy and empty dirs: no blobs for layers
    let blobs = std::fs::read_dir(d.path().join("blobs/sha256")).map(|r| r.count()).unwrap_or(0);
    assert_eq!(blobs, 0);
}

#[test]
fn unsigned_image_is_rejected_with_signature_kind() {
    if !enabled() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let (code, lines) = run(d.path(), &["switch", "quay.io/centos-bootc/centos-bootc:stream10", "--verify-only"]);
    assert_eq!(code, 1);
    let last = lines.last().unwrap();
    assert_eq!(last["ok"], false);
    assert_eq!(last["error"]["kind"], "signature");
}

#[test]
fn missing_tag_is_not_found() {
    if !enabled() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let (code, lines) = run(d.path(), &["switch", "quay.io/centos-bootc/centos-bootc:no-such-tag-droidc", "--verify-only"]);
    assert_eq!(code, 1);
    assert_eq!(lines.last().unwrap()["error"]["kind"], "not-found");
}
