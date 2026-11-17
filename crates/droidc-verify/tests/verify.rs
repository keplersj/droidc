use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use droidc_verify::*;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::{EncodePublicKey, LineEnding};
use serde_json::Value;
use sha2::{Digest, Sha256};

const REPO: &str = "ghcr.io/projectbluefin/bluefin-lts";

struct Fixture {
    digest: String,
    sig: SignatureBlob,
}

fn load(arch: &str) -> Fixture {
    let base = format!("{}/tests/fixtures/bluefin-lts-stable-{arch}", env!("CARGO_MANIFEST_DIR"));
    let j: Value = serde_json::from_slice(&std::fs::read(format!("{base}.json")).unwrap()).unwrap();
    let annotations: BTreeMap<String, String> = serde_json::from_value(j["annotations"].clone()).unwrap();
    Fixture {
        digest: j["manifest_digest"].as_str().unwrap().to_string(),
        sig: SignatureBlob { payload: std::fs::read(format!("{base}.payload")).unwrap(), annotations },
    }
}

/// Fixtures were captured 2026-11-17; "now" is just after the integrated time of 2026-10-12.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_794_000_000) // 2026-11-06
}

fn bundle(f: &Fixture) -> Value {
    serde_json::from_str(&f.sig.annotations[ANN_BUNDLE]).unwrap()
}

#[test]
fn real_bluefin_fixtures_verify_with_default_policy() {
    for arch in ["arm64", "amd64"] {
        let f = load(arch);
        let v = verify(&Policy::default_policy(), REPO, &f.digest, &[f.sig.clone()], now()).unwrap();
        assert_eq!(v.mode, "keyless");
        assert_eq!(v.issuer.as_deref(), Some(GITHUB_ACTIONS_ISSUER));
        assert_eq!(
            v.subject.as_deref(),
            Some("https://github.com/projectbluefin/actions/.github/workflows/reusable-build.yml@refs/tags/v1")
        );
        assert_eq!(v.policy_match, REPO);
    }
}

#[test]
fn any_valid_signature_suffices() {
    let f = load("arm64");
    let mut junk = f.sig.clone();
    junk.payload = b"{}".to_vec();
    verify(&Policy::default_policy(), REPO, &f.digest, &[junk, f.sig.clone()], now()).unwrap();
}

#[test]
fn tampered_payload_digest_fails() {
    let f = load("arm64");
    let mut s = f.sig.clone();
    s.payload = String::from_utf8(s.payload).unwrap().replace("sha256:d2a1", "sha256:d2a2").into_bytes();
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[s], now()).unwrap_err();
    assert!(matches!(e, VerifyError::DigestMismatch(_)), "{e:?}");
}

#[test]
fn tampered_payload_content_fails_signature() {
    let f = load("arm64");
    let mut s = f.sig.clone();
    s.payload = String::from_utf8(s.payload).unwrap().replace("bluefin-lts", "bluefin-ltz").into_bytes();
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[s], now()).unwrap_err();
    assert!(matches!(e, VerifyError::BadSignature(_)), "{e:?}");
}

#[test]
fn wrong_manifest_digest_fails() {
    let f = load("arm64");
    let other = load("amd64");
    let e = verify(&Policy::default_policy(), REPO, &other.digest, &[f.sig.clone()], now()).unwrap_err();
    assert!(matches!(e, VerifyError::DigestMismatch(_)), "{e:?}");
}

fn policy_with(subject_regex: &str, issuer: &str) -> Policy {
    let mut p = Policy::default_policy();
    let k = p.images[0].keyless.as_mut().unwrap();
    k.subject_regex = Some(subject_regex.into());
    k.issuer = issuer.into();
    p
}

#[test]
fn wrong_subject_regex_fails() {
    let f = load("arm64");
    let p = policy_with(r"^https://github\.com/ublue-os/", GITHUB_ACTIONS_ISSUER);
    let e = verify(&p, REPO, &f.digest, &[f.sig], now()).unwrap_err();
    assert!(matches!(e, VerifyError::IdentityMismatch(_)), "{e:?}");
}

#[test]
fn wrong_issuer_fails() {
    let f = load("arm64");
    let p = policy_with(r"^https://github\.com/projectbluefin/", "https://accounts.google.com");
    let e = verify(&p, REPO, &f.digest, &[f.sig], now()).unwrap_err();
    assert!(matches!(e, VerifyError::IdentityMismatch(_)), "{e:?}");
}

#[test]
fn exact_subject_is_enforced() {
    let f = load("arm64");
    let mut p = Policy::default_policy();
    let k = p.images[0].keyless.as_mut().unwrap();
    k.subject_regex = None;
    k.subject = Some("https://github.com/projectbluefin/actions/.github/workflows/other.yml@refs/tags/v1".into());
    let e = verify(&p, REPO, &f.digest, &[f.sig], now()).unwrap_err();
    assert!(matches!(e, VerifyError::IdentityMismatch(_)), "{e:?}");
}

#[test]
fn corrupted_set_fails() {
    let f = load("arm64");
    let mut b = bundle(&f);
    let set = b["SignedEntryTimestamp"].as_str().unwrap().to_string();
    // Flip a character in the middle of the base64.
    let mut c: Vec<char> = set.chars().collect();
    c[20] = if c[20] == 'A' { 'B' } else { 'A' };
    b["SignedEntryTimestamp"] = Value::String(c.into_iter().collect());
    let mut s = f.sig.clone();
    s.annotations.insert(ANN_BUNDLE.into(), b.to_string());
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[s], now()).unwrap_err();
    assert!(matches!(e, VerifyError::Tlog(_)), "{e:?}");
}

#[test]
fn modified_integrated_time_breaks_set() {
    let f = load("arm64");
    let mut b = bundle(&f);
    b["Payload"]["integratedTime"] = Value::from(b["Payload"]["integratedTime"].as_i64().unwrap() + 1);
    let mut s = f.sig.clone();
    s.annotations.insert(ANN_BUNDLE.into(), b.to_string());
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[s], now()).unwrap_err();
    assert!(matches!(e, VerifyError::Tlog(_)), "{e:?}");
}

#[test]
fn missing_bundle_fails() {
    let f = load("arm64");
    let mut s = f.sig.clone();
    s.annotations.remove(ANN_BUNDLE);
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[s], now()).unwrap_err();
    assert!(matches!(e, VerifyError::Tlog(_)), "{e:?}");
}

#[test]
fn signature_from_other_signature_fails() {
    let f = load("arm64");
    let g = load("amd64");
    let mut s = f.sig.clone();
    s.annotations.insert(ANN_SIGNATURE.into(), g.sig.annotations[ANN_SIGNATURE].clone());
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[s], now()).unwrap_err();
    assert!(matches!(e, VerifyError::BadSignature(_)), "{e:?}");
}

#[test]
fn no_matching_policy_entry_rejects() {
    let f = load("arm64");
    let e = verify(&Policy::default_policy(), "quay.io/centos-bootc/centos-bootc", &f.digest, &[f.sig], now()).unwrap_err();
    assert!(matches!(e, VerifyError::NoPolicyMatch(ref r) if r == "quay.io/centos-bootc/centos-bootc"), "{e:?}");
}

#[test]
fn no_signatures_fails() {
    let f = load("arm64");
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[], now()).unwrap_err();
    assert_eq!(e, VerifyError::NoSignatures);
}

#[test]
fn integrated_time_in_the_future_fails() {
    let f = load("arm64");
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[f.sig], UNIX_EPOCH + Duration::from_secs(1_700_000_000)).unwrap_err();
    assert!(matches!(e, VerifyError::Tlog(_)), "{e:?}");
}

// ---- expired-at-integratedTime: needs a re-signed SET, so substitute our own Rekor key in a
// ---- trusted root override (the real Fulcio chain and leaf are still used).

fn test_signing_key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).unwrap()
}

fn spki_der(k: &SigningKey) -> Vec<u8> {
    k.verifying_key().to_public_key_der().unwrap().as_bytes().to_vec()
}

fn trusted_root_with_rekor_key(k: &SigningKey, path: &std::path::Path) {
    use base64::Engine;
    let mut root: Value = serde_json::from_str(EMBEDDED_TRUSTED_ROOT).unwrap();
    let t = &mut root["tlogs"][0]["publicKey"];
    t["rawBytes"] = Value::String(base64::engine::general_purpose::STANDARD.encode(spki_der(k)));
    t["validFor"] = serde_json::json!({"start": "2020-01-01T00:00:00Z"});
    std::fs::write(path, root.to_string()).unwrap();
}

fn resign_set(f: &Fixture, k: &SigningKey, integrated: i64) -> SignatureBlob {
    use base64::Engine;
    let mut b = bundle(f);
    b["Payload"]["integratedTime"] = Value::from(integrated);
    b["Payload"]["logID"] = Value::String(hex::encode(Sha256::digest(spki_der(k))));
    let p = &b["Payload"];
    let signed = serde_json::json!({"body": p["body"], "integratedTime": p["integratedTime"], "logID": p["logID"], "logIndex": p["logIndex"]});
    let canon = serde_json_canonicalizer::to_vec(&signed).unwrap();
    let sig: Signature = k.sign(&canon);
    b["SignedEntryTimestamp"] = Value::String(base64::engine::general_purpose::STANDARD.encode(sig.to_der().as_bytes()));
    let mut s = f.sig.clone();
    s.annotations.insert(ANN_BUNDLE.into(), b.to_string());
    s
}

fn override_policy(dir: &std::path::Path, k: &SigningKey) -> Policy {
    let path = dir.join("trusted_root.json");
    trusted_root_with_rekor_key(k, &path);
    let mut p = Policy::default_policy();
    p.trusted_root_path = Some(path.to_string_lossy().into_owned());
    // Round-trip through JSON to exercise (de)serialization of the override field.
    Policy::from_json(&p.to_json()).unwrap()
}

#[test]
fn override_root_with_resigned_set_verifies_and_expiry_fails() {
    let dir = std::env::temp_dir().join(format!("droidc-verify-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let k = test_signing_key(7);
    let p = override_policy(&dir, &k);
    let f = load("arm64");
    let orig = bundle(&f)["Payload"]["integratedTime"].as_i64().unwrap();

    // Sanity: same integrated time, our Rekor key -> verifies.
    let ok = resign_set(&f, &k, orig);
    verify(&p, REPO, &f.digest, &[ok], now()).unwrap();

    // Embedded root does not know our Rekor key -> rejected.
    let ok = resign_set(&f, &k, orig);
    let e = verify(&Policy::default_policy(), REPO, &f.digest, &[ok], now()).unwrap_err();
    assert!(matches!(e, VerifyError::Tlog(_)), "{e:?}");

    // Integrated one hour after issuance (leaf lives 10 minutes) -> expired.
    let late = resign_set(&f, &k, orig + 3600);
    let e = verify(&p, REPO, &f.digest, &[late], now()).unwrap_err();
    assert!(matches!(e, VerifyError::CertExpired(_)), "{e:?}");

    // Integrated before the cert was issued -> expired too.
    let early = resign_set(&f, &k, orig - 3600);
    let e = verify(&p, REPO, &f.digest, &[early], now()).unwrap_err();
    assert!(matches!(e, VerifyError::CertExpired(_)), "{e:?}");

    // Integrated before the Fulcio CA existed -> chain failure.
    let ancient = resign_set(&f, &k, 1_600_000_000);
    let e = verify(&p, REPO, &f.digest, &[ancient], now()).unwrap_err();
    assert!(matches!(e, VerifyError::CertChain(_)), "{e:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- key-based ----

fn key_policy(k: &SigningKey) -> Policy {
    let pem = k.verifying_key().to_public_key_pem(LineEnding::LF).unwrap();
    Policy::from_json(&serde_json::json!({
        "version": 1, "default": "reject",
        "images": [{"match": "ghcr.io/ublue-os/*", "keys": [pem]}]
    }).to_string()).unwrap()
}

fn key_sig(k: &SigningKey, digest: &str) -> SignatureBlob {
    use base64::Engine;
    let payload = format!(
        r#"{{"critical":{{"identity":{{"docker-reference":"ghcr.io/ublue-os/x"}},"image":{{"docker-manifest-digest":"{digest}"}},"type":"cosign container image signature"}},"optional":null}}"#
    ).into_bytes();
    let sig: Signature = k.sign(&payload);
    let mut annotations = BTreeMap::new();
    annotations.insert(ANN_SIGNATURE.to_string(), base64::engine::general_purpose::STANDARD.encode(sig.to_der().as_bytes()));
    SignatureBlob { payload, annotations }
}

#[test]
fn key_based_verifies_and_wrong_key_fails() {
    let d = format!("sha256:{}", "ab".repeat(32));
    let k1 = test_signing_key(1);
    let k2 = test_signing_key(2);
    let s = key_sig(&k1, &d);
    let v = verify(&key_policy(&k1), "ghcr.io/ublue-os/x", &d, &[s.clone()], now()).unwrap();
    assert_eq!((v.mode.as_str(), v.subject, v.policy_match.as_str()), ("key", None, "ghcr.io/ublue-os/*"));
    let e = verify(&key_policy(&k2), "ghcr.io/ublue-os/x", &d, &[s.clone()], now()).unwrap_err();
    assert!(matches!(e, VerifyError::BadSignature(_)), "{e:?}");
    // digest mismatch
    let e = verify(&key_policy(&k1), "ghcr.io/ublue-os/x", &format!("sha256:{}", "cd".repeat(32)), &[s.clone()], now()).unwrap_err();
    assert!(matches!(e, VerifyError::DigestMismatch(_)), "{e:?}");
    // glob does not match the bare prefix or other orgs
    for r in ["ghcr.io/ublue-os", "ghcr.io/ublue-osx/x", "ghcr.io/other/x"] {
        assert!(matches!(verify(&key_policy(&k1), r, &d, &[s.clone()], now()), Err(VerifyError::NoPolicyMatch(_))), "{r}");
    }
}

#[test]
fn default_policy_ublue_key_rejects_foreign_signature() {
    let d = format!("sha256:{}", "ab".repeat(32));
    let s = key_sig(&test_signing_key(3), &d);
    let e = verify(&Policy::default_policy(), "ghcr.io/ublue-os/silverblue-main", &d, &[s], now()).unwrap_err();
    assert!(matches!(e, VerifyError::BadSignature(_)), "{e:?}");
}

// ---- policy ----

#[test]
fn policy_roundtrip_and_validation() {
    let p = Policy::default_policy();
    assert_eq!(Policy::from_json(&p.to_json()).unwrap(), p);
    for bad in [
        r#"{"version":2,"default":"reject","images":[]}"#,
        r#"{"version":1,"default":"accept","images":[]}"#,
        r#"{"version":1,"default":"reject","images":[{"match":"a"}]}"#,
        r#"{"version":1,"default":"reject","images":[{"match":"a","keyless":{"issuer":"x"}}]}"#,
        r#"{"version":1,"default":"reject","images":[{"match":"a","keyless":{"issuer":"x","subject_regex":"("}}]}"#,
        "not json",
    ] {
        assert!(matches!(Policy::from_json(bad), Err(VerifyError::Policy(_))), "{bad}");
    }
}

#[test]
fn most_specific_policy_entry_wins() {
    let p = Policy::from_json(&serde_json::json!({
        "version":1,"default":"reject","images":[
          {"match":"ghcr.io/o/*","keys":["-----BEGIN PUBLIC KEY-----\nx\n-----END PUBLIC KEY-----"]},
          {"match":"ghcr.io/o/exact","keys":["-----BEGIN PUBLIC KEY-----\ny\n-----END PUBLIC KEY-----"]}
        ]}).to_string()).unwrap();
    assert_eq!(p.find("ghcr.io/o/exact").unwrap().match_, "ghcr.io/o/exact");
    assert_eq!(p.find("ghcr.io/o/other").unwrap().match_, "ghcr.io/o/*");
    assert!(p.find("ghcr.io/p/x").is_none());
}
