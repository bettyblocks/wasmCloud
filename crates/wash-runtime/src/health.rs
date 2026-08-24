//! Tracks whether a host's own NATS heartbeat loop is still making
//! progress, so an HTTP health check can catch a wedged host (process
//! alive, command/heartbeat loop dead) that a plain TCP-accept liveness
//! probe cannot see.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct HeartbeatHealth {
    last_success_unix_secs: AtomicU64,
    max_staleness: Duration,
}

impl HeartbeatHealth {
    /// `max_staleness` is how long since the last successful heartbeat
    /// publish before this host is considered unhealthy.
    pub fn new(max_staleness: Duration) -> Self {
        Self {
            last_success_unix_secs: AtomicU64::new(now_unix_secs()),
            max_staleness,
        }
    }

    /// Record a successful heartbeat publish.
    pub fn record_success(&self) {
        self.last_success_unix_secs
            .store(now_unix_secs(), Ordering::Relaxed);
    }

    /// Seconds since the last successful heartbeat publish.
    pub fn age_secs(&self) -> u64 {
        now_unix_secs().saturating_sub(self.last_success_unix_secs.load(Ordering::Relaxed))
    }

    /// Unix timestamp (seconds) of the last successful heartbeat publish.
    /// Exposed so callers (e.g. the `/healthz` route) can surface a raw,
    /// monotonically-increasing number a human can diff across requests to
    /// confirm the heartbeat loop is still making progress.
    pub fn last_success_unix_secs(&self) -> u64 {
        self.last_success_unix_secs.load(Ordering::Relaxed)
    }

    /// `true` when the last successful heartbeat is within `max_staleness`.
    pub fn is_healthy(&self) -> bool {
        self.age_secs() <= self.max_staleness.as_secs()
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
