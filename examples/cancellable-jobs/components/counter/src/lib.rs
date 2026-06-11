//! Counter: an async-lifted export called CONCURRENTLY by the frontend —
//! ten in-flight `run` calls interleave inside the /create invocation's
//! store. Reports go directly to the sse-service over the workload's
//! virtual loopback network; the plugin is not involved in the data path.
//!
//! Protocol (line-based, over TCP to 127.0.0.1:8081):
//!   -> "feed <request-id> <index>"   once, on connect
//!   -> "count <n>"                   per tick (sleep mode)
//!   -> "done"                        on natural completion
//! A cancelled invocation traps mid-run and never sends "done" — the
//! service sees the connection drop abruptly and marks it cancelled.
//!
//! Modes:
//! - sleep (default): one count per second, parked on the P3 clock between
//!   ticks.
//! - burn: a pure CPU loop with ZERO host calls after the feed line —
//!   cancellable only via epoch interruption (Layer 2).

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::exports::demo::jobs::runner::Guest;
use bindings::wasi::clocks::monotonic_clock::wait_for;
use bindings::wasi::sockets::types::{
    IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket,
};

struct Component;

/// Sleep mode: ~5 minutes of once-per-second counts.
const MAX_COUNTS: u32 = 300;
/// Burn mode: iteration bound so an uncancelled burn still terminates.
const BURN_ITERATIONS: u64 = 50_000_000_000;
/// CPU slice between cooperative yields (~tens of milliseconds).
const BURN_CHUNK: u64 = 100_000_000;

const SSE_SERVICE_ADDR: IpSocketAddress = IpSocketAddress::Ipv4(Ipv4SocketAddress {
    port: 8081,
    address: (127, 0, 0, 1),
});

impl Guest for Component {
    async fn run(request_id: String, index: u32, burn: bool) {
        let Ok(socket) = TcpSocket::create(IpAddressFamily::Ipv4) else {
            return;
        };
        if socket.connect(SSE_SERVICE_ADDR).await.is_err() {
            return;
        }

        let (mut tx, rx) = bindings::wit_stream::new();
        futures::join!(
            async {
                let _ = socket.send(rx).await;
            },
            async move {
                tx.write_all(format!("feed {request_id} {index}\n").into_bytes())
                    .await;

                if burn {
                    // CPU-bound counting. Cancellation does NOT depend on the
                    // periodic yields below — the epoch callback traps the
                    // running wasm mid-loop within milliseconds — but wasm in
                    // one store is single-threaded and cooperatively
                    // scheduled, so without yields this loop would starve the
                    // sibling counters and even the /create response delivery
                    // (wasmtime #11869).
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
                    for count in 0..MAX_COUNTS {
                        wait_for(1_000_000_000).await;
                        // Traps here (or at the next epoch tick) once the
                        // group is cancelled.
                        tx.write_all(format!("count {count}\n").into_bytes()).await;
                    }
                }

                tx.write_all(b"done\n".to_vec()).await;
                drop(tx);
            }
        );
    }
}

bindings::export!(Component with_types_in bindings);
