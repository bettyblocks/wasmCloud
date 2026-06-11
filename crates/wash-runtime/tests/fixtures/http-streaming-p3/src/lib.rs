//! Streaming fixture: returns a response whose body is written over time —
//! three chunks, 200ms apart. With a live-streaming HTTP path the client
//! sees the first chunk ~immediately; with a buffering path it would see
//! everything only after ~400ms.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::clocks::monotonic_clock::wait_for;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};

struct Component;

const CHUNK_GAP_NS: u64 = 200_000_000;

impl Handler for Component {
    async fn handle(_request: Request) -> Result<Response, ErrorCode> {
        let headers = Fields::new();
        let (mut tx, rx) = bindings::wit_stream::new();
        let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());

        wit_bindgen::spawn(async move {
            for i in 0..3u32 {
                if i > 0 {
                    wait_for(CHUNK_GAP_NS).await;
                }
                tx.write_all(format!("chunk-{i};").into_bytes()).await;
            }
            drop(tx);
            let _ = trailers_tx.write(Ok(None)).await;
        });

        let (response, _result) = Response::new(headers, Some(rx), trailers_rx);
        Ok(response)
    }
}

bindings::export!(Component with_types_in bindings);
