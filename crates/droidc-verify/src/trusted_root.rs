//! Sigstore trusted root (`trusted_root.json`, protobuf-JSON form) parsing.

use serde::Deserialize;

use crate::crypto::{b64_decode, parse_rfc3339};
use crate::VerifyError;

/// The Sigstore public-good trusted root embedded at build time. See the crate README for how it
/// was obtained and how to refresh it.
pub const EMBEDDED_TRUSTED_ROOT: &str = include_str!("data/trusted_root.json");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRoot {
    #[serde(default)]
    tlogs: Vec<RawTlog>,
    #[serde(default)]
    certificate_authorities: Vec<RawCa>,
}
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawValidFor {
    start: Option<String>,
    end: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawKey {
    raw_bytes: String,
    #[serde(default)]
    key_details: String,
    #[serde(default)]
    valid_for: RawValidFor,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTlog {
    public_key: RawKey,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCa {
    cert_chain: RawChain,
    #[serde(default)]
    valid_for: RawValidFor,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChain {
    certificates: Vec<RawCert>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCert {
    raw_bytes: String,
}

/// Validity window in unix seconds (inclusive start, inclusive end; open ends are `None`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    start: Option<i64>,
    end: Option<i64>,
}
impl Window {
    fn parse(v: &RawValidFor) -> Result<Self, String> {
        Ok(Window {
            start: v.start.as_deref().map(parse_rfc3339).transpose()?,
            end: v.end.as_deref().map(parse_rfc3339).transpose()?,
        })
    }
    pub(crate) fn contains(&self, t: i64) -> bool {
        self.start.map_or(true, |s| t >= s) && self.end.map_or(true, |e| t <= e)
    }
}

pub(crate) struct TlogKey {
    /// SPKI DER.
    pub spki: Vec<u8>,
    pub details: String,
    pub valid: Window,
}
pub(crate) struct Ca {
    /// DER certificates (any order): pool for chain building.
    pub certs: Vec<Vec<u8>>,
    pub valid: Window,
}

/// Parsed trusted root.
pub struct TrustedRoot {
    pub(crate) tlogs: Vec<TlogKey>,
    pub(crate) cas: Vec<Ca>,
}

impl TrustedRoot {
    /// Parse a `trusted_root.json` document.
    pub fn from_json(s: &str) -> Result<Self, VerifyError> {
        let err = |m: String| VerifyError::Policy(format!("trusted root: {m}"));
        let raw: RawRoot = serde_json::from_str(s).map_err(|e| err(e.to_string()))?;
        let mut tlogs = Vec::new();
        for t in raw.tlogs {
            tlogs.push(TlogKey {
                spki: b64_decode(&t.public_key.raw_bytes).map_err(err)?,
                details: t.public_key.key_details.clone(),
                valid: Window::parse(&t.public_key.valid_for).map_err(err)?,
            });
        }
        let mut cas = Vec::new();
        for c in raw.certificate_authorities {
            let certs = c
                .cert_chain
                .certificates
                .iter()
                .map(|x| b64_decode(&x.raw_bytes))
                .collect::<Result<Vec<_>, _>>()
                .map_err(err)?;
            cas.push(Ca { certs, valid: Window::parse(&c.valid_for).map_err(err)? });
        }
        Ok(TrustedRoot { tlogs, cas })
    }

    /// The embedded Sigstore public-good root.
    pub fn embedded() -> Self {
        Self::from_json(EMBEDDED_TRUSTED_ROOT).expect("embedded trusted_root.json is valid")
    }
}
