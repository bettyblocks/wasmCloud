//! Integration tests for the P3 HTTP async-submit + streaming path
//! (`host/http_p3.rs`): the response is forwarded to the client as soon as
//! the guest produces it, the body streams live, and the store keeps being
//! driven afterwards while background guest work has in-flight host
//! activity.

#![cfg(feature = "wasip3")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::{Context, Result};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

use wash_runtime::host::HostApi;

mod common;
use common::{component_workload_request, default_counter_resources, http_only_host_interfaces};

const ASYNC_SUBMIT_WASM: &[u8] = include_bytes!("wasm/http_async_submit_p3.wasm");
const STREAMING_WASM: &[u8] = include_bytes!("wasm/http_streaming_p3.wasm");

/// Minimal HTTP test server: records (path, arrival time) per request and
/// responds 200 after `response_delay`. The delay keeps the guest's
/// outbound request in flight, which is what the driver's host-work
/// tracking observes.
async fn start_test_server(
    response_delay: Duration,
) -> (SocketAddr, Arc<Mutex<Vec<(String, Instant)>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits: Arc<Mutex<Vec<(String, Instant)>>> = Arc::default();
    let task_hits = hits.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let hits = task_hits.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            data.extend_from_slice(&buf[..n]);
                            if data.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let path = String::from_utf8_lossy(&data)
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                hits.lock().unwrap().push((path, Instant::now()));
                sleep(response_delay).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });

    (addr, hits)
}

/// The guest returns its response immediately while a spawned background
/// task performs two sequential outbound POSTs, the first of which is held
/// open by the test server for a second. The response must arrive long
/// before the background work finishes — and the background work must
/// still finish (the store outlives the response).
#[tokio::test(flavor = "multi_thread")]
async fn p3_async_submit_response_precedes_background_work() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (server_addr, hits) = start_test_server(Duration::from_millis(1000)).await;
    let (addr, host) = common::start_host_with_p3("127.0.0.1:0").await?;

    host.workload_start(component_workload_request(
        "async-submit",
        "async-submit",
        ASYNC_SUBMIT_WASM,
        default_counter_resources(),
        http_only_host_interfaces("submit"),
    ))
    .await
    .context("failed to start async-submit workload")?;

    let started = Instant::now();
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/submit?cb={server_addr}"))
        .header("HOST", "submit")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("submit request failed")?;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await?, "submitted");
    let response_at = Instant::now();

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "response must not wait for background work, took {:?}",
        started.elapsed()
    );

    // Background work: /done1 held open 1s by the server, then /done2.
    // /done2 arriving proves the store was driven well past the response
    // (and past the 500ms background grace window).
    timeout(Duration::from_secs(10), async {
        loop {
            if hits.lock().unwrap().iter().any(|(p, _)| p == "/done2") {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("background /done2 never arrived — store was dropped too early")?;

    let done2_at = hits
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "/done2")
        .map(|(_, t)| *t)
        .unwrap();
    assert!(
        done2_at.duration_since(response_at) >= Duration::from_millis(800),
        "background work should have finished well after the response"
    );

    Ok(())
}

/// The guest writes three body chunks 200ms apart. With live streaming the
/// first chunk arrives almost immediately; a buffering implementation
/// would deliver everything only after ~400ms.
#[tokio::test(flavor = "multi_thread")]
async fn p3_response_body_streams_live() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (addr, host) = common::start_host_with_p3("127.0.0.1:0").await?;
    host.workload_start(component_workload_request(
        "streaming",
        "streaming",
        STREAMING_WASM,
        default_counter_resources(),
        http_only_host_interfaces("streaming"),
    ))
    .await
    .context("failed to start streaming workload")?;

    let started = Instant::now();
    let mut response = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .header("HOST", "streaming")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("streaming request failed")?;
    assert_eq!(response.status(), 200);

    let mut chunks: Vec<(bytes::Bytes, Instant)> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        chunks.push((chunk, Instant::now()));
    }

    let body: Vec<u8> = chunks.iter().flat_map(|(c, _)| c.iter().copied()).collect();
    assert_eq!(
        String::from_utf8_lossy(&body),
        "chunk-0;chunk-1;chunk-2;",
        "chunk payloads must arrive complete and in order"
    );

    let first_at = chunks.first().unwrap().1;
    let last_at = chunks.last().unwrap().1;
    assert!(
        first_at.duration_since(started) < Duration::from_millis(250),
        "first chunk must arrive before the guest finished writing (live streaming), took {:?}",
        first_at.duration_since(started)
    );
    assert!(
        last_at.duration_since(first_at) >= Duration::from_millis(300),
        "chunks should be spread over the guest's write schedule, span was {:?}",
        last_at.duration_since(first_at)
    );

    Ok(())
}

/// Dropping the response mid-stream (client disconnect) must not wedge the
/// host: the driver reaps the invocation and subsequent requests work.
#[tokio::test(flavor = "multi_thread")]
async fn p3_client_disconnect_mid_stream_recovers() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (addr, host) = common::start_host_with_p3("127.0.0.1:0").await?;
    host.workload_start(component_workload_request(
        "streaming",
        "streaming",
        STREAMING_WASM,
        default_counter_resources(),
        http_only_host_interfaces("streaming"),
    ))
    .await
    .context("failed to start streaming workload")?;

    let client = reqwest::Client::new();
    let mut response = client
        .get(format!("http://{addr}/"))
        .header("HOST", "streaming")
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    // Read one chunk, then hang up.
    let _ = response.chunk().await?;
    drop(response);

    // Give the driver time to notice and reap, then prove the host still
    // serves complete streams.
    sleep(Duration::from_millis(700)).await;
    let response = client
        .get(format!("http://{addr}/"))
        .header("HOST", "streaming")
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let body = response.text().await?;
    assert_eq!(body, "chunk-0;chunk-1;chunk-2;");

    Ok(())
}
