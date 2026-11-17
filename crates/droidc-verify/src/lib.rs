//! Offline cosign signature verification (keyless and key-based) for droidc.
//!
//! See the crate README for the verification pipeline and trust-root refresh procedure.

mod crypto;
mod error;
mod keyless;
mod policy;
mod trusted_root;

use std::collections::BTreeMap;
use std::time::SystemTime;

pub use error::VerifyError;
pub use policy::{ImagePolicy, KeylessPolicy, Policy, GITHUB_ACTIONS_ISSUER, UBLUE_COSIGN_PUB};
pub use trusted_root::{TrustedRoot, EMBEDDED_TRUSTED_ROOT};

/// One cosign signature as stored in the `sha256-<hex>.sig` manifest: the layer blob (the
/// simple-signing payload) plus the layer annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureBlob {
    /// Raw layer blob bytes (cosign simple-signing JSON).
    pub payload: Vec<u8>,
    /// Layer annotations (`dev.cosignproject.cosign/signature`, `dev.sigstore.cosign/certificate`,
    /// `dev.sigstore.cosign/chain`, `dev.sigstore.cosign/bundle`).
    pub annotations: BTreeMap<String, String>,
}

/// Annotation holding the base64 ASN.1-DER signature.
pub const ANN_SIGNATURE: &str = "dev.cosignproject.cosign/signature";
/// Annotation holding the PEM Fulcio leaf certificate.
pub const ANN_CERTIFICATE: &str = "dev.sigstore.cosign/certificate";
/// Annotation holding the PEM chain (ignored; the chain comes from the trusted root).
pub const ANN_CHAIN: &str = "dev.sigstore.cosign/chain";
/// Annotation holding the Rekor bundle JSON.
pub const ANN_BUNDLE: &str = "dev.sigstore.cosign/bundle";

/// Successful verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// `"keyless"` or `"key"`.
    pub mode: String,
    /// Certificate SAN (keyless only).
    pub subject: Option<String>,
    /// Fulcio OIDC issuer (keyless only).
    pub issuer: Option<String>,
    /// The `match` string of the policy entry that applied.
    pub policy_match: String,
}

/// Verify that at least one of `sigs` is a valid signature over `manifest_digest` that satisfies
/// the policy entry for `repository`. Fully offline.
///
/// `now` is used to reject Rekor integrated times in the future; leaf certificate validity is
/// checked against the Rekor integrated time, not `now` (Fulcio certs live ~10 minutes).
pub fn verify(
    policy: &Policy,
    repository: &str,
    manifest_digest: &str,
    sigs: &[SignatureBlob],
    now: SystemTime,
) -> Result<Verified, VerifyError> {
    let entry = policy
        .find(repository)
        .ok_or_else(|| VerifyError::NoPolicyMatch(repository.to_string()))?;
    if sigs.is_empty() {
        return Err(VerifyError::NoSignatures);
    }
    let root = match &policy.trusted_root_path {
        Some(p) => {
            let s = std::fs::read_to_string(p)
                .map_err(|e| VerifyError::Policy(format!("trusted_root_path {p}: {e}")))?;
            TrustedRoot::from_json(&s)?
        }
        None => TrustedRoot::embedded(),
    };
    let mut best: Option<VerifyError> = None;
    for sig in sigs {
        let r = keyless::check_payload_digest(sig, manifest_digest).and_then(|()| {
            let mut last: Option<VerifyError> = None;
            if let Some(k) = &entry.keyless {
                match keyless::verify_keyless(&root, k, sig, now) {
                    Ok((subject, issuer)) => {
                        return Ok(Verified {
                            mode: "keyless".into(),
                            subject: Some(subject),
                            issuer: Some(issuer),
                            policy_match: entry.match_.clone(),
                        })
                    }
                    Err(e) => last = Some(e),
                }
            }
            if !entry.keys.is_empty() {
                match keyless::verify_keys(&entry.keys, sig) {
                    Ok(()) => {
                        return Ok(Verified {
                            mode: "key".into(),
                            subject: None,
                            issuer: None,
                            policy_match: entry.match_.clone(),
                        })
                    }
                    // Prefer the keyless diagnostics when the signature carries a certificate.
                    Err(e) => {
                        if last.is_none() {
                            last = Some(e)
                        }
                    }
                }
            }
            Err(last
                .unwrap_or_else(|| VerifyError::Invalid("policy entry has no verifiers".into())))
        });
        match r {
            Ok(v) => return Ok(v),
            Err(e) => {
                if best.as_ref().is_none_or(|b| e.rank() > b.rank()) {
                    best = Some(e)
                }
            }
        }
    }
    Err(best.unwrap_or(VerifyError::NoSignatures))
}
