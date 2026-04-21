//! Pluggable artifact storage for precompiled component bytes.
//!
//! An [`ArtifactStore`] resolves a URL (e.g. `nats://bucket/key`) to the raw bytes
//! of a precompiled component. Stores are registered per URL scheme in an
//! [`ArtifactStoreRegistry`] and dispatched by scheme at fetch time, so adding a
//! new backend is additive — implement the trait, register under a new scheme.
//!
//! The NATS JetStream Object Store backend lives in [`nats`].

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use bytes::Bytes;

#[cfg(feature = "artifact-store-nats")]
pub mod nats;

/// A store that resolves opaque references to precompiled component bytes.
#[async_trait]
pub trait ArtifactStore: Send + Sync + 'static {
    /// Fetch the bytes for the given reference. The reference format is
    /// backend-specific (typically a URL with a scheme matching the backend,
    /// e.g. `nats://bucket/object`).
    async fn fetch(&self, reference: &str) -> anyhow::Result<Bytes>;
}

/// A collection of [`ArtifactStore`] implementations keyed by URL scheme.
///
/// `fetch` dispatches a URL to the store registered under that URL's scheme.
/// Unknown schemes produce an error.
#[derive(Default, Clone)]
pub struct ArtifactStoreRegistry {
    by_scheme: Arc<HashMap<String, Arc<dyn ArtifactStore>>>,
}

impl ArtifactStoreRegistry {
    /// Returns an empty registry. Use [`ArtifactStoreRegistry::builder`] to construct one with stores.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Start building a registry.
    pub fn builder() -> ArtifactStoreRegistryBuilder {
        ArtifactStoreRegistryBuilder::default()
    }

    /// Fetch bytes for a URL, dispatching to the store registered for the URL's scheme.
    pub async fn fetch(&self, url: &str) -> anyhow::Result<Bytes> {
        let (scheme, _) = url
            .split_once("://")
            .with_context(|| format!("artifact url missing scheme: {url}"))?;
        let store = self.by_scheme.get(scheme).ok_or_else(|| {
            anyhow!("no artifact store registered for scheme '{scheme}' (url: {url})")
        })?;
        store.fetch(url).await
    }

    /// Returns true if a store is registered for the given scheme.
    pub fn has_scheme(&self, scheme: &str) -> bool {
        self.by_scheme.contains_key(scheme)
    }
}

/// Builder for [`ArtifactStoreRegistry`].
#[derive(Default)]
pub struct ArtifactStoreRegistryBuilder {
    by_scheme: HashMap<String, Arc<dyn ArtifactStore>>,
}

impl ArtifactStoreRegistryBuilder {
    /// Register a store for the given URL scheme. Replaces any previous registration.
    pub fn register(
        mut self,
        scheme: impl Into<String>,
        store: Arc<dyn ArtifactStore>,
    ) -> Self {
        self.by_scheme.insert(scheme.into(), store);
        self
    }

    /// Finalize the registry.
    pub fn build(self) -> ArtifactStoreRegistry {
        ArtifactStoreRegistry {
            by_scheme: Arc::new(self.by_scheme),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticStore(Bytes);

    #[async_trait]
    impl ArtifactStore for StaticStore {
        async fn fetch(&self, _reference: &str) -> anyhow::Result<Bytes> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn dispatches_by_scheme() {
        let registry = ArtifactStoreRegistry::builder()
            .register("demo", Arc::new(StaticStore(Bytes::from_static(b"hello"))))
            .build();

        let got = registry.fetch("demo://any/path").await.unwrap();
        assert_eq!(&got[..], b"hello");
    }

    #[tokio::test]
    async fn errors_on_unknown_scheme() {
        let registry = ArtifactStoreRegistry::empty();
        let err = registry
            .fetch("unknown://x")
            .await
            .expect_err("expected unknown-scheme error");
        assert!(err.to_string().contains("unknown"));
    }

    #[tokio::test]
    async fn errors_on_missing_scheme() {
        let registry = ArtifactStoreRegistry::empty();
        let err = registry
            .fetch("no-scheme-here")
            .await
            .expect_err("expected missing-scheme error");
        assert!(err.to_string().contains("missing scheme"));
    }
}
