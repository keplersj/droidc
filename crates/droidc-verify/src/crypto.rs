//! Small ECDSA / PEM helpers shared by the key and keyless paths.

use p256::ecdsa::signature::hazmat::PrehashVerifier;
use sha2::{Digest, Sha256, Sha384};

use base64::Engine;

pub(crate) fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

pub(crate) fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    // Some producers wrap lines; strip whitespace first.
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    b64()
        .decode(t.as_bytes())
        .map_err(|e| format!("bad base64: {e}"))
}

/// Which hash a signature uses.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Hash {
    Sha256,
    Sha384,
}

pub(crate) fn digest(h: Hash, data: &[u8]) -> Vec<u8> {
    match h {
        Hash::Sha256 => Sha256::digest(data).to_vec(),
        Hash::Sha384 => Sha384::digest(data).to_vec(),
    }
}

/// Decode the first PEM block with the given label to DER.
pub(crate) fn pem_to_der(pem: &str, label: &str) -> Result<Vec<u8>, String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let s = pem
        .find(&begin)
        .ok_or_else(|| format!("no `{begin}` block"))?
        + begin.len();
    let e = pem[s..].find(&end).ok_or_else(|| format!("no `{end}`"))? + s;
    b64_decode(&pem[s..e])
}

/// Verify an ASN.1-DER ECDSA signature over `msg` (hashed with `hash`) using an SPKI DER key on
/// P-256 or P-384. `hash` of `None` means the curve's natural hash (SHA-256 / SHA-384).
pub(crate) fn verify_ecdsa(
    spki_der: &[u8],
    hash: Option<Hash>,
    msg: &[u8],
    sig_der: &[u8],
) -> Result<(), String> {
    use p256::pkcs8::DecodePublicKey;
    if let Ok(vk) = p256::ecdsa::VerifyingKey::from_public_key_der(spki_der) {
        let prehash = digest(hash.unwrap_or(Hash::Sha256), msg);
        let sig = p256::ecdsa::Signature::from_der(sig_der)
            .map_err(|e| format!("bad DER signature: {e}"))?;
        return vk
            .verify_prehash(&prehash, &sig)
            .map_err(|_| "ECDSA P-256 signature mismatch".into());
    }
    if let Ok(vk) = p384::ecdsa::VerifyingKey::from_public_key_der(spki_der) {
        let prehash = digest(hash.unwrap_or(Hash::Sha384), msg);
        let sig = p384::ecdsa::Signature::from_der(sig_der)
            .map_err(|e| format!("bad DER signature: {e}"))?;
        return vk
            .verify_prehash(&prehash, &sig)
            .map_err(|_| "ECDSA P-384 signature mismatch".into());
    }
    Err("unsupported public key (only ECDSA P-256 / P-384)".into())
}

/// Parse an RFC 3339 UTC timestamp (`2026-10-12T03:45:06Z`, optional fraction) to unix seconds.
pub(crate) fn parse_rfc3339(s: &str) -> Result<i64, String> {
    let bad = || format!("bad timestamp `{s}`");
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return Err(bad());
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<i64>().map_err(|_| bad());
    let (y, m, d, hh, mm, ss) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    if !s.ends_with('Z') {
        return Err(bad());
    }
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Ok(days * 86400 + hh * 3600 + mm * 60 + ss)
}
