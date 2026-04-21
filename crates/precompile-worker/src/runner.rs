//! Job consumption loop: pull from the JetStream work queue, compile, publish.

use anyhow::{Context, anyhow};
use async_nats::jetstream;
use futures::StreamExt as _;
use sha2::Digest as _;
use wash_runtime::wasmtime::Engine;

use crate::job::{PrecompileCompletion, PrecompileJob, PublishedVariant, completion_subject};

/// Configuration the worker reads at startup.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub nats_url: String,
    pub stream: String,
    pub consumer: String,
    pub bucket: String,
}

/// Connect to NATS and run the compile loop until the process is killed.
pub async fn run(config: WorkerConfig) -> anyhow::Result<()> {
    let client = async_nats::connect(&config.nats_url)
        .await
        .with_context(|| format!("connecting to NATS at {}", config.nats_url))?;
    tracing::info!(url = %config.nats_url, "connected to NATS");

    let js = jetstream::new(client.clone());

    let stream = js
        .get_stream(&config.stream)
        .await
        .map_err(|e| anyhow!("get_stream {}: {e}", config.stream))?;
    let consumer: jetstream::consumer::PullConsumer = stream
        .get_consumer(&config.consumer)
        .await
        .map_err(|e| anyhow!("get_consumer {}: {e}", config.consumer))?;

    let object_store = js
        .get_object_store(&config.bucket)
        .await
        .map_err(|e| anyhow!("get_object_store {}: {e}", config.bucket))?;

    let engine = wash_runtime::engine::Engine::builder()
        .build()
        .context("constructing wasmtime engine")?;
    let inner_engine = engine.inner().clone();
    let compat_hash =
        wash_runtime::component_loader::DefaultComponentLoader::compat_hash(&inner_engine);

    tracing::info!(
        target = wash_runtime::TARGET_TRIPLE,
        wasmtime_version = wash_runtime::WASMTIME_VERSION,
        compat_hash = %compat_hash,
        "worker ready"
    );

    let mut messages = consumer
        .messages()
        .await
        .map_err(|e| anyhow!("subscribe to consumer: {e}"))?;

    while let Some(message) = messages.next().await {
        let message = match message {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(err = %e, "transient consumer error; retrying");
                continue;
            }
        };

        let job: PrecompileJob = match serde_json::from_slice(&message.payload) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(err = %e, "discarding undecodable job");
                if let Err(ack_err) = message.ack().await {
                    tracing::warn!(err = %ack_err, "ack of undecodable job failed");
                }
                continue;
            }
        };

        let completion = match process_job(&inner_engine, &object_store, &config.bucket, &job).await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(err = %e, ns = %job.artifact_namespace, name = %job.artifact_name, "job failed");
                PrecompileCompletion {
                    artifact_namespace: job.artifact_namespace.clone(),
                    artifact_name: job.artifact_name.clone(),
                    target: job.target.clone(),
                    wasmtime_version: wash_runtime::WASMTIME_VERSION.to_string(),
                    result: Err(e.to_string()),
                }
            }
        };

        let subject = completion_subject(&job.artifact_namespace, &job.artifact_name);
        match serde_json::to_vec(&completion) {
            Ok(payload) => {
                if let Err(e) = client.publish(subject, payload.into()).await {
                    tracing::error!(err = %e, "publishing completion event");
                }
            }
            Err(e) => tracing::error!(err = %e, "serializing completion event"),
        }
        if let Err(e) = message.ack().await {
            tracing::warn!(err = %e, "ack failed");
        }
    }

    Ok(())
}

async fn process_job(
    engine: &Engine,
    object_store: &jetstream::object_store::ObjectStore,
    bucket_name: &str,
    job: &PrecompileJob,
) -> anyhow::Result<PrecompileCompletion> {
    if job.target != wash_runtime::TARGET_TRIPLE {
        anyhow::bail!(
            "worker target {} does not match job target {}",
            wash_runtime::TARGET_TRIPLE,
            job.target
        );
    }
    if job.wasmtime_version != wash_runtime::WASMTIME_VERSION {
        tracing::warn!(
            job_version = %job.wasmtime_version,
            worker_version = wash_runtime::WASMTIME_VERSION,
            "wasmtime version mismatch; relying on compat hash for safety"
        );
    }

    tracing::info!(
        namespace = %job.artifact_namespace,
        name = %job.artifact_name,
        image = %job.artifact_image,
        target = %job.target,
        "compiling component"
    );

    let mut oci_config = wash_runtime::oci::OciConfig::default();
    if let (Some(u), Some(p)) = (job.registry_username.as_ref(), job.registry_password.as_ref()) {
        oci_config.credentials = Some((u.clone(), p.clone()));
    }
    let (wasm_bytes, _source_digest) = wash_runtime::oci::pull_component(
        &job.artifact_image,
        oci_config,
        wash_runtime::oci::OciPullPolicy::IfNotPresent,
    )
    .await
    .with_context(|| format!("pulling {}", job.artifact_image))?;

    let started = std::time::Instant::now();
    let cwasm = engine
        .precompile_component(&wasm_bytes)
        .map_err(|e| anyhow!("precompile_component: {e}"))?;
    tracing::info!(
        bytes = cwasm.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "compile complete"
    );

    let mut hasher = sha2::Sha256::new();
    hasher.update(&cwasm);
    let digest = hex_encode(hasher.finalize().as_slice());

    let object_key = format!(
        "{digest}-{target}-{ver}.cwasm",
        target = job.target,
        ver = wash_runtime::WASMTIME_VERSION
    );
    let mut reader = std::io::Cursor::new(cwasm.clone());
    object_store
        .put(object_key.as_str(), &mut reader)
        .await
        .map_err(|e| anyhow!("put {object_key}: {e}"))?;

    let compat_hash =
        wash_runtime::component_loader::DefaultComponentLoader::compat_hash(engine);
    let url = format!("nats://{bucket_name}/{object_key}");

    Ok(PrecompileCompletion {
        artifact_namespace: job.artifact_namespace.clone(),
        artifact_name: job.artifact_name.clone(),
        target: job.target.clone(),
        wasmtime_version: wash_runtime::WASMTIME_VERSION.to_string(),
        result: Ok(PublishedVariant {
            target: job.target.clone(),
            wasmtime_version: wash_runtime::WASMTIME_VERSION.to_string(),
            compat_hash,
            artifact_url: url,
            digest,
        }),
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
