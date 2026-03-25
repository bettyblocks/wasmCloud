//! Integration test for SSE Relay + Mock OpenAI components
//!
//! Tests:
//! 1. Mock OpenAI service starts and binds to virtual loopback
//! 2. SSE Relay component proxies POST /chat to mock, relays SSE stream
//! 3. SSE events arrive in correct order: created → deltas → completed
//! 4. Delta payloads concatenate to expected text

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

const MOCK_OPENAI_WASM: &[u8] = include_bytes!("wasm/mock_openai.wasm");
const SSE_RELAY_WASM: &[u8] = include_bytes!("wasm/sse_relay.wasm");

fn relay_host_interfaces() -> Vec<WitInterface> {
    vec![WitInterface {
        namespace: "wasi".to_string(),
        package: "http".to_string(),
        interfaces: ["incoming-handler".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.2.2").unwrap()),
        config: {
            let mut config = HashMap::new();
            config.insert("host".to_string(), "sse-relay-test".to_string());
            config
        },
        name: None,
    }]
}

#[tokio::test]
async fn test_sse_relay_integration() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let engine = Engine::builder().build()?;

    let http_server = HttpServer::new(DevRouter::default(), "127.0.0.1:0".parse()?).await?;
    let bound_addr = http_server.addr();

    let host = HostBuilder::new()
        .with_engine(engine)
        .with_http_handler(Arc::new(http_server))
        .build()?;

    let host = host.start().await.context("Failed to start host")?;

    // Start workload: mock-openai as service + sse-relay as component
    let req = WorkloadStartRequest {
        workload_id: uuid::Uuid::new_v4().to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "sse-relay-workload".to_string(),
            annotations: HashMap::new(),
            service: Some(Service {
                digest: None,
                bytes: bytes::Bytes::from_static(MOCK_OPENAI_WASM),
                local_resources: LocalResources {
                    environment: HashMap::from([(
                        "MOCK_BIND_ADDR".to_string(),
                        "127.0.0.1:9091".to_string(),
                    )]),
                    ..Default::default()
                },
                max_restarts: 0,
            }),
            components: vec![Component {
                name: "sse-relay".to_string(),
                digest: None,
                bytes: bytes::Bytes::from_static(SSE_RELAY_WASM),
                local_resources: LocalResources {
                    environment: HashMap::from([(
                        "UPSTREAM_ADDR".to_string(),
                        "127.0.0.1:9091".to_string(),
                    )]),
                    ..Default::default()
                },
                pool_size: 4,
                max_invocations: 100,
            }],
            host_interfaces: relay_host_interfaces(),
            volumes: vec![],
        },
    };

    host.workload_start(req)
        .await
        .context("Failed to start relay workload")?;

    // Wait for mock service to start
    tokio::time::sleep(Duration::from_secs(2)).await;

    let client = reqwest::Client::new();

    // --- Test: POST /chat and verify SSE stream ---
    let response = timeout(
        Duration::from_secs(10),
        client
            .post(format!("http://{bound_addr}/chat"))
            .header("Host", "sse-relay-test")
            .header("Content-Type", "application/json")
            .body(r#"{"prompt":"Hello"}"#)
            .send(),
    )
    .await
    .context("Request timed out")?
    .context("Failed to send request")?;

    assert!(
        response.status().is_success(),
        "Request failed with status {}",
        response.status(),
    );

    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "Response should have text/event-stream content type"
    );

    // Read the full SSE body
    let body = timeout(Duration::from_secs(10), response.text())
        .await
        .context("Reading body timed out")?
        .context("Failed to read body")?;

    // Verify event types are present in order
    assert!(
        body.contains("event: response.created"),
        "Should contain response.created event, got: {body}"
    );
    assert!(
        body.contains("event: response.output_text.delta"),
        "Should contain response.output_text.delta event, got: {body}"
    );
    assert!(
        body.contains("event: response.completed"),
        "Should contain response.completed event, got: {body}"
    );

    // Verify ordering: created before deltas before completed
    let created_pos = body.find("event: response.created").unwrap();
    let delta_pos = body.find("event: response.output_text.delta").unwrap();
    let completed_pos = body.find("event: response.completed").unwrap();
    assert!(
        created_pos < delta_pos,
        "response.created should come before deltas"
    );
    assert!(
        delta_pos < completed_pos,
        "deltas should come before response.completed"
    );

    // Verify delta content contains expected chunks
    assert!(
        body.contains(r#""delta":"Hello"#),
        "Should contain 'Hello' delta, got: {body}"
    );
    assert!(
        body.contains(r#""delta":" from"#),
        "Should contain ' from' delta, got: {body}"
    );
    assert!(
        body.contains(r#""delta":" mock!"#),
        "Should contain ' mock!' delta, got: {body}"
    );

    // Verify completed event has full concatenated text
    assert!(
        body.contains("Hello from mock!"),
        "Completed event should contain full text 'Hello from mock!', got: {body}"
    );

    Ok(())
}
