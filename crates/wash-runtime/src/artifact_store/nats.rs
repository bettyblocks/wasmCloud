//! NATS JetStream Object Store backend for [`ArtifactStore`].
//!
//! URLs take the form `nats://{bucket}/{object_name}`. The bucket is the
//! JetStream object-store bucket name; the path is the object key within it.
//! Both must already exist in JetStream — this backend only reads.

use anyhow::Context;
use async_nats::jetstream;
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::AsyncReadExt;

use super::ArtifactStore;

/// Resolves `nats://bucket/object` URLs to bytes in a JetStream object store.
pub struct NatsArtifactStore {
    jetstream: jetstream::Context,
}

impl NatsArtifactStore {
    /// Construct a store from a connected NATS client.
    pub fn new(client: async_nats::Client) -> Self {
        Self {
            jetstream: jetstream::new(client),
        }
    }

    /// Construct a store from an existing JetStream context (useful when the
    /// caller already configured a domain, prefix, etc.).
    pub fn from_jetstream(jetstream: jetstream::Context) -> Self {
        Self { jetstream }
    }
}

#[async_trait]
impl ArtifactStore for NatsArtifactStore {
    async fn fetch(&self, reference: &str) -> anyhow::Result<Bytes> {
        let url = url::Url::parse(reference)
            .with_context(|| format!("parsing artifact url '{reference}'"))?;
        anyhow::ensure!(
            url.scheme() == "nats",
            "NatsArtifactStore expected a nats:// url, got scheme '{}'",
            url.scheme()
        );

        let bucket = url
            .host_str()
            .with_context(|| format!("artifact url '{reference}' has no bucket"))?;
        let object_key = url.path().trim_start_matches('/');
        anyhow::ensure!(
            !object_key.is_empty(),
            "artifact url '{reference}' has no object key"
        );

        let store = self
            .jetstream
            .get_object_store(bucket)
            .await
            .map_err(anyhow::Error::new)
            .with_context(|| format!("opening object store bucket '{bucket}'"))?;

        let mut object = store
            .get(object_key)
            .await
            .map_err(anyhow::Error::new)
            .with_context(|| format!("getting object '{object_key}' from '{bucket}'"))?;

        let mut buf = Vec::new();
        object
            .read_to_end(&mut buf)
            .await
            .with_context(|| format!("reading object '{object_key}' from '{bucket}'"))?;

        Ok(Bytes::from(buf))
    }
}
