use anyhow::bail;
use anyhow::{Context, Result};
use async_nats::jetstream::object_store;
use std::env;
use url::Url;

pub async fn write(output: &Url, bytes: &[u8]) -> Result<()> {
    match output.scheme() {
        "file" => write_file(output, bytes),
        "nats" => write_nats(output, bytes).await,
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
    let (bucket, key) = parse_nats_url(output)?;

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
                num_replicas: object_store_replicas()?,
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

/// Reads `OBJECT_STORE_REPLICAS`, defaulting to 1 when unset. Only applies
/// when the object store bucket doesn't already exist, since we attach to
/// (rather than reconfigure) an existing bucket's replica count.
fn object_store_replicas() -> Result<usize> {
    match env::var("OBJECT_STORE_REPLICAS") {
        Ok(val) => val
            .parse()
            .with_context(|| format!("invalid OBJECT_STORE_REPLICAS value: '{val}'")),
        Err(env::VarError::NotPresent) => Ok(1),
        Err(e) => Err(e).context("failed to read OBJECT_STORE_REPLICAS"),
    }
}

fn parse_nats_url(url: &Url) -> Result<(String, String)> {
    let bucket = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("nats:// URL missing bucket: {url}"))?
        .to_string();
    let key = url.path().trim_start_matches('/').to_string();
    if key.is_empty() {
        bail!("nats:// URL missing object key: {url}");
    }
    Ok((bucket, key))
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
        let url = Url::parse("s3://bucket/key").unwrap();
        let err = write(&url, b"x").await.unwrap_err();
        assert!(err.to_string().contains("unsupported output scheme"));
    }

    #[test]
    fn parses_nats_url_into_bucket_and_key() {
        let url = Url::parse("nats://precompiled-artifacts/myapp/x86_64.cwasm").unwrap();
        let (bucket, key) = parse_nats_url(&url).unwrap();
        assert_eq!(bucket, "precompiled-artifacts");
        assert_eq!(key, "myapp/x86_64.cwasm");
    }

    #[test]
    fn nats_url_without_key_errors() {
        let url = Url::parse("nats://bucket/").unwrap();
        let err = parse_nats_url(&url).unwrap_err();
        assert!(err.to_string().contains("missing object key"));
    }

    // OBJECT_STORE_REPLICAS is process-global env state; serialize access so
    // these tests don't race each other when run in parallel test threads.
    static REPLICAS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[allow(unsafe_code)]
    fn object_store_replicas_defaults_to_one_when_unset() {
        let _guard = REPLICAS_ENV_LOCK.lock().unwrap();
        unsafe { env::remove_var("OBJECT_STORE_REPLICAS") };
        assert_eq!(object_store_replicas().unwrap(), 1);
    }

    #[test]
    #[allow(unsafe_code)]
    fn object_store_replicas_reads_configured_value() {
        let _guard = REPLICAS_ENV_LOCK.lock().unwrap();
        unsafe { env::set_var("OBJECT_STORE_REPLICAS", "3") };
        let result = object_store_replicas();
        unsafe { env::remove_var("OBJECT_STORE_REPLICAS") };
        assert_eq!(result.unwrap(), 3);
    }

    #[test]
    #[allow(unsafe_code)]
    fn object_store_replicas_errors_on_invalid_value() {
        let _guard = REPLICAS_ENV_LOCK.lock().unwrap();
        unsafe { env::set_var("OBJECT_STORE_REPLICAS", "not-a-number") };
        let result = object_store_replicas();
        unsafe { env::remove_var("OBJECT_STORE_REPLICAS") };
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid OBJECT_STORE_REPLICAS")
        );
    }
}
