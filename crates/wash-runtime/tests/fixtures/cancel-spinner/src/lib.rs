//! Test fixture for per-invocation cancellation.
//!
//! Routes:
//! - `/spin` — loop on keyvalue `increment` (one host call per iteration)
//!   until a wall-clock deadline. If cancellation works, the invocation
//!   traps at the keyvalue host boundary long before the deadline and this
//!   handler never returns. The deadline bounds the test when cancellation
//!   is broken.
//! - `/cancel/<token>` — call the host-provided `canceller` import and
//!   report whether the token matched a registered invocation.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::{
    exports::wasi::http::incoming_handler::Guest,
    wasi::{
        clocks::monotonic_clock::now,
        http::types::{
            Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
        },
        keyvalue::{atomics::increment, store::open},
    },
    wasmcloud::cancel_spinner::canceller::cancel,
};

struct Component;

/// Upper bound on the spin loop, in nanoseconds. Generous enough that a
/// successful cancel is unambiguous, small enough that a broken cancel
/// doesn't hang the test suite.
const SPIN_DEADLINE_NS: u64 = 30_000_000_000;

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let (status, body) = route(&path);
        respond(response_out, status, &body);
    }
}

fn route(path: &str) -> (u16, String) {
    if let Some(token) = path.strip_prefix("/cancel/") {
        (200, cancel(token).to_string())
    } else if path.starts_with("/spin") {
        spin()
    } else {
        (404, format!("no route for {path}"))
    }
}

fn spin() -> (u16, String) {
    let bucket = match open("") {
        Ok(bucket) => bucket,
        Err(e) => return (500, format!("failed to open bucket: {e}")),
    };

    let deadline = now() + SPIN_DEADLINE_NS;
    let mut iterations: u64 = 0;
    while now() < deadline {
        // A cancelled invocation traps inside this host call and never
        // reaches the lines below.
        if let Err(e) = increment(&bucket, "spin-count", 1) {
            return (500, format!("increment failed: {e}"));
        }
        iterations += 1;
    }

    (200, format!("completed {iterations} iterations"))
}

fn respond(response_out: ResponseOutparam, status: u16, body: &str) {
    let response = OutgoingResponse::new(Fields::new());
    response.set_status_code(status).unwrap();
    let outgoing_body = response.body().unwrap();
    ResponseOutparam::set(response_out, Ok(response));

    let stream = outgoing_body.write().unwrap();
    stream.blocking_write_and_flush(body.as_bytes()).unwrap();
    drop(stream);
    OutgoingBody::finish(outgoing_body, None).unwrap();
}

bindings::export!(Component with_types_in bindings);
