//! Counter: one invocation per counter, started programmatically by the
//! JobsPlugin. Counts once per second and reports each count through the
//! reporter import — which is also the cancellation actuator: when the
//! group is cancelled the invocation traps inside `report` and unwinds.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::{
    demo::jobs::reporter::report, exports::demo::jobs::runner::Guest,
    wasi::clocks::monotonic_clock::subscribe_duration,
};

struct Component;

/// Upper bound so an uncancelled counter still terminates (~5 minutes).
const MAX_COUNTS: u32 = 300;
const TICK_NS: u64 = 1_000_000_000;

impl Guest for Component {
    fn run(request_id: String, index: u32) {
        for count in 0..MAX_COUNTS {
            subscribe_duration(TICK_NS).block();
            // Traps here once the group is cancelled.
            report(&request_id, index, count);
        }
    }
}

bindings::export!(Component with_types_in bindings);
