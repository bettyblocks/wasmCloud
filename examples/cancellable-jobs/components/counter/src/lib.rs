//! Counter: one invocation per counter, started programmatically by the
//! JobsPlugin. Reports directly to the sse-service over the workload's
//! virtual loopback network (the documented component-to-service channel) —
//! no plugin involvement in the data path.
//!
//! Protocol (line-based, over TCP to 127.0.0.1:8081):
//!   -> "feed <request-id> <index>"   once, on connect
//!   -> "count <n>"                   per tick (sleep mode)
//!   -> "done"                        on natural completion
//! A cancelled invocation traps mid-run and never sends "done" — the
//! service sees the connection drop abruptly and marks it cancelled.
//!
//! Modes:
//! - sleep (default): one count per second; between ticks the guest is
//!   host-blocked, so a cancel lands at the next wasm instruction after
//!   the tick (~1s worst case).
//! - burn: a pure CPU loop with ZERO host calls after the feed line. The
//!   Layer 1 host-boundary gate never sees it — only epoch interruption
//!   (Layer 2) can trap it, which it does within a tick.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::exports::demo::jobs::runner::Guest;
use wstd::io::AsyncWrite;
use wstd::time::Duration;

const SSE_SERVICE_ADDR: &str = "127.0.0.1:8081";

/// Sleep mode: ~5 minutes of once-per-second counts.
const MAX_COUNTS: u32 = 300;
/// Burn mode: iteration bound so an uncancelled burn still terminates.
const BURN_ITERATIONS: u64 = 50_000_000_000;

struct Component;

impl Guest for Component {
    fn run(request_id: String, index: u32, burn: bool) {
        wstd::runtime::block_on(async move {
            let Ok(mut feed) = wstd::net::TcpStream::connect(SSE_SERVICE_ADDR).await else {
                return;
            };
            if feed
                .write_all(format!("feed {request_id} {index}\n").as_bytes())
                .await
                .is_err()
                || feed.flush().await.is_err()
            {
                return;
            }

            if burn {
                // Pure CPU from here on: no host calls until the loop ends.
                let mut acc: u64 = 0;
                for i in 0..BURN_ITERATIONS {
                    acc = acc.wrapping_add(std::hint::black_box(i));
                }
                std::hint::black_box(acc);
            } else {
                for count in 0..MAX_COUNTS {
                    wstd::task::sleep(Duration::from_secs(1)).await;
                    if feed
                        .write_all(format!("count {count}\n").as_bytes())
                        .await
                        .is_err()
                        || feed.flush().await.is_err()
                    {
                        return;
                    }
                }
            }

            let _ = feed.write_all(b"done\n").await;
            let _ = feed.flush().await;
        });
    }
}

bindings::export!(Component with_types_in bindings);
