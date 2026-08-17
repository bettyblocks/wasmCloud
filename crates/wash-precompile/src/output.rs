use anyhow::bail;
use anyhow::{Context, Result};
use async_nats::jetstream::object_store;
use std::env;
use url::Url;

pub async fn write(output: &Url, bytes: &[u8]) -> Result<()> {
    match output.scheme() {
        "file" => write_file(output, bytes),
        "nats" => write_nats(output, bytes).await,
        #[cfg(feature = "precompile-s3")]
        "s3" => write_s3(output, bytes).await,
        #[cfg(feature = "precompile-azblob")]
        "azblob" => write_azblob(output, bytes).await,
        other => anyhow::bail!("unsupported output scheme: {other}"),
    }
}

fn write_file(output: &Url, bytes: &[u8]) -> Result<()> {
    let path = output
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("invalid file:// URL: {output}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)?;
    Ok(())
}

async fn write_nats(output: &Url, mut bytes: &[u8]) -> Result<()> {
    let (bucket, key) = parse_bucket_and_key(output)?;

    let nats_url = env::var("NATS_URL").context("NATS_URL env var not set")?;
    let client = async_nats::connect(&nats_url)
        .await
        .with_context(|| format!("failed to connect to NATS at {nats_url}"))?;
    let jetstream = async_nats::jetstream::new(client);

    let store = match jetstream.get_object_store(&bucket).await {
        Ok(store) => store,
        Err(_) => jetstream
            .create_object_store(object_store::Config {
                bucket: bucket.clone(),
                ..Default::default()
            })
            .await
            .with_context(|| format!("failed to create object store '{bucket}'"))?,
    };

    store
        .put(key.as_str(), &mut bytes)
        .await
        .with_context(|| format!("failed to put '{key}' in '{bucket}'"))?;

    Ok(())
}

/// Split a `scheme://<bucket-or-container>/<key>` URL into its bucket/container
/// (host) and object key (path). Shared by the NATS, S3 and Azure Blob writers,
/// which differ only in how the resulting store is built and authenticated.
fn parse_bucket_and_key(url: &Url) -> Result<(String, String)> {
    let bucket = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{url} missing bucket/container"))?
        .to_string();
    let key = url.path().trim_start_matches('/').to_string();
    if key.is_empty() {
        bail!("{url} missing object key");
    }
    Ok((bucket, key))
}

/// Put `bytes` at `key` in `container` on any `object_store` backend. Shared by
/// `write_s3` and `write_azblob`, which differ only in how the store itself is
/// built (bucket vs. container name, AWS vs. Azure credentials).
#[cfg(any(feature = "precompile-s3", feature = "precompile-azblob"))]
async fn write_via_object_store(
    store: impl ::object_store::ObjectStore,
    container: &str,
    key: &str,
    bytes: &[u8],
) -> Result<()> {
    use ::object_store::ObjectStoreExt;
    use ::object_store::path::Path as ObjectPath;

    store
        .put(&ObjectPath::from(key), bytes.to_vec().into())
        .await
        .with_context(|| format!("failed to put '{key}' in '{container}'"))?;
    Ok(())
}

#[cfg(feature = "precompile-s3")]
async fn write_s3(output: &Url, bytes: &[u8]) -> Result<()> {
    use ::object_store::aws::AmazonS3Builder;

    let (bucket, key) = parse_bucket_and_key(output)?;

    let store = AmazonS3Builder::from_env()
        .with_bucket_name(&bucket)
        .build()
        .with_context(|| format!("failed to configure S3 client for bucket '{bucket}'"))?;

    write_via_object_store(store, &bucket, &key, bytes).await
}

#[cfg(feature = "precompile-azblob")]
async fn write_azblob(output: &Url, bytes: &[u8]) -> Result<()> {
    use ::object_store::azure::MicrosoftAzureBuilder;

    let (container, key) = parse_bucket_and_key(output)?;

    let store = MicrosoftAzureBuilder::from_env()
        .with_container_name(&container)
        .build()
        .with_context(|| format!("failed to configure Azure Blob client for container '{container}'"))?;

    write_via_object_store(store, &container, &key, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_bytes_to_file_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.cwasm");
        let url = Url::from_file_path(&path).unwrap();

        write(&url, b"hello").await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn unknown_scheme_errors() {
        let url = Url::parse("ftp://bucket/key").unwrap();
        let err = write(&url, b"x").await.unwrap_err();
        assert!(err.to_string().contains("unsupported output scheme"));
    }

    #[test]
    fn parses_url_into_bucket_and_key() {
        let url = Url::parse("nats://precompiled-artifacts/myapp/x86_64.cwasm").unwrap();
        let (bucket, key) = parse_bucket_and_key(&url).unwrap();
        assert_eq!(bucket, "precompiled-artifacts");
        assert_eq!(key, "myapp/x86_64.cwasm");
    }

    #[test]
    fn url_without_key_errors() {
        let url = Url::parse("nats://bucket/").unwrap();
        let err = parse_bucket_and_key(&url).unwrap_err();
        assert!(err.to_string().contains("missing object key"));
    }

    #[cfg(any(feature = "precompile-s3", feature = "precompile-azblob"))]
    #[tokio::test]
    async fn write_via_object_store_puts_bytes_at_key() {
        use ::object_store::ObjectStoreExt;
        use ::object_store::memory::InMemory;
        use ::object_store::path::Path as ObjectPath;

        let store = InMemory::new();

        write_via_object_store(store.clone(), "test-container", "some/key.cwasm", b"hello")
            .await
            .unwrap();

        let bytes = store
            .get(&ObjectPath::from("some/key.cwasm"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"hello");
    }
}
