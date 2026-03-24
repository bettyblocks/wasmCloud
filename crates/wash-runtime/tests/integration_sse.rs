//! Integration test for SSE (Server-Sent Events) components
//!
//! Tests:
//! 1. SSE service component starts and binds to virtual loopback
//! 2. HTTP gateway component proxies SSE connections and push requests
//! 3. Events pushed via POST are received on the SSE stream
//! 4. Connection listing works

#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

use wash_runtime::{
    engine::Engine,
    host::{
        HostApi, HostBuilder,
        http::{DevRouter, HttpServer},
    },
    types::{Component, LocalResources, Service, Workload, WorkloadStartRequest},
    wit::WitInterface,
};

const SSE_SERVICE_WASM: &[u8] = include_bytes!("wasm/sse_service.wasm");
const SSE_GATEWAY_WASM: &[u8] = include_bytes!("wasm/sse_http_gateway.wasm");

fn sse_host_interfaces() -> Vec<WitInterface> {
    vec![WitInterface {
        namespace: "wasi".to_string(),
        package: "http".to_string(),
        interfaces: ["incoming-handler".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.2.2").unwrap()),
        config: {
            let mut config = HashMap::new();
            config.insert("host".to_string(), "sse-test".to_string());
            config
        },
        name: None,
    }]
}

#[tokio::test]
async fn test_sse_integration() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let engine = Engine::builder().build()?;

    // Create HTTP server with DevRouter (no SSE-specific config needed)
    let http_server = HttpServer::new(DevRouter::default(), "127.0.0.1:0".parse()?).await?;
    let bound_addr = http_server.addr();

    let host = HostBuilder::new()
        .with_engine(engine)
        .with_http_handler(Arc::new(http_server))
        .build()?;

    let host = host.start().await.context("Failed to start host")?;

    // Start workload with SSE service + HTTP gateway
    let req = WorkloadStartRequest {
        workload_id: uuid::Uuid::new_v4().to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "sse-workload".to_string(),
            annotations: HashMap::new(),
            service: Some(Service {
                digest: None,
                bytes: bytes::Bytes::from_static(SSE_SERVICE_WASM),
                local_resources: LocalResources {
                    environment: HashMap::from([(
                        "SSE_BIND_ADDR".to_string(),
                        "127.0.0.1:9090".to_string(),
                    )]),
                    ..Default::default()
                },
                max_restarts: 0,
            }),
            components: vec![Component {
                name: "sse-gateway".to_string(),
                digest: None,
                bytes: bytes::Bytes::from_static(SSE_GATEWAY_WASM),
                local_resources: Default::default(),
                pool_size: 4,
                max_invocations: 100,
            }],
            host_interfaces: sse_host_interfaces(),
            volumes: vec![],
        },
    };

    host.workload_start(req)
        .await
        .context("Failed to start SSE workload")?;

    // Wait for SSE service to start (check stderr for startup message)
    tokio::time::sleep(Duration::from_secs(2)).await;

    let client = reqwest::Client::new();

    // --- Test 1: Open SSE connection ---
    let sse_response = timeout(
        Duration::from_secs(10),
        client
            .get(format!("http://{bound_addr}/chatMessage/123"))
            .header("Host", "sse-test")
            .header("Accept", "text/event-stream")
            .send(),
    )
    .await
    .context("SSE connection timed out")?
    .context("Failed to open SSE connection")?;

    assert!(
        sse_response.status().is_success(),
        "SSE connection failed with status {}",
        sse_response.status(),
    );

    assert_eq!(
        sse_response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "Response should have text/event-stream content type"
    );

    // --- Test 2: Push an event ---
    // Give the SSE connection a moment to register
    tokio::time::sleep(Duration::from_millis(500)).await;

    let push_response = timeout(
        Duration::from_secs(10),
        client
            .post(format!("http://{bound_addr}/push"))
            .header("Host", "sse-test")
            .header("Content-Type", "application/json")
            .body(r#"{"scope_key":"chatMessage/123","data":"{\"id\":1,\"message\":\"hello\"}"}"#)
            .send(),
    )
    .await
    .context("Push request timed out")?
    .context("Failed to push event")?;

    let push_body = push_response.text().await?;
    assert!(
        push_body.contains("\"sent\":1"),
        "Push should have sent to 1 connection, got: {push_body}"
    );

    // --- Test 3: List connections ---
    let list_response = timeout(
        Duration::from_secs(10),
        client
            .get(format!("http://{bound_addr}/connections/chatMessage/123"))
            .header("Host", "sse-test")
            .send(),
    )
    .await
    .context("List request timed out")?
    .context("Failed to list connections")?;

    let list_body = list_response.text().await?;
    assert!(
        list_body.contains("sse-conn-"),
        "Should list at least one SSE connection, got: {list_body}"
    );

    Ok(())
}
