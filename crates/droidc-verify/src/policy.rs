//! Trust policy (`policy.json`) parsing and matching.

use serde::{Deserialize, Serialize};

use crate::VerifyError;

/// PEM of `ublue-os` / Bluefin `cosign.pub` (ECDSA P-256), from
/// <https://raw.githubusercontent.com/projectbluefin/bluefin-lts/main/cosign.pub>.
pub const UBLUE_COSIGN_PUB: &str = include_str!("data/bluefin-lts-cosign.pub");

/// Fulcio OIDC issuer used by GitHub Actions.
pub const GITHUB_ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Keyless (Fulcio + Rekor) identity constraint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeylessPolicy {
    /// Exact OIDC issuer (Fulcio extension 1.3.6.1.4.1.57264.1.1 / .1.8). Required.
    pub issuer: String,
    /// Exact certificate SAN (URI or email).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Regex (unanchored unless you anchor it) the SAN must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_regex: Option<String>,
}

/// One policy entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImagePolicy {
    /// Exact repository (`ghcr.io/org/name`) or prefix glob with a trailing `/*`.
    #[serde(rename = "match")]
    pub match_: String,
    /// Accept keyless signatures from this identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyless: Option<KeylessPolicy>,
    /// Accept signatures made by any of these PEM public keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

impl ImagePolicy {
    fn matches(&self, repo: &str) -> Option<usize> {
        if let Some(prefix) = self.match_.strip_suffix("/*") {
            let p = format!("{prefix}/");
            repo.starts_with(&p).then_some(p.len())
        } else {
            (self.match_ == repo).then_some(usize::MAX)
        }
    }
}

/// Parsed trust policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    /// Format version; must be 1.
    pub version: u32,
    /// What to do when no entry matches. Only `"reject"` is supported.
    pub default: String,
    /// Entries; the most specific match wins (exact beats glob, longer glob beats shorter, then
    /// first in file order).
    #[serde(default)]
    pub images: Vec<ImagePolicy>,
    /// Optional path to a replacement Sigstore `trusted_root.json` (overrides the embedded one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_root_path: Option<String>,
}

impl Policy {
    /// Parse and validate a policy document.
    pub fn from_json(s: &str) -> Result<Self, VerifyError> {
        let p: Policy = serde_json::from_str(s).map_err(|e| VerifyError::Policy(e.to_string()))?;
        if p.version != 1 {
            return Err(VerifyError::Policy(format!("unsupported version {}", p.version)));
        }
        if p.default != "reject" {
            return Err(VerifyError::Policy(format!("unsupported default `{}` (only \"reject\")", p.default)));
        }
        for e in &p.images {
            if e.keyless.is_none() && e.keys.is_empty() {
                return Err(VerifyError::Policy(format!("entry `{}` has neither keyless nor keys", e.match_)));
            }
            if let Some(k) = &e.keyless {
                if k.issuer.is_empty() {
                    return Err(VerifyError::Policy(format!("entry `{}`: keyless.issuer is required", e.match_)));
                }
                if k.subject.is_none() && k.subject_regex.is_none() {
                    return Err(VerifyError::Policy(format!(
                        "entry `{}`: keyless needs subject or subject_regex",
                        e.match_
                    )));
                }
                if let Some(r) = &k.subject_regex {
                    regex::Regex::new(r).map_err(|e2| {
                        VerifyError::Policy(format!("entry `{}`: bad subject_regex: {e2}", e.match_))
                    })?;
                }
            }
            for k in &e.keys {
                if !k.contains("-----BEGIN PUBLIC KEY-----") {
                    return Err(VerifyError::Policy(format!("entry `{}`: key is not a PEM PUBLIC KEY", e.match_)));
                }
            }
        }
        Ok(p)
    }

    /// Built-in policy: Bluefin LTS keyless, `ghcr.io/ublue-os/*` with the published cosign key,
    /// default reject.
    pub fn default_policy() -> Self {
        Policy {
            version: 1,
            default: "reject".into(),
            images: vec![
                ImagePolicy {
                    match_: "ghcr.io/projectbluefin/bluefin-lts".into(),
                    keyless: Some(KeylessPolicy {
                        issuer: GITHUB_ACTIONS_ISSUER.into(),
                        subject: None,
                        subject_regex: Some(r"^https://github\.com/projectbluefin/".into()),
                    }),
                    keys: vec![],
                },
                ImagePolicy {
                    match_: "ghcr.io/ublue-os/*".into(),
                    keyless: None,
                    keys: vec![UBLUE_COSIGN_PUB.to_string()],
                },
            ],
            trusted_root_path: None,
        }
    }

    /// Serialize to pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("policy serializes")
    }

    /// Most specific entry for `repository`.
    pub fn find(&self, repository: &str) -> Option<&ImagePolicy> {
        self.images
            .iter()
            .filter_map(|e| e.matches(repository).map(|spec| (spec, e)))
            // max_by_key returns the last max; reverse so the first in file order wins ties.
            .rev()
            .max_by_key(|(spec, _)| *spec)
            .map(|(_, e)| e)
    }
}
