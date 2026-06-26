//! Counter: the work. An async-lifted `runner.run` export, called once by
//! the frontend through the runtime's linked-call trampoline so it runs
//! inside the /do-work store and keeps that store alive while it works.
//!
//! Progress and completion are reported to the host JobsPlugin via
//! `demo:jobs/control`; the plugin owns the per-id status record. A
//! cancelled run is trapped mid-execution and never reaches `complete` — the
//! frozen `count` the plugin already holds is the proof it stopped.
//!
//! Modes:
//! - tick (default): one count per second to a small deadline, parked on the
//!   P3 clock between ticks. Cancellation lands when the sleep returns or at
//!   the next epoch tick — so within ~1s.
//! - cpu: a pure CPU loop with NO progress reports — cancellable only via
//!   epoch interruption (Layer 2), which traps the running wasm mid-loop.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::demo::jobs::control;
use bindings::exports::demo::jobs::runner::Guest;
use bindings::wasi::clocks::monotonic_clock::wait_for;

struct Component;

/// tick mode: count to ten, one per second.
const MAX_COUNTS: u32 = 10;
/// cpu mode: iteration bound so an uncancelled burn still terminates.
const BURN_ITERATIONS: u64 = 60_000_000_000;
/// CPU slice between cooperative yields (~tens of milliseconds).
const BURN_CHUNK: u64 = 100_000_000;

impl Guest for Component {
    async fn run(request_id: String, mode: String) {
        if mode == "cpu" {
            // Pure CPU. Cancellation does NOT depend on the periodic yields
            // below — the epoch callback traps the running wasm mid-loop
            // within milliseconds — but wasm in one store is single-threaded
            // and cooperatively scheduled, so without yields this loop would
            // starve the detached driver and even the /do-work response
            // delivery (wasmtime #11869).
            let mut acc: u64 = 0;
            let mut i: u64 = 0;
            while i < BURN_ITERATIONS {
                let chunk_end = (i + BURN_CHUNK).min(BURN_ITERATIONS);
                while i < chunk_end {
                    acc = acc.wrapping_add(std::hint::black_box(i));
                    i += 1;
                }
                wit_bindgen::yield_async().await;
            }
            std::hint::black_box(acc);
        } else {
            for count in 1..=MAX_COUNTS {
                wait_for(1_000_000_000).await;
                // Traps here (or at the next epoch tick) once cancelled, so
                // the count the plugin holds stops advancing.
                control::progress(&request_id, count);
            }
        }

        control::complete(&request_id);
    }
}

bindings::export!(Component with_types_in bindings);
