//! Classifies failures as permanent (a retry gives the same result) or
//! retryable (network, auth, NATS, ...).

use std::fmt;

/// Exit code when at least one item failed and a retry may help.
pub const EXIT_RETRYABLE: i32 = 1;

/// Exit code when the run failed and a retry will not help: every failed item
/// failed permanently (invalid wasm, image not found, bad reference, bad
/// output URL), or the input itself was invalid. clap also exits with 2 on a
/// usage error, which is permanent too, so the meaning stays the same.
pub const EXIT_PERMANENT: i32 = 2;

/// Marker attached as context to errors that a retry cannot fix. Only the batch
/// path adds it, so the error text of the single-pair CLI stays the same.
#[derive(Debug, Clone, Copy)]
pub struct Permanent;

impl fmt::Display for Permanent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("permanent error")
    }
}

/// True when any layer of the error chain is marked [`Permanent`], or holds a
/// pull error that a retry cannot fix.
pub fn is_permanent(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Permanent>().is_some() || crate::pull::is_permanent_pull_error(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn marker_survives_more_context() {
        let err = Err::<(), _>(anyhow!("manifest unknown"))
            .context(Permanent)
            .context("failed to pull r/a:1")
            .unwrap_err();
        assert!(is_permanent(&err));
    }

    #[test]
    fn plain_errors_are_not_permanent() {
        let err = Err::<(), _>(anyhow!("connection refused"))
            .context("failed to pull r/a:1")
            .unwrap_err();
        assert!(!is_permanent(&err));
    }
}
