//! Proves that a *running* WebAssembly guest can be cancelled on demand, and
//! documents exactly which wasmtime primitives make that possible.
//!
//! These are deliberately self-contained: they drive `wasmtime` directly with a
//! tiny core-wasm module that loops forever, rather than going through the full
//! wash-runtime HTTP/component stack. The point is to verify the *mechanism* in
//! isolation so it can then be wired into the real invocation path.
//!
//! Background — how cancellation works with a wasmtime `Store`:
//!
//!   * A guest invocation runs inside one `Store`. In wash-runtime every linked
//!     component of an invocation shares that single `Store`
//!     (`SharedCtx.contexts`), so cancelling the call tears the whole linked
//!     graph down at once — it is inherently all-or-nothing per invocation.
//!
//!   * Compiled wasm only checks for interruption at *epoch boundaries* (loop
//!     headers and function entries). With `Config::epoch_interruption(true)` a
//!     background ticker calls `Engine::increment_epoch()`; when a store's
//!     deadline is crossed wasmtime consults the store's epoch policy:
//!       - `epoch_deadline_trap()`              -> trap immediately
//!       - `epoch_deadline_callback(..)`        -> we decide: Interrupt / Continue / Yield
//!       - `epoch_deadline_async_yield_and_update(..)` -> yield to the async executor
//!
//!   * Two ways to cancel an in-flight call:
//!       1. TRAP IT (test `cancel_trigger_traps_running_guest`): an external
//!          flag is checked in the epoch callback; when set we return
//!          `UpdateDeadline::Interrupt` and the call returns `Err(Trap::Interrupt)`.
//!          Works even for a fully CPU-bound guest that never yields.
//!       2. DROP THE FUTURE (test `dropping_call_future_cancels_running_guest`):
//!          on the async path, dropping the `call_async` future unwinds the
//!          fiber and cancels the guest. We trigger that here with
//!          `tokio::time::timeout`. This requires the guest to actually yield
//!          back to the executor, hence `epoch_deadline_async_yield_and_update`.
//!
//!   * CAVEAT mirrored from wasmtime's own docs: epochs only interrupt running
//!     *wasm*. A guest blocked inside a host import (a long KV/blobstore/HTTP
//!     call) is not interrupted by epochs — that needs the async host-fn path
//!     plus a timeout. And note the production `spawn_blocking` HTTP path in
//!     `host/http.rs` cannot use mechanism #2: a `spawn_blocking` task can't be
//!     cancelled by dropping its `JoinHandle`, so a trap-based kill switch
//!     (mechanism #1) is the route that works there.
//!
//! Docs: wasmtime `docs/examples-interrupting-wasm.md`, the "Async" section of
//! `wasmtime/src/lib.rs`, and `examples/epochs.rs`.

#![cfg(not(miri))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wasmtime::{Config, Engine, Instance, Store, UpdateDeadline};

/// A guest that never returns on its own: a tight infinite loop. wasmtime
/// inserts an epoch check at the loop header, so it is interruptible.
const INFINITE_LOOP_WAT: &str = r#"
    (module
      (func (export "run")
        (loop $l br $l)))
"#;

/// Spawn an OS thread (not a tokio task — it must keep ticking even while a
/// CPU-bound guest pins a worker thread) that bumps the engine epoch until the
/// returned flag is set. Bumping every few ms means the guest's epoch callback
/// runs at roughly that cadence.
fn spawn_epoch_ticker(engine: &Engine, period: Duration) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let engine = engine.clone();
    let stop_clone = stop.clone();
    std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            engine.increment_epoch();
            std::thread::sleep(period);
        }
    });
    stop
}

fn engine_with_epochs() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    // Note: async execution (`call_async` / `instantiate_async`) is always
    // available because the crate builds wasmtime with its async feature; the
    // old `Config::async_support` toggle is deprecated and has no effect.
    Ok(Engine::new(&config)?)
}

/// Mechanism #1 — an external "cancel" trigger traps a running, CPU-bound guest.
///
/// This is the model the user asked about: fire a trigger from outside and the
/// in-flight call (plus everything in its store) stops. The trigger is an
/// `AtomicBool`; the epoch callback observes it and returns `Interrupt`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_trigger_traps_running_guest() -> anyhow::Result<()> {
    let engine = engine_with_epochs()?;
    let stop_ticker = spawn_epoch_ticker(&engine, Duration::from_millis(5));

    let module = wasmtime::Module::new(&engine, wat::parse_str(INFINITE_LOOP_WAT)?)?;
    let mut store = Store::new(&engine, ());

    // The external trigger. In wash-runtime this would live in a registry keyed
    // by invocation/job id, set when a NATS "cancel" message arrives.
    let cancel = Arc::new(AtomicBool::new(false));

    // Arm the epoch policy: at every deadline crossing, kill if the trigger is
    // set, otherwise re-arm one tick ahead and keep running.
    store.set_epoch_deadline(1);
    {
        let cancel = cancel.clone();
        store.epoch_deadline_callback(move |_store_ctx| {
            if cancel.load(Ordering::Relaxed) {
                Ok(UpdateDeadline::Interrupt)
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });
    }

    let instance = Instance::new_async(&mut store, &module, &[]).await?;
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;

    // Kick off the (never-terminating) call on its own task.
    let call = tokio::spawn(async move { run.call_async(&mut store, ()).await });

    // Let it spin, then fire the cancel trigger.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let fired_at = Instant::now();
    cancel.store(true, Ordering::Relaxed);

    // The call must come back promptly with a trap, not run forever.
    let result = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("guest was not cancelled within 5s — interruption did not take effect")?;

    let err = result.expect_err("guest returned Ok, but it should have been trapped");
    let trap = err
        .downcast_ref::<wasmtime::Trap>()
        .copied()
        .expect("expected a wasmtime::Trap");
    assert_eq!(trap, wasmtime::Trap::Interrupt, "expected an Interrupt trap");

    // Sanity: cancellation was prompt relative to the 5ms tick, not a fluke of
    // the 5s outer timeout.
    assert!(
        fired_at.elapsed() < Duration::from_secs(1),
        "cancellation took too long: {:?}",
        fired_at.elapsed()
    );

    stop_ticker.store(true, Ordering::Relaxed);
    Ok(())
}

/// Mechanism #2 — dropping the `call_async` future cancels the guest.
///
/// On the pure-async path, `tokio::time::timeout` dropping the in-flight future
/// unwinds the wasm fiber. This is the cleanest cancellation, but it only works
/// if the guest yields back to the executor — which is what
/// `epoch_deadline_async_yield_and_update` arranges. (The production
/// `spawn_blocking` path defeats this; see the module docs.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_call_future_cancels_running_guest() -> anyhow::Result<()> {
    let engine = engine_with_epochs()?;
    let stop_ticker = spawn_epoch_ticker(&engine, Duration::from_millis(5));

    let module = wasmtime::Module::new(&engine, wat::parse_str(INFINITE_LOOP_WAT)?)?;
    let mut store = Store::new(&engine, ());

    // Yield to the async executor every epoch tick, then extend the deadline.
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);

    let instance = Instance::new_async(&mut store, &module, &[]).await?;
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;

    let started = Instant::now();
    // The timeout elapsing drops the call future, which cancels the guest.
    let outcome = tokio::time::timeout(
        Duration::from_millis(200),
        run.call_async(&mut store, ()),
    )
    .await;

    assert!(
        outcome.is_err(),
        "expected the call future to be dropped by the timeout"
    );
    // It returned because the future was dropped, near the 200ms mark — not
    // because the infinite loop somehow finished.
    let elapsed = started.elapsed();
    assert!(
        (Duration::from_millis(150)..Duration::from_millis(1500)).contains(&elapsed),
        "cancellation timing looks wrong: {elapsed:?}"
    );

    stop_ticker.store(true, Ordering::Relaxed);
    Ok(())
}
