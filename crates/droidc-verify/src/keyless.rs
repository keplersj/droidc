//! Per-signature verification: payload digest, key-based, and keyless (Fulcio + Rekor) paths.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use x509_cert::der::asn1::Utf8StringRef;
use x509_cert::der::{Decode, Encode};
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{ExtendedKeyUsage, SubjectAltName};
use x509_cert::Certificate;

use crate::crypto::{b64_decode, pem_to_der, verify_ecdsa, Hash};
use crate::trusted_root::TrustedRoot;
use crate::{
    KeylessPolicy, SignatureBlob, VerifyError, ANN_BUNDLE, ANN_CERTIFICATE, ANN_SIGNATURE,
};

const OID_FULCIO_ISSUER_V1: &str = "1.3.6.1.4.1.57264.1.1";
const OID_FULCIO_ISSUER_V2: &str = "1.3.6.1.4.1.57264.1.8";
const OID_SAN: &str = "2.5.29.17";
const OID_EKU: &str = "2.5.29.37";
const OID_EKU_CODE_SIGNING: &str = "1.3.6.1.5.5.7.3.3";
const OID_ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
const OID_ECDSA_SHA384: &str = "1.2.840.10045.4.3.3";
/// Allowed clock skew when comparing the integrated time with `now`.
const SKEW_SECS: i64 = 300;

fn invalid(m: impl Into<String>) -> VerifyError {
    VerifyError::Invalid(m.into())
}

/// Step 1: the payload's `critical.image.docker-manifest-digest` equals the pulled manifest digest.
pub(crate) fn check_payload_digest(
    sig: &SignatureBlob,
    manifest_digest: &str,
) -> Result<(), VerifyError> {
    let v: Value = serde_json::from_slice(&sig.payload)
        .map_err(|e| invalid(format!("payload is not JSON: {e}")))?;
    let crit = v
        .get("critical")
        .ok_or_else(|| invalid("payload has no `critical`"))?;
    let ty = crit.get("type").and_then(Value::as_str).unwrap_or("");
    if ty != "cosign container image signature" {
        return Err(invalid(format!("unexpected payload type `{ty}`")));
    }
    let d = crit
        .get("image")
        .and_then(|i| i.get("docker-manifest-digest"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("payload has no docker-manifest-digest"))?;
    if d != manifest_digest {
        return Err(VerifyError::DigestMismatch(format!(
            "payload signs {d}, expected {manifest_digest}"
        )));
    }
    Ok(())
}

fn ann<'a>(sig: &'a SignatureBlob, k: &str) -> Option<&'a str> {
    sig.annotations.get(k).map(String::as_str)
}

fn sig_bytes(sig: &SignatureBlob) -> Result<Vec<u8>, VerifyError> {
    let s = ann(sig, ANN_SIGNATURE)
        .ok_or_else(|| invalid(format!("missing annotation {ANN_SIGNATURE}")))?;
    b64_decode(s).map_err(invalid)
}

/// Key-based verification: signature over the payload verifies with any of the PEM keys.
pub(crate) fn verify_keys(keys: &[String], sig: &SignatureBlob) -> Result<(), VerifyError> {
    let sb = sig_bytes(sig)?;
    let mut last = "no keys".to_string();
    for pem in keys {
        let der =
            pem_to_der(pem, "PUBLIC KEY").map_err(|e| VerifyError::Policy(format!("key: {e}")))?;
        match verify_ecdsa(&der, None, &sig.payload, &sb) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(VerifyError::BadSignature(format!(
        "no policy key verifies the signature ({last})"
    )))
}

fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

fn cert_secs(t: &x509_cert::time::Time) -> i64 {
    t.to_unix_duration().as_secs() as i64
}

fn cert_window_contains(c: &Certificate, t: i64) -> bool {
    let v = c.tbs_certificate().validity();
    cert_secs(&v.not_before) <= t && t <= cert_secs(&v.not_after)
}

/// Verify that `cert` is signed by `issuer`.
fn cert_signed_by(cert: &Certificate, issuer: &Certificate) -> Result<(), String> {
    let hash = match cert.signature_algorithm().oid.to_string().as_str() {
        OID_ECDSA_SHA256 => Hash::Sha256,
        OID_ECDSA_SHA384 => Hash::Sha384,
        o => return Err(format!("unsupported certificate signature algorithm {o}")),
    };
    let tbs = cert.tbs_certificate().to_der().map_err(|e| e.to_string())?;
    let spki = issuer
        .tbs_certificate()
        .subject_public_key_info()
        .to_der()
        .map_err(|e| e.to_string())?;
    let sig = cert
        .signature()
        .as_bytes()
        .ok_or("unaligned signature bits")?;
    verify_ecdsa(&spki, Some(hash), &tbs, sig)
}

/// Build and verify the chain leaf -> ... -> self-signed root from the trusted root's CA pool.
fn verify_chain(root: &TrustedRoot, leaf: &Certificate, t: i64) -> Result<(), VerifyError> {
    let err = |m: String| VerifyError::CertChain(m);
    let mut last =
        "no certificate authority in the trusted root is valid at the integrated time".to_string();
    for ca in root.cas.iter().filter(|c| c.valid.contains(t)) {
        let pool: Vec<Certificate> = ca
            .certs
            .iter()
            .map(|d| Certificate::from_der(d).map_err(|e| err(format!("trusted root cert: {e}"))))
            .collect::<Result<_, _>>()?;
        let mut cur = leaf;
        for _ in 0..4 {
            let issuer_name = cur
                .tbs_certificate()
                .issuer()
                .to_der()
                .map_err(|e| err(e.to_string()))?;
            let mut found = None;
            for cand in &pool {
                let sn = cand
                    .tbs_certificate()
                    .subject()
                    .to_der()
                    .map_err(|e| err(e.to_string()))?;
                if sn == issuer_name && cert_signed_by(cur, cand).is_ok() {
                    found = Some(cand);
                    break;
                }
            }
            let Some(issuer) = found else {
                last = "certificate is not signed by a trusted Fulcio CA".into();
                break;
            };
            if !cert_window_contains(issuer, t) {
                last = "Fulcio CA certificate not valid at the integrated time".into();
                break;
            }
            if issuer.tbs_certificate().subject().to_der().ok()
                == issuer.tbs_certificate().issuer().to_der().ok()
            {
                // Self-signed root reached (its signature over itself was just verified).
                return Ok(());
            }
            cur = issuer;
        }
    }
    Err(err(last))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Bundle {
    signed_entry_timestamp: String,
    payload: Value,
}

fn leaf_identity(cert: &Certificate) -> Result<(String, String), VerifyError> {
    let err = |m: &str| VerifyError::IdentityMismatch(m.to_string());
    let exts = cert
        .tbs_certificate()
        .extensions()
        .ok_or_else(|| err("leaf has no extensions"))?;
    let mut san: Option<String> = None;
    let mut issuer: Option<String> = None;
    let mut code_signing = false;
    for e in exts {
        match e.extn_id.to_string().as_str() {
            OID_SAN => {
                let s = SubjectAltName::from_der(e.extn_value.as_bytes())
                    .map_err(|e| VerifyError::CertChain(format!("bad SAN: {e}")))?;
                for n in s.0 {
                    match n {
                        GeneralName::UniformResourceIdentifier(u) if san.is_none() => {
                            san = Some(u.to_string())
                        }
                        GeneralName::Rfc822Name(u) if san.is_none() => san = Some(u.to_string()),
                        _ => {}
                    }
                }
            }
            OID_FULCIO_ISSUER_V2 => {
                if let Ok(s) = Utf8StringRef::from_der(e.extn_value.as_bytes()) {
                    issuer = Some(s.as_str().to_string());
                }
            }
            OID_FULCIO_ISSUER_V1 if issuer.is_none() => {
                // Deprecated form: raw UTF-8, not DER-wrapped.
                issuer = std::str::from_utf8(e.extn_value.as_bytes())
                    .ok()
                    .map(str::to_string);
            }
            OID_EKU => {
                if let Ok(k) = ExtendedKeyUsage::from_der(e.extn_value.as_bytes()) {
                    code_signing = k.0.iter().any(|o| o.to_string() == OID_EKU_CODE_SIGNING);
                }
            }
            _ => {}
        }
    }
    if !code_signing {
        return Err(VerifyError::CertChain(
            "leaf certificate lacks the code signing EKU".into(),
        ));
    }
    Ok((
        san.ok_or_else(|| err("leaf certificate has no URI/email SAN"))?,
        issuer.ok_or_else(|| err("leaf certificate has no Fulcio OIDC issuer extension"))?,
    ))
}

/// Keyless verification of one signature. Returns (subject, issuer).
pub(crate) fn verify_keyless(
    root: &TrustedRoot,
    pol: &KeylessPolicy,
    sig: &SignatureBlob,
    now: SystemTime,
) -> Result<(String, String), VerifyError> {
    let cert_pem = ann(sig, ANN_CERTIFICATE)
        .ok_or_else(|| invalid("no certificate annotation (not a keyless signature)"))?;
    let cert_der = pem_to_der(cert_pem, "CERTIFICATE").map_err(VerifyError::CertChain)?;
    let leaf = Certificate::from_der(&cert_der)
        .map_err(|e| VerifyError::CertChain(format!("leaf: {e}")))?;
    let sb = sig_bytes(sig)?;

    let tlog = |m: String| VerifyError::Tlog(m);
    let bundle_s = ann(sig, ANN_BUNDLE).ok_or_else(|| tlog("no Rekor bundle annotation".into()))?;
    let bundle: Bundle =
        serde_json::from_str(bundle_s).map_err(|e| tlog(format!("bad bundle: {e}")))?;
    let p = &bundle.payload;
    let integrated = p
        .get("integratedTime")
        .and_then(Value::as_i64)
        .ok_or_else(|| tlog("bundle has no integratedTime".into()))?;
    let log_id = p
        .get("logID")
        .and_then(Value::as_str)
        .ok_or_else(|| tlog("bundle has no logID".into()))?;
    let body_b64 = p
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| tlog("bundle has no body".into()))?;
    if p.get("logIndex").and_then(Value::as_i64).is_none() {
        return Err(tlog("bundle has no logIndex".into()));
    }

    // (2) chain
    verify_chain(root, &leaf, integrated)?;

    // (3) signature over payload with the leaf key
    let leaf_spki = leaf
        .tbs_certificate()
        .subject_public_key_info()
        .to_der()
        .map_err(|e| VerifyError::CertChain(e.to_string()))?;
    verify_ecdsa(&leaf_spki, None, &sig.payload, &sb).map_err(VerifyError::BadSignature)?;

    // (4) Rekor SET over canonical JSON of {body, integratedTime, logID, logIndex}
    let key = root
        .tlogs
        .iter()
        .find(|k| hex::encode(Sha256::digest(&k.spki)) == log_id)
        .ok_or_else(|| tlog(format!("logID {log_id} is not in the trusted root")))?;
    if !key.valid.contains(integrated) {
        return Err(tlog(
            "Rekor key was not valid at the integrated time".into(),
        ));
    }
    if !key.details.contains("ECDSA_P256") {
        return Err(tlog(format!(
            "unsupported Rekor key type `{}`",
            key.details
        )));
    }
    let signed = serde_json::json!({
        "body": body_b64,
        "integratedTime": integrated,
        "logID": log_id,
        "logIndex": p["logIndex"],
    });
    let canon = serde_json_canonicalizer::to_vec(&signed)
        .map_err(|e| tlog(format!("canonicalize: {e}")))?;
    let set = b64_decode(&bundle.signed_entry_timestamp).map_err(tlog)?;
    verify_ecdsa(&key.spki, Some(Hash::Sha256), &canon, &set)
        .map_err(|e| tlog(format!("SignedEntryTimestamp: {e}")))?;
    if integrated > secs(now) + SKEW_SECS {
        return Err(tlog("integrated time is in the future".into()));
    }

    // (5) leaf validity contains integratedTime
    if !cert_window_contains(&leaf, integrated) {
        return Err(VerifyError::CertExpired(format!(
            "integrated time {integrated} outside leaf validity"
        )));
    }

    // (6) Rekor hashedrekord body matches payload hash, signature, certificate
    let body: Value = serde_json::from_slice(&b64_decode(body_b64).map_err(tlog)?)
        .map_err(|e| tlog(format!("body: {e}")))?;
    if body.get("kind").and_then(Value::as_str) != Some("hashedrekord") {
        return Err(tlog("Rekor entry is not a hashedrekord".into()));
    }
    let spec = body
        .get("spec")
        .ok_or_else(|| tlog("body has no spec".into()))?;
    let hash = spec.pointer("/data/hash");
    let alg = hash
        .and_then(|h| h.get("algorithm"))
        .and_then(Value::as_str);
    let val = hash.and_then(|h| h.get("value")).and_then(Value::as_str);
    if alg != Some("sha256") || val != Some(hex::encode(Sha256::digest(&sig.payload)).as_str()) {
        return Err(tlog("Rekor entry hash does not match the payload".into()));
    }
    let body_sig = spec
        .pointer("/signature/content")
        .and_then(Value::as_str)
        .map(b64_decode);
    if !matches!(body_sig, Some(Ok(ref b)) if *b == sb) {
        return Err(tlog("Rekor entry signature does not match".into()));
    }
    let body_cert = spec
        .pointer("/signature/publicKey/content")
        .and_then(Value::as_str)
        .and_then(|s| b64_decode(s).ok())
        .and_then(|pem| String::from_utf8(pem).ok())
        .and_then(|pem| pem_to_der(&pem, "CERTIFICATE").ok());
    if body_cert.as_deref() != Some(&cert_der[..]) {
        return Err(tlog("Rekor entry certificate does not match".into()));
    }

    // (7) identity
    let (subject, issuer) = leaf_identity(&leaf)?;
    let id = |m: String| VerifyError::IdentityMismatch(m);
    if issuer != pol.issuer {
        return Err(id(format!("issuer `{issuer}` != policy `{}`", pol.issuer)));
    }
    if let Some(s) = &pol.subject {
        if &subject != s {
            return Err(id(format!("subject `{subject}` != policy `{s}`")));
        }
    }
    if let Some(r) = &pol.subject_regex {
        let re = regex::Regex::new(r).map_err(|e| VerifyError::Policy(e.to_string()))?;
        if !re.is_match(&subject) {
            return Err(id(format!("subject `{subject}` does not match /{r}/")));
        }
    }
    Ok((subject, issuer))
}
