//! Concurrency caller: issues four CONCURRENT linked calls into the callee
//! component (each sleeping ~300ms) and reports the elapsed wall-clock time
//! in the response body. Concurrent trampolines → ~300ms; the old blocking
//! trampoline would either trap or take ~1200ms.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::clocks::monotonic_clock::now;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};
use bindings::wasmcloud::test_concurrent::worker::work;

struct Component;

const CALLS: usize = 4;
const SLEEP_MS: u64 = 300;

impl Handler for Component {
    async fn handle(_request: Request) -> Result<Response, ErrorCode> {
        let started = now();
        let results =
            futures::future::join_all((0..CALLS).map(|_| work(SLEEP_MS))).await;
        let elapsed_ms = (now() - started) / 1_000_000;

        let ok = results.iter().all(|&r| r == SLEEP_MS);
        let body_bytes = format!("elapsed_ms={elapsed_ms};ok={ok}").into_bytes();

        let headers = Fields::new();
        let (mut tx, rx) = bindings::wit_stream::new();
        let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());
        wit_bindgen::spawn(async move {
            tx.write_all(body_bytes).await;
            drop(tx);
            let _ = trailers_tx.write(Ok(None)).await;
        });

        let (response, _result) = Response::new(headers, Some(rx), trailers_rx);
        Ok(response)
    }
}

bindings::export!(Component with_types_in bindings);
