//! Concurrency callee: an async-lifted export that suspends (sleeps) before
//! returning. Under the classic blocking trampoline this would trap the
//! caller ("cannot block a synchronous task"); under the concurrent
//! trampoline, several of these calls interleave within one store.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::exports::wasmcloud::test_concurrent::worker::Guest;
use bindings::wasi::clocks::monotonic_clock::wait_for;

struct Component;

impl Guest for Component {
    async fn work(ms: u64) -> u64 {
        wait_for(ms * 1_000_000).await;
        ms
    }
}

bindings::export!(Component with_types_in bindings);
