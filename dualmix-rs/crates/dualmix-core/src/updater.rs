//! RP Soundboard-inspired update manifest parsing (`version.json`) and SHA-256 verification.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_UPDATE_FEED: &str =
    "https://raw.githubusercontent.com/gamebaiters/DualMix/main/version.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionManifest {
    pub product: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer(pub u32, pub u32, pub u32);

impl SemVer {
    pub fn parse(s: &str) -> Option<Self> {
        let clean = s.trim().trim_start_matches(['v', 'V']);
        let mut it = clean.split('.').map(|p| p.parse::<u32>().ok());
        Some(Self(it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
    }
}

pub fn verify_sha256_hex(bytes: &[u8], expected_hex: &str) -> bool {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex.eq_ignore_ascii_case(expected_hex.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_and_sha256_check() {
        let a = SemVer::parse("2.3.0").unwrap();
        let b = SemVer::parse("v2.4.0").unwrap();
        assert!(b > a);

        let manifest = r#"{
          "product": "DualMix",
          "version": "2.4.0",
          "url": "https://github.com/gamebaiters/dualmix/releases/download/v2.4.0/DualMix-2.4.0-win-x64.zip",
          "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        }"#;
        let parsed: VersionManifest = serde_json::from_str(manifest).unwrap();
        assert!(verify_sha256_hex(b"", &parsed.sha256));
        assert!(!verify_sha256_hex(b"tampered", &parsed.sha256));
    }
}
