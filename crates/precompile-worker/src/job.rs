//! Wire types shared with the operator's precompile queue producer/consumer.
//!
//! Kept JSON-serializable so the Go side can produce them without code-sharing.

use serde::{Deserialize, Serialize};

/// Work item the operator publishes when an Artifact needs a precompiled
/// variant for a particular target.
///
/// Idempotency: the worker keys its put into the object store by
/// `(digest_of_compiled_bytes, target, wasmtime_version)`. Duplicate jobs
/// produce duplicate puts at the same key — harmless.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecompileJob {
    /// Namespace and name of the source Artifact, used to scope the
    /// completion subject.
    pub artifact_namespace: String,
    pub artifact_name: String,
    /// OCI ref of the source `.wasm` to pull and compile.
    pub artifact_image: String,
    /// Optional pull-secret reference name in the Artifact's namespace. The
    /// operator resolves and inlines credentials before publishing.
    #[serde(default)]
    pub registry_username: Option<String>,
    #[serde(default)]
    pub registry_password: Option<String>,
    /// Target triple to produce the variant for.
    pub target: String,
    /// Wasmtime version the operator expects. Worker logs a warning on
    /// mismatch; compat hash equality is the real safety net.
    pub wasmtime_version: String,
}

/// Event the worker publishes when a job completes (successfully or not).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecompileCompletion {
    pub artifact_namespace: String,
    pub artifact_name: String,
    pub target: String,
    pub wasmtime_version: String,
    /// Result of the job. `Ok` carries the published variant metadata so the
    /// operator can write it directly into Artifact.status without consulting
    /// the object store.
    pub result: Result<PublishedVariant, String>,
}

/// Variant metadata as published by the worker. Mirrors the shape of
/// `runtime.wasmcloud.dev/v1alpha1.PrecompiledVariant` on the Go side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedVariant {
    pub target: String,
    pub wasmtime_version: String,
    pub compat_hash: String,
    pub artifact_url: String,
    pub digest: String,
}

/// Subject pattern for completion events. Operator subscribes to
/// `wasmcloud.precompile.completion.>` and uses the suffix to route updates
/// back to the right Artifact.
pub fn completion_subject(namespace: &str, name: &str) -> String {
    format!("wasmcloud.precompile.completion.{namespace}.{name}")
}
