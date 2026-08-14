use anyhow::{Context, Result, bail};
use std::env;
use tokio::io::AsyncReadExt;
use url::Url;

pub async fn fetch(output: &str) -> Result<Vec<u8>> {
    let url = Url::parse(output).with_context(|| format!("invalid precompiled URL: {output}"))?;
    match url.scheme() {
        "nats" => fetch_nats(&url).await,
        "file" => fetch_file(&url),
        #[cfg(feature = "s3")]
        "s3" => fetch_s3(&url).await,
        #[cfg(feature = "azblob")]
        "azblob" => fetch_azblob(&url).await,
        other => bail!("unsupported precompiled scheme: {other}"),
    }
}

fn fetch_file(url: &Url) -> Result<Vec<u8>> {
    let path = url
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("invalid file:// URL: {url}"))?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read precompiled bytes from {}", path.display()))?;
    Ok(bytes)
}

async fn fetch_nats(url: &Url) -> Result<Vec<u8>> {
    let (bucket, key) = parse_bucket_and_key(url)?;

    let nats_url = env::var("NATS_URL").context("NATS_URL env var not set")?;
    let client = async_nats::connect(&nats_url)
        .await
        .with_context(|| format!("failed to connect to NATS at {nats_url}"))?;
    let jetstream = async_nats::jetstream::new(client);
    let store = jetstream
        .get_object_store(&bucket)
        .await
        .with_context(|| format!("object store '{bucket}' not found"))?;

    let mut object = store
        .get(key.as_str())
        .await
        .with_context(|| format!("object '{key}' not found in '{bucket}'"))?;

    let mut bytes = Vec::new();
    object
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("failed to read object '{key}'"))?;

    Ok(bytes)
}

/// Split a `scheme://<bucket-or-container>/<key>` URL into its bucket/container
/// (host) and object key (path). Shared by the NATS, S3 and Azure Blob fetchers,
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

/// Get the bytes at `key` in `container` from any `object_store` backend. Shared
/// by `fetch_s3` and `fetch_azblob`, which differ only in how the store itself
/// is built (bucket vs. container name, AWS vs. Azure credentials).
#[cfg(any(feature = "s3", feature = "azblob"))]
async fn fetch_via_object_store(
    store: impl object_store::ObjectStore,
    container: &str,
    key: &str,
) -> Result<Vec<u8>> {
    use object_store::ObjectStoreExt;
    use object_store::path::Path as ObjectPath;

    let object = store
        .get(&ObjectPath::from(key))
        .await
        .with_context(|| format!("object '{key}' not found in '{container}'"))?;
    let bytes = object
        .bytes()
        .await
        .with_context(|| format!("failed to read object '{key}' from '{container}'"))?;
    Ok(bytes.to_vec())
}

#[cfg(feature = "s3")]
async fn fetch_s3(url: &Url) -> Result<Vec<u8>> {
    use object_store::aws::AmazonS3Builder;

    let (bucket, key) = parse_bucket_and_key(url)?;

    let store = AmazonS3Builder::from_env()
        .with_bucket_name(&bucket)
        .build()
        .with_context(|| format!("failed to configure S3 client for bucket '{bucket}'"))?;

    fetch_via_object_store(store, &bucket, &key).await
}

#[cfg(feature = "azblob")]
async fn fetch_azblob(url: &Url) -> Result<Vec<u8>> {
    use object_store::azure::MicrosoftAzureBuilder;

    let (container, key) = parse_bucket_and_key(url)?;

    let store = MicrosoftAzureBuilder::from_env()
        .with_container_name(&container)
        .build()
        .with_context(|| format!("failed to configure Azure Blob client for container '{container}'"))?;

    fetch_via_object_store(store, &container, &key).await
}

/// Download a precompiled component's `.cwasm` into the host's local cache dir and
/// return where it landed. The engine loads the component from that file, which lets
/// the OS drop it from RAM when memory is tight and re-read it from disk on demand —
/// much cheaper than keeping every component pinned in memory.
///
/// The filename is a hash of the URL, so if we've already downloaded this artifact we
/// just hand back the file we have instead of downloading it again (precompiled URLs
/// are digest-pinned: the same URL always means the same bytes). The download is
/// written to a temp file and then renamed into place, so a component is never loaded
/// from a half-written file — and because the host owns the file, nothing else can
/// change it while it's in use.
#[cfg(feature = "oci")]
pub async fn download_cwasm(url: &str, cache_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let key = cache_key_for_url(url);
    let path = cache_dir.join(format!("{key}.cwasm"));

    // Cache hit: the file already exists — skip the download. Refresh its mtime first so
    // the sweep's grace window treats a reused (possibly long-idle) file as fresh. Without
    // this, an orphan older than the grace period that's being started again could be
    // swept in the gap before the engine maps it — and the precompiled path, unlike the
    // OCI path, has no wasm to recompile from, so that start would just fail.
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        refresh_mtime(&path).await;
        tracing::debug!(url, path = %path.display(), "precompiled cache hit");
        return Ok(path);
    }

    let bytes = fetch(url).await?;

    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create compiled cache dir {}",
                cache_dir.display()
            )
        })?;

    // Write to a unique temp file in the same dir, then rename onto the final path.
    // Why indirect? A direct write isn't atomic — a crash or a concurrent start could
    // leave/expose a half-written `.cwasm` at the canonical path, which the cache-hit
    // check would then trust and `deserialize_file` would mmap as corrupt. rename is
    // atomic within a filesystem, so a reader sees either no file or the complete one.
    let tmp = cache_dir.join(format!(".{key}.{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("failed to write precompiled bytes to {}", tmp.display()))?;
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow::Error::from(e)).with_context(|| {
            format!(
                "failed to move precompiled file into place at {}",
                path.display()
            )
        });
    }

    tracing::debug!(
        url,
        path = %path.display(),
        size_bytes = bytes.len(),
        "cached precompiled component"
    );
    Ok(path)
}

/// Best-effort bump of `path`'s modification time to now, so the compiled-cache sweep's
/// grace window treats a just-reused file as fresh. Any failure is ignored: the worst
/// case is the pre-existing reuse-vs-sweep race, never a broken load.
#[cfg(feature = "oci")]
async fn refresh_mtime(path: &std::path::Path) {
    if let Ok(file) = tokio::fs::OpenOptions::new().write(true).open(path).await {
        let _ = file
            .into_std()
            .await
            .set_modified(std::time::SystemTime::now());
    }
}

/// Content-addressed filename stem for a precompiled URL: the hex SHA-256 of the URL.
/// Collision-free and stable, avoiding the lossy `/:@ -> -` collapsing that reusing
/// the OCI `sanitize_digest` on a URL would cause.
#[cfg(feature = "oci")]
fn cache_key_for_url(url: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let digest = Sha256::digest(url.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "oci")]
    #[tokio::test]
    async fn download_cwasm_caches_then_hits_without_refetch() {
        let src_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();

        let source = src_dir.path().join("artifact.cwasm");
        std::fs::write(&source, b"precompiled-bytes").unwrap();
        let url = format!("file://{}", source.display());

        // Miss: fetch the source and write a host-owned copy into the cache dir.
        let path = download_cwasm(&url, cache_dir.path()).await.unwrap();
        assert_eq!(path.parent(), Some(cache_dir.path()));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("cwasm"));
        assert_eq!(std::fs::read(&path).unwrap(), b"precompiled-bytes");

        // Delete the source so any re-fetch would fail — proving the second call is a
        // pure cache hit (stat, no download) that returns the same host-owned file.
        std::fs::remove_file(&source).unwrap();
        let hit = download_cwasm(&url, cache_dir.path()).await.unwrap();
        assert_eq!(hit, path);
        assert_eq!(std::fs::read(&hit).unwrap(), b"precompiled-bytes");
    }

    #[cfg(feature = "oci")]
    #[test]
    fn cache_key_for_url_is_stable_hex_sha256() {
        let a = cache_key_for_url("nats://bucket/key");
        let b = cache_key_for_url("nats://bucket/key");
        let c = cache_key_for_url("nats://bucket/other");
        assert_eq!(a, b, "same URL must hash the same");
        assert_ne!(a, c, "different URLs must differ");
        assert_eq!(a.len(), 64, "hex sha256 is 64 chars");
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
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

    #[tokio::test]
    async fn fetches_bytes_from_file_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cwasm");
        std::fs::write(&path, b"hello").unwrap();
        let url = format!("file://{}", path.display());

        let bytes = fetch(&url).await.unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn unknown_scheme_errors() {
        let err = fetch("ftp://bucket/key").await.unwrap_err();
        assert!(err.to_string().contains("unsupported precompiled scheme"));
    }

    #[cfg(any(feature = "s3", feature = "azblob"))]
    #[tokio::test]
    async fn fetch_via_object_store_gets_bytes_at_key() {
        use object_store::ObjectStoreExt;
        use object_store::memory::InMemory;
        use object_store::path::Path as ObjectPath;

        let store = InMemory::new();
        store
            .put(&ObjectPath::from("some/key.cwasm"), b"hello".as_ref().into())
            .await
            .unwrap();

        let bytes = fetch_via_object_store(store, "test-container", "some/key.cwasm")
            .await
            .unwrap();
        assert_eq!(bytes, b"hello");
    }
}
