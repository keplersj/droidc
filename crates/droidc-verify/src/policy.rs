//! Trust policy (`policy.json`) parsing and matching.

use serde::{Deserialize, Serialize};

use crate::VerifyError;

/// PEM of `ublue-os` / Bluefin `cosign.pub` (ECDSA P-256), from
/// <https://raw.githubusercontent.com/projectbluefin/bluefin-lts/main/cosign.pub>.
pub const UBLUE_COSIGN_PUB: &str = include_str!("data/bluefin-lts-cosign.pub");

/// PEM of the Pinniped OS image factory's `cosign.pub` (ECDSA P-256), from
/// <https://github.com/Pinniped-OS/factory/blob/main/keys/cosign.pub>. Pinniped OS publishes the
/// droidc-tuned images (`<image>:<release>-droidc`) that Selkie offers.
pub const PINNIPED_OS_COSIGN_PUB: &str = include_str!("data/pinniped-os-cosign.pub");

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
            return Err(VerifyError::Policy(format!(
                "unsupported version {}",
                p.version
            )));
        }
        if p.default != "reject" {
            return Err(VerifyError::Policy(format!(
                "unsupported default `{}` (only \"reject\")",
                p.default
            )));
        }
        for e in &p.images {
            if e.keyless.is_none() && e.keys.is_empty() {
                return Err(VerifyError::Policy(format!(
                    "entry `{}` has neither keyless nor keys",
                    e.match_
                )));
            }
            if let Some(k) = &e.keyless {
                if k.issuer.is_empty() {
                    return Err(VerifyError::Policy(format!(
                        "entry `{}`: keyless.issuer is required",
                        e.match_
                    )));
                }
                if k.subject.is_none() && k.subject_regex.is_none() {
                    return Err(VerifyError::Policy(format!(
                        "entry `{}`: keyless needs subject or subject_regex",
                        e.match_
                    )));
                }
                if let Some(r) = &k.subject_regex {
                    regex::Regex::new(r).map_err(|e2| {
                        VerifyError::Policy(format!(
                            "entry `{}`: bad subject_regex: {e2}",
                            e.match_
                        ))
                    })?;
                }
            }
            for k in &e.keys {
                if !k.contains("-----BEGIN PUBLIC KEY-----") {
                    return Err(VerifyError::Policy(format!(
                        "entry `{}`: key is not a PEM PUBLIC KEY",
                        e.match_
                    )));
                }
            }
        }
        Ok(p)
    }

    /// Built-in policy: Bluefin LTS keyless, `ghcr.io/ublue-os/*` and `ghcr.io/pinniped-os/*` with
    /// their published cosign keys, default reject.
    pub fn default_policy() -> Self {
        let mut p = Self::default_policy_v1();
        p.images.push(ImagePolicy {
            match_: "ghcr.io/pinniped-os/*".into(),
            keyless: None,
            keys: vec![PINNIPED_OS_COSIGN_PUB.to_string()],
        });
        p
    }

    /// Earlier built-in defaults, oldest first. A `policy.json` equal to one of these was written
    /// by droidc and never edited, so [`Policy::upgrade_default`] may replace it.
    fn earlier_defaults() -> [Self; 1] {
        [Self::default_policy_v1()]
    }

    /// If this is an unedited earlier built-in default, the current default; otherwise `None`.
    /// Edited policies are never touched.
    pub fn upgrade_default(&self) -> Option<Self> {
        Self::earlier_defaults()
            .iter()
            .any(|old| old == self)
            .then(Self::default_policy)
    }

    /// The first built-in default: Bluefin LTS keyless and `ghcr.io/ublue-os/*`.
    fn default_policy_v1() -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_trusts_pinniped_os_with_its_key() {
        let p = Policy::default_policy();
        for repo in [
            "ghcr.io/pinniped-os/monk",
            "ghcr.io/pinniped-os/harbor",
            "ghcr.io/pinniped-os/hooded",
        ] {
            let e = p.find(repo).expect(repo);
            assert_eq!(e.match_, "ghcr.io/pinniped-os/*");
            assert!(e.keyless.is_none());
            assert_eq!(e.keys, vec![PINNIPED_OS_COSIGN_PUB.to_string()]);
        }
        assert!(p.find("ghcr.io/pinniped-os-evil/monk").is_none());
        assert!(p.find("ghcr.io/pinniped-os").is_none());
    }

    #[test]
    fn default_policy_round_trips_and_validates() {
        let p = Policy::default_policy();
        assert_eq!(Policy::from_json(&p.to_json()).unwrap(), p);
    }

    #[test]
    fn pinniped_os_key_is_a_pem_public_key() {
        assert!(PINNIPED_OS_COSIGN_PUB.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert_ne!(PINNIPED_OS_COSIGN_PUB, UBLUE_COSIGN_PUB);
    }

    #[test]
    fn unedited_earlier_default_upgrades() {
        let old = Policy::from_json(&Policy::default_policy_v1().to_json()).unwrap();
        assert_eq!(old.upgrade_default(), Some(Policy::default_policy()));
    }

    #[test]
    fn current_default_and_edited_policies_are_left_alone() {
        assert_eq!(Policy::default_policy().upgrade_default(), None);
        let mut edited = Policy::default_policy_v1();
        edited.images.pop();
        assert_eq!(edited.upgrade_default(), None);
        let mut moved = Policy::default_policy_v1();
        moved.trusted_root_path = Some("/data/root.json".into());
        assert_eq!(moved.upgrade_default(), None);
    }
}
