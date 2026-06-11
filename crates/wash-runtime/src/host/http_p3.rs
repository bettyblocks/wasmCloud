//! P3 HTTP handler for WASIP3 components.
//!
//! This module provides the HTTP request handling path for components that
//! target WASIP3's `wasi:http/handler` interface. It uses wasmtime-wasi-http's
//! P3 `ServicePre`/`Service` to invoke the component.
//!
//! ## Async submit + streaming
//!
//! Unlike the P2 path, the response is forwarded to hyper **as soon as the
//! guest produces it** (`task.return`), with a live streaming body — the
//! guest's `GuestBody` polls plain channels, so hyper can consume it on the
//! connection task. The store itself is owned by a detached driver task that
//! keeps the component-model event loop running until every guest task
//! (including work spawned after the response was returned) has exited and
//! the response body has been fully consumed (or dropped by the client).
//!
//! This makes two long-running shapes possible:
//! - **async submit**: a handler returns `202`-style responses immediately
//!   while spawned guest work continues in the background;
//! - **streaming responses** (e.g. SSE): the guest writes body chunks over
//!   time and the client sees them live.
//!
//! There is deliberately no timeout on the driver: per-call timeouts inside
//! `run_concurrent` are unreliable (wasmtime #11869/#11870), and runaway
//! invocations are already cancellable store-wide via the per-invocation
//! cancellation handle + epoch interruption armed in
//! `new_store_from_metadata` (see docs/WORKLOAD_CANCELLATION.md).

use std::pin::Pin;
use std::task::{Context, Poll};

use crate::engine::ctx::SharedCtx;
use crate::observability::FuelConsumptionMeter;
use http_body_util::BodyExt;
use tracing::Instrument;
use wasmtime::Store;
use wasmtime::component::InstancePre;
use wasmtime_wasi_http::p3::bindings::ServicePre;
use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

type P3Body = http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, ErrorCode>;

/// How long background guest work may go without any tracked host activity
/// before the driver considers the invocation finished and drops the store.
const BACKGROUND_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
/// Polling cadence of the post-response drain loop.
const DRAIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Handle an HTTP request using the WASIP3 `wasi:http/handler` interface.
pub async fn handle_component_request_p3(
    mut store: Store<SharedCtx>,
    pre: InstancePre<SharedCtx>,
    req: hyper::Request<hyper::body::Incoming>,
    fuel_meter: FuelConsumptionMeter,
) -> anyhow::Result<hyper::Response<P3Body>> {
    let service_pre = ServicePre::new(pre)
        .map_err(|e| anyhow::anyhow!(e).context("failed to create P3 ServicePre"))?;

    // Convert the hyper request body — map error type since hyper::Error doesn't impl Into<ErrorCode>
    let (parts, body) = req.into_parts();
    let attributes = [
        opentelemetry::KeyValue::new("method", parts.method.to_string()),
        opentelemetry::KeyValue::new("scheme", "http"),
    ];
    let body = body
        .map_err(|e| ErrorCode::InternalError(Some(e.to_string())))
        .boxed_unsync();
    let req = hyper::Request::from_parts(parts, body);
    let (wasi_req, req_io) = wasmtime_wasi_http::p3::Request::from_http(req);

    let (resp_tx, resp_rx) =
        tokio::sync::oneshot::channel::<anyhow::Result<hyper::Response<P3Body>>>();

    // Detached driver: owns the store, keeps the component-model event loop
    // alive past the response so background guest tasks and the streaming
    // body can complete. Errors after the response was forwarded can only be
    // logged.
    tokio::spawn(
        async move {
            let (body_done_tx, body_done_rx) = tokio::sync::oneshot::channel::<()>();
            let mut resp_tx = Some(resp_tx);
            let mut body_done_tx = Some(body_done_tx);
            let host_work = store.data().active_ctx.host_work.clone();

            let service = match service_pre.instantiate_async(&mut store).await {
                Ok(service) => service,
                Err(e) => {
                    if let Some(tx) = resp_tx.take() {
                        let _ = tx.send(Err(anyhow::anyhow!(e)
                            .context("failed to instantiate P3 service")));
                    }
                    return;
                }
            };

            let driving = fuel_meter
                .observe(&attributes, &mut store, async move |store| {
                    store
                        .run_concurrent(async move |store| {
                            let handler_fut = async {
                                match service.handle(store, wasi_req).await {
                                    Ok(Ok(response)) => {
                                        let converted = store.with(|s| {
                                            response.into_http(s, async { Ok(()) })
                                        });
                                        let outcome = match converted {
                                            Ok(http_response) => {
                                                // Forward headers + a LIVE body; learn via
                                                // the signal when hyper finished (or the
                                                // client disconnected) so the store isn't
                                                // dropped while chunks are still in flight.
                                                let done = body_done_tx
                                                    .take()
                                                    .expect("body_done_tx taken once");
                                                Ok(http_response.map(|inner| {
                                                    SignalOnEnd::new(inner, done).boxed_unsync()
                                                }))
                                            }
                                            Err(e) => Err(anyhow::Error::from(e)
                                                .context("failed to convert P3 response")),
                                        };
                                        if let Some(tx) = resp_tx.take() {
                                            // Client may be gone already; keep driving —
                                            // background guest work still gets to finish.
                                            let _ = tx.send(outcome);
                                        }
                                    }
                                    Ok(Err(error_code)) => {
                                        tracing::error!(
                                            error_code = ?error_code,
                                            "P3 HTTP handler returned ErrorCode",
                                        );
                                        if let Some(tx) = resp_tx.take() {
                                            let _ = tx.send(empty_response(500));
                                        }
                                    }
                                    Err(e) => {
                                        if let Some(tx) = resp_tx.take() {
                                            let _ = tx.send(Err(anyhow::anyhow!(e)
                                                .context("P3 handler trap")));
                                        }
                                    }
                                }
                            };
                            let io_fut = async {
                                if let Err(e) = req_io.await {
                                    tracing::error!(err = ?e, "P3 request I/O error");
                                }
                            };
                            let ((), ()) = tokio::join!(handler_fut, io_fut);

                            // Wait until hyper consumed (or dropped) the response body,
                            // so buffered pipe chunks aren't lost with the store. While
                            // we wait here, run_concurrent keeps driving the guest's
                            // tasks (the closure being pending is what keeps the event
                            // loop alive).
                            let _ = body_done_rx.await;

                            // Outlive background guest work (async submit): wasmtime 44
                            // exposes no "all guest tasks exited" signal, so we drain on
                            // wash's own host-work tracker — background work is kept
                            // alive while it has in-flight host activity (outbound HTTP,
                            // concurrent linked calls), bridged by a grace window for
                            // pure-compute gaps. Documented contract: background work
                            // that goes silent (no tracked host activity) longer than
                            // the grace window may be reaped.
                            let mut quiet_since = std::time::Instant::now();
                            loop {
                                if host_work.in_flight() > 0 {
                                    quiet_since = std::time::Instant::now();
                                } else if quiet_since.elapsed() >= BACKGROUND_GRACE {
                                    break;
                                }
                                tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
                            }
                            anyhow::Ok(())
                        })
                        .await
                        .map_err(anyhow::Error::from)?
                })
                .await;

            if let Err(e) = driving {
                tracing::error!(err = ?e, "P3 invocation driver failed");
            }
            // store dropped here — all guest work is done
        }
        .instrument(tracing::Span::current()),
    );

    match resp_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "P3 component finished without producing a response"
        )),
    }
}

fn empty_response(status: u16) -> anyhow::Result<hyper::Response<P3Body>> {
    let body = http_body_util::Empty::new()
        .map_err(|never| match never {})
        .boxed_unsync();
    hyper::Response::builder()
        .status(status)
        .body(body)
        .map_err(anyhow::Error::from)
}

/// Body adapter that fires a oneshot when the body ends — either normally
/// (final frame / error) or because hyper dropped it (client disconnect).
/// The P3 driver waits on this signal before dropping the store.
struct SignalOnEnd {
    inner: P3Body,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SignalOnEnd {
    fn new(inner: P3Body, done: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            inner,
            done: Some(done),
        }
    }

    fn signal(&mut self) {
        if let Some(tx) = self.done.take() {
            let _ = tx.send(());
        }
    }
}

impl http_body::Body for SignalOnEnd {
    type Data = bytes::Bytes;
    type Error = ErrorCode;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(None) => {
                this.signal();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.signal();
                Poll::Ready(Some(Err(e)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for SignalOnEnd {
    fn drop(&mut self) {
        self.signal();
    }
}
