//! Content-addressed output keys.
//!
//! The operator can key a `.cwasm` by the sha256 of the `.wasm` (the OCI
//! layer digest) instead of by Artifact name and image tag:
//!
//! ```text
//! <base>/sha256/<hex>/<target>-<wasmtime>-<engine profile>.cwasm
//! ```
//!
//! Many Artifacts can then share one object, and the operator skips the Job
//! when the object exists. A wrong write would therefore reach every
//! Artifact with the same bytes. Before anything is written, [`check`]
//! makes sure that
//!
//! - the pulled bytes hash to the digest in the key, and
//! - the engine profile in the key is the profile of this worker.
//!
//! Legacy keys (no `sha256/<hex>/` segment) are not checked.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use url::Url;

/// The parts of a content-addressed key that the worker checks.
#[derive(Debug, PartialEq, Eq)]
pub struct DigestKey {
    /// Lower-case hex sha256 of the `.wasm`.
    pub sha256: String,
    /// The last `-`-separated part of the file name, without `.cwasm`.
    pub engine_profile: String,
}

/// Lower-case hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parses a content-addressed key out of `url`. Returns `Ok(None)` for a
/// legacy key, and an error for a key that has a `sha256/` segment but is
/// malformed: such a key was meant to be content-addressed, so it must not
/// silently skip the checks.
pub fn parse(url: &Url) -> Result<Option<DigestKey>> {
    let segs: Vec<&str> = url.path().trim_start_matches('/').split('/').collect();
    let Some(pos) = segs.iter().position(|s| *s == "sha256") else {
        return Ok(None);
    };
    let (Some(hex), Some(file), None) = (segs.get(pos + 1), segs.get(pos + 2), segs.get(pos + 3))
    else {
        bail!("content-addressed key must be sha256/<hex>/<file>.cwasm: {url}");
    };
    if !is_sha256_hex(hex) {
        bail!("content-addressed key has an invalid sha256 digest {hex:?}: {url}");
    }
    let Some(stem) = file.strip_suffix(".cwasm") else {
        bail!("content-addressed key must end in .cwasm: {url}");
    };
    let profile = stem.rsplit('-').next().unwrap_or_default();
    if profile.is_empty() || profile == stem {
        bail!("content-addressed key has no engine profile: {url}");
    }
    Ok(Some(DigestKey {
        sha256: (*hex).to_string(),
        engine_profile: profile.to_string(),
    }))
}

/// Checks a content-addressed `output` against the pulled `wasm` and this
/// worker's `engine_profile`. Legacy keys pass unchecked.
pub fn check(output: &Url, wasm: &[u8], engine_profile: &str) -> Result<()> {
    let Some(key) = parse(output)? else {
        return Ok(());
    };
    if key.engine_profile != engine_profile {
        bail!(
            "output key names engine profile {:?}, but this worker has profile {engine_profile:?}; \
             set the operator's -precompile-engine-profile to the value of \
             `wash-precompile --print-engine-profile`",
            key.engine_profile
        );
    }
    let got = sha256_hex(wasm);
    if got != key.sha256 {
        bail!(
            "pulled component has sha256:{got}, but the output key names sha256:{}",
            key.sha256
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const PROFILE: &str = "0123456789abcdef";

    /// Same format as the operator's `DigestKeyURL`.
    fn key_url(sha: &str, profile: &str) -> Url {
        Url::parse(&format!(
            "nats://precompiled-artifacts/sha256/{sha}/x86_64-unknown-linux-gnu-48-{profile}.cwasm"
        ))
        .unwrap()
    }

    #[test]
    fn hashes_like_sha256sum() {
        assert_eq!(sha256_hex(b""), EMPTY);
    }

    #[test]
    fn parses_operator_key() {
        assert_eq!(
            parse(&key_url(EMPTY, PROFILE)).unwrap(),
            Some(DigestKey {
                sha256: EMPTY.into(),
                engine_profile: PROFILE.into()
            })
        );
    }

    #[test]
    fn accepts_matching_digest_and_profile() {
        check(&key_url(EMPTY, PROFILE), b"", PROFILE).unwrap();
    }

    #[test]
    fn rejects_other_bytes() {
        let err = check(&key_url(EMPTY, PROFILE), b"not empty", PROFILE).unwrap_err();
        assert!(
            err.to_string().contains("output key names sha256:"),
            "{err}"
        );
    }

    #[test]
    fn rejects_other_engine_profile() {
        let err = check(&key_url(EMPTY, PROFILE), b"", "fedcba9876543210").unwrap_err();
        assert!(err.to_string().contains("engine profile"), "{err}");
    }

    #[test]
    fn legacy_keys_are_not_checked() {
        let url = Url::parse("nats://b/myapp/reg_app_n0001_g1/x86_64-27.0.0.cwasm").unwrap();
        assert_eq!(parse(&url).unwrap(), None);
        check(&url, b"anything", PROFILE).unwrap();
    }

    #[test]
    fn file_urls_are_checked_too() {
        let url = Url::parse(&format!(
            "file:///tmp/out/sha256/{EMPTY}/t-48-{PROFILE}.cwasm"
        ))
        .unwrap();
        assert!(check(&url, b"x", PROFILE).is_err());
        check(&url, b"", PROFILE).unwrap();
    }

    #[test]
    fn malformed_content_addressed_keys_are_errors() {
        for bad in [
            "nats://b/sha256/ABC/t-48-p.cwasm".to_string(),
            format!("nats://b/sha256/{}/t-48-p.cwasm", EMPTY.to_uppercase()),
            format!("nats://b/sha256/{EMPTY}"),
            format!("nats://b/sha256/{EMPTY}/t-48-p.wasm"),
            format!("nats://b/sha256/{EMPTY}/nodash.cwasm"),
            format!("nats://b/sha256/{EMPTY}/t-48-.cwasm"),
            format!("nats://b/sha256/{EMPTY}/extra/t-48-p.cwasm"),
        ] {
            let url = Url::parse(&bad).unwrap();
            assert!(parse(&url).is_err(), "{bad} must be rejected");
            assert!(check(&url, b"", "p").is_err(), "{bad} must fail the check");
        }
    }
}
