//! Integration test for CONCURRENT inter-component calls: a P3 HTTP caller
//! issues four parallel linked calls into an async-lifted callee export
//! that suspends (sleeps ~300ms). With the concurrent trampoline the calls
//! interleave within one store (~300ms total); the classic blocking
//! trampoline would trap the caller outright ("cannot block a synchronous
//! task") or serialize to ~1200ms.

#![cfg(feature = "wasip3")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::{Context, Result};
use std::{collections::HashMap, time::Duration};

use wash_runtime::{
    host::HostApi,
    types::{Component, LocalResources, Workload, WorkloadStartRequest},
};

mod common;
use common::http_only_host_interfaces;

const CALLER_WASM: &[u8] = include_bytes!("wasm/inter_component_concurrent_caller.wasm");
const CALLEE_WASM: &[u8] = include_bytes!("wasm/inter_component_concurrent_callee.wasm");

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_linked_calls_interleave() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (addr, host) = common::start_host_with_p3("127.0.0.1:0").await?;

    let component = |name: &str, wasm: &'static [u8]| Component {
        name: name.to_string(),
        digest: None,
        bytes: bytes::Bytes::from_static(wasm),
        local_resources: LocalResources::default(),
        pool_size: 1,
        max_invocations: 100,
        ..Default::default()
    };

    host.workload_start(WorkloadStartRequest {
        workload_id: uuid::Uuid::new_v4().to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "concurrent".to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![
                component("caller", CALLER_WASM),
                component("callee", CALLEE_WASM),
            ],
            host_interfaces: http_only_host_interfaces("concurrent"),
            volumes: vec![],
        },
    })
    .await
    .context("failed to start concurrent workload")?;

    let response = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .header("HOST", "concurrent")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("request failed")?;
    assert_eq!(response.status(), 200);
    let body = response.text().await?;

    let elapsed_ms: u64 = body
        .split_once("elapsed_ms=")
        .and_then(|(_, rest)| rest.split(';').next())
        .and_then(|v| v.parse().ok())
        .with_context(|| format!("unexpected body: {body}"))?;
    assert!(
        body.contains("ok=true"),
        "all linked calls must return their sleep duration: {body}"
    );

    // Four 300ms calls: concurrent ≈300ms, sequential ≈1200ms.
    assert!(
        (280..700).contains(&elapsed_ms),
        "linked calls did not interleave: elapsed {elapsed_ms}ms (concurrent ≈300, sequential ≈1200)"
    );

    Ok(())
}
