//! Loads a wasmtime [`Component`] from either a precompiled variant or raw bytes.
//!
//! The [`ComponentLoader`] trait abstracts the step that turns component metadata
//! plus source material into a runnable [`Component`]. The default implementation
//! ([`DefaultComponentLoader`]) prefers precompiled variants from an
//! [`ArtifactStoreRegistry`](crate::artifact_store::ArtifactStoreRegistry) when one
//! matches the host's `(target, wasmtime_version, compat_hash)`, and falls back to
//! invoking Cranelift on `fallback_bytes` otherwise.
//!
//! The variant-matching logic is intentionally conservative: if any of the three
//! cache-key dimensions disagree, the variant is skipped rather than loaded. The
//! `unsafe` call to [`Component::deserialize`] is only reachable after all three
//! match.

use std::sync::OnceLock;
use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::artifact_store::ArtifactStoreRegistry;

/// A precompiled `.cwasm` variant published for a specific host configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrecompiledVariant {
    /// Target triple this variant was compiled for, e.g. `x86_64-linux-gnu`.
    pub target: String,
    /// Exact wasmtime crate version used to produce this variant.
    pub wasmtime_version: String,
    /// Stable hex string derived from [`Engine::precompile_compatibility_hash`].
    pub compat_hash: String,
    /// URL that an [`ArtifactStore`](crate::artifact_store::ArtifactStore) can fetch.
    pub url: String,
    /// sha256 digest of the precompiled bytes, for integrity checking.
    pub digest: String,
}

/// Everything a loader needs to produce a [`Component`] for a single source.
#[derive(Debug, Clone)]
pub struct ComponentSource {
    /// Human-readable name for logging and error messages.
    pub name: String,
    /// OCI image reference the raw `.wasm` came from, or an empty string if unknown.
    pub oci_image: String,
    /// Optional digest of the source `.wasm` (used for cache keys).
    pub digest: Option<String>,
    /// Precompiled variants published for this source. Ordered by preference.
    pub precompiled: Vec<PrecompiledVariant>,
    /// Raw `.wasm` bytes, used when no precompiled variant matches.
    ///
    /// May be `None` in Strict mode where the operator guarantees a match; the
    /// loader errors if this is `None` and no variant resolved.
    pub fallback_bytes: Option<Bytes>,
}

/// Produces a [`Component`] from a [`ComponentSource`].
#[async_trait]
pub trait ComponentLoader: Send + Sync {
    /// Load the component described by `source`, using `engine` as the wasmtime
    /// engine to deserialize or compile against.
    async fn load(
        &self,
        source: &ComponentSource,
        engine: &Engine,
    ) -> anyhow::Result<Component>;
}

/// Metric handles for [`DefaultComponentLoader`], initialized lazily on first
/// use. Metrics are emitted via the global OpenTelemetry meter, the same
/// mechanism used by [`crate::observability::Meters`]. When no OTel exporter
/// is configured these are cheap no-ops.
struct LoaderMetrics {
    loads: Counter<u64>,
    fetch_duration: Histogram<f64>,
    compile_duration: Histogram<f64>,
}

fn loader_metrics() -> &'static LoaderMetrics {
    static METRICS: OnceLock<LoaderMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = opentelemetry::global::meter("wash-runtime");
        LoaderMetrics {
            loads: meter
                .u64_counter("component_loader.loads")
                .with_description(
                    "Component loads, labelled by source (precompiled | fallback_compile | error)",
                )
                .build(),
            fetch_duration: meter
                .f64_histogram("component_loader.fetch_duration_seconds")
                .with_description("Wall-clock duration fetching precompiled bytes from the store")
                .build(),
            compile_duration: meter
                .f64_histogram("component_loader.compile_duration_seconds")
                .with_description("Wall-clock duration of inline Cranelift compilation")
                .build(),
        }
    })
}

/// Default [`ComponentLoader`]: tries precompiled variants first, then compiles inline.
///
/// A variant is used only when all three of `target`, `wasmtime_version`, and
/// `compat_hash` match the loader's expected values. `compat_hash` is computed
/// lazily per call from [`Engine::precompile_compatibility_hash`].
pub struct DefaultComponentLoader {
    stores: ArtifactStoreRegistry,
    target: String,
    wasmtime_version: String,
}

impl DefaultComponentLoader {
    /// Construct a loader with the given store registry and host identity.
    ///
    /// `target` is the target triple this host runs (e.g. `x86_64-linux-gnu`).
    /// `wasmtime_version` is the exact version string used to build this binary.
    pub fn new(
        stores: ArtifactStoreRegistry,
        target: impl Into<String>,
        wasmtime_version: impl Into<String>,
    ) -> Self {
        Self {
            stores,
            target: target.into(),
            wasmtime_version: wasmtime_version.into(),
        }
    }

    /// Returns the target triple this loader was configured with.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Returns the wasmtime version this loader was configured with.
    pub fn wasmtime_version(&self) -> &str {
        &self.wasmtime_version
    }

    /// Compute a stable hex string from the engine's precompile compatibility hash.
    ///
    /// Uses sha256 as a deterministic [`std::hash::Hasher`] adapter so worker and
    /// host produce the same string for the same engine configuration, independent
    /// of Rust stdlib hash-algorithm choices.
    pub fn compat_hash(engine: &Engine) -> String {
        use std::hash::Hash as _;
        let mut hasher = Sha256Hasher::new();
        engine.precompile_compatibility_hash().hash(&mut hasher);
        hasher.finalize_hex()
    }
}

/// Adapter wrapping [`sha2::Sha256`] as a deterministic [`std::hash::Hasher`].
struct Sha256Hasher(sha2::Sha256);

impl Sha256Hasher {
    fn new() -> Self {
        use sha2::Digest as _;
        Self(sha2::Sha256::new())
    }

    fn finalize_hex(self) -> String {
        use sha2::Digest as _;
        let out = self.0.finalize();
        let mut s = String::with_capacity(out.len() * 2);
        for b in out.iter() {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

impl std::hash::Hasher for Sha256Hasher {
    fn write(&mut self, bytes: &[u8]) {
        use sha2::Digest as _;
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        // Required by the trait but not meaningful for sha256. The real
        // digest is obtained via finalize_hex. Callers should not rely on this.
        0
    }
}

#[async_trait]
impl ComponentLoader for DefaultComponentLoader {
    async fn load(
        &self,
        source: &ComponentSource,
        engine: &Engine,
    ) -> anyhow::Result<Component> {
        let metrics = loader_metrics();
        let host_compat_hash = Self::compat_hash(engine);

        for variant in &source.precompiled {
            if variant.target != self.target
                || variant.wasmtime_version != self.wasmtime_version
                || variant.compat_hash != host_compat_hash
            {
                continue;
            }

            tracing::debug!(
                component = %source.name,
                url = %variant.url,
                target = %variant.target,
                "fetching precompiled variant"
            );

            let started = Instant::now();
            let bytes = self
                .stores
                .fetch(&variant.url)
                .await
                .with_context(|| format!("fetching precompiled variant {}", variant.url))?;
            metrics
                .fetch_duration
                .record(started.elapsed().as_secs_f64(), &[]);

            let component = deserialize_precompiled(engine, &bytes, &variant.url)?;

            metrics.loads.add(
                1,
                &[KeyValue::new("source", "precompiled")],
            );
            tracing::info!(
                component = %source.name,
                url = %variant.url,
                fetch_ms = started.elapsed().as_millis() as u64,
                "loaded precompiled component"
            );
            return Ok(component);
        }

        let bytes = match source.fallback_bytes.as_ref() {
            Some(b) => b,
            None => {
                metrics
                    .loads
                    .add(1, &[KeyValue::new("source", "error")]);
                anyhow::bail!(
                    "no precompiled variant matched for component '{}' and no fallback bytes provided",
                    source.name
                );
            }
        };

        tracing::info!(
            component = %source.name,
            "no precompiled variant matched; compiling inline"
        );
        let started = Instant::now();
        let result = Component::new(engine, bytes.as_ref())
            .map_err(anyhow::Error::from)
            .with_context(|| format!("compiling component '{}'", source.name));
        metrics
            .compile_duration
            .record(started.elapsed().as_secs_f64(), &[]);
        match &result {
            Ok(_) => metrics
                .loads
                .add(1, &[KeyValue::new("source", "fallback_compile")]),
            Err(_) => metrics
                .loads
                .add(1, &[KeyValue::new("source", "error")]),
        }
        result
    }
}

/// Deserializes precompiled bytes into a wasmtime [`Component`].
///
/// The caller must have already verified that `bytes` were produced by a worker
/// built against the same wasmtime version and with the same engine
/// compatibility hash, and that the bytes were published via a channel
/// (Artifact CRD) whose admission is trusted.
#[allow(unsafe_code)]
fn deserialize_precompiled(
    engine: &Engine,
    bytes: &[u8],
    url: &str,
) -> anyhow::Result<Component> {
    // SAFETY: the caller matched `(target, wasmtime_version, compat_hash)`
    // against the variant before reaching this point, so the bytes are
    // compatible with `engine`. The variant URL comes from an
    // admission-gated Artifact CR, so the producer is trusted.
    unsafe { Component::deserialize(engine, bytes) }
        .map_err(anyhow::Error::from)
        .with_context(|| format!("deserializing precompiled variant {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_store::ArtifactStore;
    use std::sync::{Arc, Mutex};

    /// Minimal valid component bytes, produced via `wat` so wasmtime accepts them.
    fn minimal_component_wasm() -> Bytes {
        let bytes = wat::parse_str("(component)").expect("parse minimal component wat");
        Bytes::from(bytes)
    }

    struct RecordingStore {
        bytes: Bytes,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ArtifactStore for RecordingStore {
        async fn fetch(&self, reference: &str) -> anyhow::Result<Bytes> {
            self.calls.lock().unwrap().push(reference.to_string());
            Ok(self.bytes.clone())
        }
    }

    #[tokio::test]
    async fn falls_back_when_no_variants() {
        let engine = Engine::default();
        let loader = DefaultComponentLoader::new(
            ArtifactStoreRegistry::empty(),
            "x86_64-linux-gnu",
            "43.0.1",
        );
        let source = ComponentSource {
            name: "test".into(),
            oci_image: String::new(),
            digest: None,
            precompiled: vec![],
            fallback_bytes: Some(minimal_component_wasm()),
        };

        loader
            .load(&source, &engine)
            .await
            .expect("fallback compile should succeed for minimal component");
    }

    #[tokio::test]
    async fn errors_when_no_variant_and_no_fallback() {
        let engine = Engine::default();
        let loader = DefaultComponentLoader::new(
            ArtifactStoreRegistry::empty(),
            "x86_64-linux-gnu",
            "43.0.1",
        );
        let source = ComponentSource {
            name: "test".into(),
            oci_image: String::new(),
            digest: None,
            precompiled: vec![],
            fallback_bytes: None,
        };

        let err = loader
            .load(&source, &engine)
            .await
            .err()
            .expect("expected missing-fallback error");
        assert!(
            err.to_string().contains("no precompiled variant matched"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn skips_variants_with_mismatched_target() {
        let engine = Engine::default();
        let compat_hash = DefaultComponentLoader::compat_hash(&engine);
        let store = Arc::new(RecordingStore {
            bytes: Bytes::from_static(b"should-not-be-used"),
            calls: Mutex::new(Vec::new()),
        });
        let registry = ArtifactStoreRegistry::builder()
            .register("mock", store.clone() as Arc<dyn ArtifactStore>)
            .build();

        let loader = DefaultComponentLoader::new(
            registry,
            "x86_64-linux-gnu",
            "43.0.1",
        );
        let source = ComponentSource {
            name: "test".into(),
            oci_image: String::new(),
            digest: None,
            precompiled: vec![PrecompiledVariant {
                target: "aarch64-linux-gnu".into(), // mismatched
                wasmtime_version: "43.0.1".into(),
                compat_hash,
                url: "mock://bucket/obj".into(),
                digest: "n/a".into(),
            }],
            fallback_bytes: Some(minimal_component_wasm()),
        };

        loader
            .load(&source, &engine)
            .await
            .expect("should fall back when target mismatched");
        assert!(
            store.calls.lock().unwrap().is_empty(),
            "mismatched-target variant should be skipped without a fetch"
        );
    }

    #[tokio::test]
    async fn skips_variants_with_mismatched_wasmtime_version() {
        let engine = Engine::default();
        let compat_hash = DefaultComponentLoader::compat_hash(&engine);
        let store = Arc::new(RecordingStore {
            bytes: Bytes::from_static(b"should-not-be-used"),
            calls: Mutex::new(Vec::new()),
        });
        let registry = ArtifactStoreRegistry::builder()
            .register("mock", store.clone() as Arc<dyn ArtifactStore>)
            .build();

        let loader = DefaultComponentLoader::new(
            registry,
            "x86_64-linux-gnu",
            "43.0.1",
        );
        let source = ComponentSource {
            name: "test".into(),
            oci_image: String::new(),
            digest: None,
            precompiled: vec![PrecompiledVariant {
                target: "x86_64-linux-gnu".into(),
                wasmtime_version: "42.0.0".into(), // mismatched
                compat_hash,
                url: "mock://bucket/obj".into(),
                digest: "n/a".into(),
            }],
            fallback_bytes: Some(minimal_component_wasm()),
        };

        loader.load(&source, &engine).await.unwrap();
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_variants_with_mismatched_compat_hash() {
        let engine = Engine::default();
        let store = Arc::new(RecordingStore {
            bytes: Bytes::from_static(b"should-not-be-used"),
            calls: Mutex::new(Vec::new()),
        });
        let registry = ArtifactStoreRegistry::builder()
            .register("mock", store.clone() as Arc<dyn ArtifactStore>)
            .build();

        let loader = DefaultComponentLoader::new(
            registry,
            "x86_64-linux-gnu",
            "43.0.1",
        );
        let source = ComponentSource {
            name: "test".into(),
            oci_image: String::new(),
            digest: None,
            precompiled: vec![PrecompiledVariant {
                target: "x86_64-linux-gnu".into(),
                wasmtime_version: "43.0.1".into(),
                compat_hash: "deadbeef".into(), // mismatched
                url: "mock://bucket/obj".into(),
                digest: "n/a".into(),
            }],
            fallback_bytes: Some(minimal_component_wasm()),
        };

        loader.load(&source, &engine).await.unwrap();
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn loads_precompiled_variant_when_all_match() {
        let engine = Engine::default();
        let compat_hash = DefaultComponentLoader::compat_hash(&engine);

        // Produce real precompiled bytes via the engine itself so deserialize succeeds.
        let source_wasm = minimal_component_wasm();
        let precompiled_bytes = engine
            .precompile_component(&source_wasm)
            .expect("precompile minimal component");

        let store = Arc::new(RecordingStore {
            bytes: Bytes::from(precompiled_bytes),
            calls: Mutex::new(Vec::new()),
        });
        let registry = ArtifactStoreRegistry::builder()
            .register("mock", store.clone() as Arc<dyn ArtifactStore>)
            .build();

        let loader = DefaultComponentLoader::new(
            registry,
            "x86_64-linux-gnu",
            "43.0.1",
        );
        let source = ComponentSource {
            name: "test".into(),
            oci_image: String::new(),
            digest: None,
            precompiled: vec![PrecompiledVariant {
                target: "x86_64-linux-gnu".into(),
                wasmtime_version: "43.0.1".into(),
                compat_hash,
                url: "mock://bucket/obj".into(),
                digest: "n/a".into(),
            }],
            fallback_bytes: None, // no fallback — must use variant
        };

        loader
            .load(&source, &engine)
            .await
            .expect("should load precompiled variant");
        assert_eq!(store.calls.lock().unwrap().len(), 1);
    }
}
