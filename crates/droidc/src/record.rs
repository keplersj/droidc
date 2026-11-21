//! Deployment record (`deployment.json`), see docs/design-mvp.md.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignatureInfo {
    pub verified: bool,
    /// keyless | key | none
    pub mode: String,
    pub subject: Option<String>,
    pub issuer: Option<String>,
    pub policy_match: Option<String>,
}

impl SignatureInfo {
    pub fn none() -> Self {
        SignatureInfo {
            verified: false,
            mode: "none".into(),
            subject: None,
            issuer: None,
            policy_match: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Stats {
    pub download_bytes: u64,
    pub files: u64,
    pub bytes_on_disk: u64,
    pub hardlinks: u64,
    /// The strategy that was requested (`auto|hardlink|link2symlink|copy`).
    pub link_strategy: String,
    /// What actually happened (`auto` may end up as `hardlink`, `link2symlink` or a mix).
    #[serde(default)]
    pub link_strategy_used: String,
    pub unpack_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deployment {
    pub id: String,
    pub image_ref: String,
    pub manifest_digest: String,
    pub index_digest: Option<String>,
    pub config_digest: String,
    pub platform: String,
    pub created: Option<String>,
    pub deployed_at: String,
    pub session_label: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub signature: SignatureInfo,
    pub insecure: bool,
    pub stats: Stats,
    /// `"image"` once `fix-ownership` applied the image's uid/gid/mode/xattrs (needs root from then
    /// on); absent while everything belongs to the app uid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<String>,
}

pub const OWNERSHIP_IMAGE: &str = "image";

/// First 16 hex chars of the manifest digest.
pub fn deployment_id(manifest_digest: &str) -> String {
    let hexpart = manifest_digest
        .strip_prefix("sha256:")
        .unwrap_or(manifest_digest);
    hexpart.chars().take(16).collect()
}

/// RFC 3339 UTC timestamp for "now".
pub fn now_rfc3339() -> String {
    rfc3339(std::time::SystemTime::now())
}

pub fn rfc3339(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn id_is_16_hex() {
        assert_eq!(
            deployment_id(
                "sha256:d2a12ef344f80bb0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ),
            "d2a12ef344f80bb0"
        );
    }

    #[test]
    fn rfc3339_known() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00Z"
        );
    }
}
