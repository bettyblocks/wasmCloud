# Cancelling a Running Workload

> Status: **design agreed (Layer 1); POC wired through the real invocation
> path** (2026-06-11). `tests/integration_invocation_cancel.rs` proves the
> full flow — handle minted per store, registered with a cancel plugin via a
> new `on_invocation_start` hook, tripped by a guest-routed `/cancel/<token>`
> request, enforced by a check in the keyvalue host fns; the spinning
> invocation traps in well under a second against a 30s bound. Production
> gaps: registry entry removal (RAII), enforcement across all host-call
> surfaces, the async-submit `202`+token response, epoch (Layer 2).
> Companion docs: [`LONG_RUNNING_WORKLOADS.md`](./LONG_RUNNING_WORKLOADS.md),
> [`CPU_BOUND_GUEST_STARVATION.md`](./CPU_BOUND_GUEST_STARVATION.md).
>
> Since the original decision, two refinements were agreed (2026-06-11):
> the **listener is an HTTP `/cancel` route handled in-guest, backed by a
> cancel host plugin** (see [Listener](#listener-the-cancel-route-via-a-host-plugin)),
> and the **trigger model is async submit** — the token is returned
> immediately, not when the invocation finishes (see
> [Trigger model](#trigger-model-async-submit)).
>
> **Jump to the decision: [Decision — Layer 1](#decision--layer-1-host-boundary-gate).**

## The question

When a workload is triggered (an HTTP request, an inter-component call, a job),
can we later send a **trigger to cancel that in-flight call**, including all the
wasm components that were linked and spun up for it?

Short answer: the wasmtime primitives exist, but **how effective a cancel is
depends on what the guest is doing**, and the current `spawn_blocking` dispatch
defeats the cleanest mechanism. This doc records why, and the options.

---

## How cancellation works with a wasmtime `Store`

A guest invocation runs inside **one `Store`**. In wash-runtime every linked
component of an invocation shares that single `Store<SharedCtx>`
(`new_store_from_metadata`, `engine/workload.rs:1166-1184`; linked ctxs live in
`SharedCtx.contexts`). Consequence: **cancel is all-or-nothing per invocation** —
trap or drop the call and the whole linked graph is torn down together. That is
exactly the "cancel including all linked components" semantics we want.

Compiled wasm only checks for interruption at **epoch boundaries** (loop headers
and function entries). The pieces:

1. `Config::epoch_interruption(true)` — compiler emits the epoch checks.
2. A background ticker calls `Engine::increment_epoch()` periodically.
3. Each `Store` has a deadline (`set_epoch_deadline`). When the global epoch
   crosses it, wasmtime consults the store's **policy**:
   - `epoch_deadline_trap()` → trap immediately
   - `epoch_deadline_callback(..)` → we decide: `Interrupt` / `Continue(n)` / `Yield(n)`
   - `epoch_deadline_async_yield_and_update(n)` → yield to the async executor

Two distinct ways to cancel an in-flight call:

| Mechanism | How | Reusable after? |
|---|---|---|
| **Trap it** | epoch callback returns `UpdateDeadline::Interrupt` → call returns `Err(Trap::Interrupt)` | store usable; component marked trapped |
| **Drop the future** | drop the `call_async` future → fiber unwinds, guest cancelled | drop the store |

Key references in `~/Repos/wasmtime`:
- `docs/examples-interrupting-wasm.md` — trap vs async-yield, fuel vs epoch.
- `crates/wasmtime/src/lib.rs:179-277` — the "Async" section: *"to prevent
  infinite execution of wasm it's recommended to place a timeout on the entire
  future ... and the periodic yields with epochs should ensure that when the
  timeout is reached it's appropriately recognized."*
- `examples/epochs.rs`, `tests/all/epoch_interruption.rs` — sync + async epoch.
- `crates/wasmtime/src/runtime/store.rs:389` — `UpdateDeadline` enum.
- `crates/wasmtime/src/runtime/fiber.rs:245-277` — future-drop = cancellation.
- `crates/wasmtime/src/config.rs:690-705` — **epochs only interrupt running
  wasm**, not a guest blocked inside a host call.

---

## Cancellability is NOT uniform — it depends on guest state

This is the crux. At the moment a cancel signal arrives, the guest is in one of
three states:

1. **Executing wasm** (CPU-bound loop, computation) → **cancellable**. The epoch
   callback fires at the next loop header / function entry (~one tick) and traps.

2. **Blocked inside a host import** (KV `get`, blobstore read, outbound
   `wasi-http`, messaging `subscribe` waiting for a message) → **NOT cancellable
   by epoch**. The guest fiber is suspended; no wasm is executing, so no epoch
   check is reached. The guest becomes cancellable again the *instant the host
   call returns*. The genuinely-stuck case is a host call that never completes —
   which is exactly the long-running-workload scenario. The only way to cancel
   here is to cancel the **host future** (drop it, or make the host fn itself
   cancellation-aware).

3. **Idle service between events** → nothing in-flight to cancel; this is the
   existing `stop_service` / deregister path.

The two mechanisms cover **disjoint** states: epoch covers running wasm;
future-drop covers host-blocked wasm. **Full cancellability needs both.**

Preconditions before *any* of this applies:
- the engine must be built with `epoch_interruption(true)`, and
- the store must be armed with the callback in `new_store_from_metadata`.

A store that wasn't armed is not cancellable at all — so "cancellable by default"
is a choice made at store creation, not free.

---

## Current state in wasmCloud (this branch)

- **No epoch interruption**: `engine/mod.rs:821-846` has no `epoch_interruption`
  call and no ticker. (Despite `CPU_BOUND_GUEST_STARVATION.md` implying it
  landed — it did not, on this branch.)
- **`spawn_blocking` blocks future-drop**: the HTTP path wraps the whole
  instantiate-and-call in `tokio::task::spawn_blocking(|| Handle::block_on(..))`
  (`host/http.rs:960`, commit `6b814dfa8`). A `spawn_blocking` task **cannot be
  cancelled by aborting/dropping its `JoinHandle`** — it runs to completion. So
  mechanism #2 (future-drop) is unavailable on the live path.
- **Fuel** is enabled only for *measurement* (`observability.rs:189`: set
  `u64::MAX`, measure consumed), not interruption.
- **No invocation registry**: `Ctx` has `id` / `component_id` / `workload_id`
  but nothing maps an invocation id → a cancel handle globally.
- The only existing bound is `CALL_TIMEOUT = 600s` on the inter-component dynamic
  call (`engine/workload.rs:931`), which fires by dropping a future — also
  defeated under `spawn_blocking`.

Net: **a triggered workload has no working cancel today.**

---

## Who watches for the stop signal?

Nothing polls. Three actors collaborate; only one is an event-driven watcher:

1. **Epoch ticker** (one OS thread per engine) — bumps the epoch every few ms.
   Makes guests *interruptible*; does not watch for cancels. Missing today.
2. **Cancel listener** (event-driven, already exists) — the NATS command
   dispatcher (`washlet/mod.rs:227` → `handle_command:315`, today handles
   `workload.start` / `workload.stop` / `workload.status`). Add a
   `workload.cancel` / `invocation.cancel` arm that does one cheap thing: look
   the id up in a registry and **set a flag**. Must run on a thread the guest
   can't starve.
3. **Epoch callback** (armed per-store, runs on the guest's fiber, driven by the
   ticker) — reads the flag and returns `Interrupt`. The only code allowed to
   turn a flag into a trap, because it's the only thing in the guest's execution
   context.

```
operator/API ──NATS workload.cancel{id}──▶ handle_command (listener)
                                              │ registry[id].store(true)
                                              ▼
                              CancelRegistry: id → Arc<AtomicBool>
                                              ▲
   epoch ticker ──tick every ~5ms──▶ guest hits epoch check
                                              │
                              epoch_deadline_callback reads flag
                                              │ true → UpdateDeadline::Interrupt
                                              ▼
            guest traps → fiber unwinds → Store drops → all linked components gone
```

---

## Spike test (proves the mechanism)

`crates/wash-runtime/tests/integration_guest_cancellation.rs` — self-contained,
drives wasmtime directly with a tight infinite-loop core module (no WASI /
component stack). Two tests, both green:

- `cancel_trigger_traps_running_guest` — external `AtomicBool` stop signal +
  epoch callback returning `Interrupt`; asserts the call returns `Trap::Interrupt`
  promptly (<1s, not via the 5s safety timeout). This is the on-demand
  stop-signal model, and it works even under `spawn_blocking`.
- `dropping_call_future_cancels_running_guest` — `tokio::time::timeout` drops the
  `call_async` future; asserts it returns near the 200ms mark. This is the
  future-drop mechanism.

NOT wired into the real invocation path — it verifies the primitive in isolation.

## POC (proves the design end-to-end)

`crates/wash-runtime/tests/integration_invocation_cancel.rs` — runs the whole
Layer 1 flow through the production invocation path:

- Engine: `Ctx.cancel_handle` (`engine/ctx.rs`), minted once per store in
  `new_store_from_metadata` and shared by the active + linked contexts;
  bound plugins are notified via a new `HostPlugin::on_invocation_start`
  default-noop hook (`plugin/mod.rs`) with `(workload_id, ctx.id, handle)` —
  *before* any guest code runs, so a cancel can't race registration.
- Actuator: `ensure_not_cancelled()` checks in the in-memory keyvalue
  `get`/`increment` host fns (`plugin/wasi_keyvalue/in_memory.rs`).
- Listener: a test-local `CancelPlugin` owns the `(workload_id, token) →
  handle` registry; the `cancel-spinner` fixture (`tests/fixtures/
  cancel-spinner/`) routes `/spin` (keyvalue-increment loop, 30s bound) and
  `/cancel/<token>` (calls the `wasmcloud:cancel-spinner/canceller` import).
- Asserted: token registered under the right workload id; bogus token is a
  no-op; real cancel returns true, the spin invocation traps at its next
  keyvalue call (<5s after cancel, ~instant in practice), and subsequent
  invocations of the same workload are unaffected.

POC simplifications vs the work list: no registry entry removal (RAII), the
token is discovered from the registry rather than returned via an async
submit `202`, and only the keyvalue host fns enforce.

---

## Options to fix this

Each is rated against the three guest states (✅ covers / ⚠️ partial / ❌ no).

### A. Epoch trap + cancel registry
Enable epoch interruption + ticker; arm each store with a callback that checks a
per-invocation flag set by `workload.cancel`.
- CPU-bound ✅ · host-blocked ❌ · idle n/a
- **Works under `spawn_blocking`** (callback runs on the guest fiber regardless).
- Cheap, low risk. The natural first increment. Insufficient alone for
  long-running host-blocked workloads.

### B. Replace `spawn_blocking` with `tokio::spawn` + epoch *yield* + abort/drop
Revisit the decision in `CPU_BOUND_GUEST_STARVATION.md`. Epoch **yield**
(`epoch_deadline_async_yield_and_update`) solves CPU starvation *just like*
`spawn_blocking` does, while keeping the task a normal async future that can be
dropped/aborted. Cancel = drop the future (and/or trap via callback).
- CPU-bound ✅ · host-blocked ✅ (drops the host future too) · idle n/a
- Removes the `spawn_blocking` blocker. **Likely the right core fix.**
- Risk: changes the dispatch model that was just merged; must re-validate the
  starvation/heartbeat behavior the `spawn_blocking` change was protecting.

### C. Cancellation-aware host functions
Wrap each host import (the `func_new_async` wrapper in `engine/workload.rs`, the
WASI/plugin host fns) to `select!` their real work against a per-invocation
cancel token. On cancel they return an error/trap; the guest then unwinds at the
next wasm point (where the epoch callback finishes the job).
- CPU-bound ✅ (via A) · host-blocked ✅ · idle n/a
- **Works even under `spawn_blocking`** — we don't cancel the blocking task from
  outside; the work *inside* it observes the signal and returns. Complements A
  to reach full coverage without abandoning `spawn_blocking`.
- Risk/cost: large mechanical surface (every host call path), and each host fn
  must clean up its own in-flight I/O correctly on cancel.

### D. Process / pod-level isolation + kill
Run long-running ("job") workloads in a separate OS process (or, on Kubernetes, a
separate pod) and cancel by killing it.
- CPU-bound ✅ · host-blocked ✅ · idle ✅ — bulletproof, sidesteps all wasmtime
  subtleties.
- Cost: process/pod spawn + IPC; loses in-process pooling-allocator sharing.
  Best reserved for a dedicated "job" workload class rather than every request.
  Fits the K8s deployment model (operator deletes the pod).

### E. Deadline / fuel resource guards (not on-demand cancel)
Per-invocation max wall-clock (extend the existing `CALL_TIMEOUT`, honored via
epoch yields) and/or a fuel budget that traps on exhaustion.
- A safety net, not a stop signal. Fuel isn't consumed during host calls, so it
  doesn't bound host-blocked time. Pair with A/B/C, don't rely on alone.

### F. P3 concurrent task model (forward-looking)
The async component model path (`Store::run_concurrent`, used in
`host/http_p3.rs`) may offer finer-grained per-subtask cancellation than the
P2 fiber model. Worth investigating as the longer-term home for cancellable
concurrent invocations. Unverified — needs a spike.

### G. Guest-cooperative cancellation (weak)
Hand the guest a "should I stop?" import to poll. Only works for cooperating,
well-behaved guests; useless for tight loops or untrusted/buggy components.
Mention for completeness; not a primary mechanism.

> The decision below selects **option C as Layer 1** (cancellation-aware host
> calls) and defers **option A (epoch)** as a separable Layer 2. The options
> above are kept as the analysis that led there.

---

## Decision — Layer 1: host-boundary gate

Arrived at via a *Simple Made Easy* design pass. The full dimension-by-dimension
synthesis is in the session log; the essentials:

- **What:** abort one in-flight invocation so it (1) initiates no new external
  effect and (2) terminates. Effects exist only at the host-call boundary;
  in-flight effects are allowed to land. Reclaiming the compute of a silent,
  host-call-free loop is **out of scope**.
- **Who:** the unit is a **single invocation** (one request the workload is
  handling) — *not* the workload, *not* a component. It needs a per-invocation
  identity (a token); `workload_id` is the wrong granularity and `Ctx.id` is
  internal-only.
- **Why:** an external actor — human or AI agent, same flow — realizes the
  invocation it started is wrong/misconfigured and aborts *that one* to prevent
  further effects. Deliberate, rare, not latency-critical.
- **How:** one concept — a per-invocation **cancellation handle** (a plain
  observable value) living in `SharedCtx` (one per store = one per invocation,
  shared by the active + all linked contexts). One enforcement seam — the
  host-call wrapper consults the handle and, if tripped, returns a **trap**
  (not a WIT-level error the guest could swallow). The guest self-unwinds → task
  ends → store drops → all linked components torn down together.
- **When/Where:** one operation; the primary **detector** is an HTTP `/cancel`
  route handled by a guest component that calls a **cancel host plugin**
  carrying the token (see below). A NATS `invocation.cancel` control verb can
  coexist — both are mere listeners that trip the same handle. (The original
  second detector, connection-drop, is moot under the async submit model:
  there is no held-open connection to drop.) Detectors only *set* the handle;
  the host boundary *reads* it.

### The three pieces

It's tempting to think of cancellation as "link a request to its store + a
listener to cancel it." That's only two of three pieces — and the missing one is
the one that does the work:

1. **Identity link** — a per-invocation token mapped to that invocation's
   cancellation handle in `SharedCtx` (a registry). *"I know which one you mean."*
2. **Listener** — the `invocation.cancel` control message (and the connection-drop
   detector) that trips the handle. *"I heard you."*
3. **Actuator** — the **host-boundary trap** that actually stops it: on the next
   host call the handle is read, a trap is returned, and the guest unwinds itself.
   *"It is now stopped."*

Pieces 1 and 2 alone do nothing — a tripped flag is inert. **The actuator is the
whole point**, and in-process it has to be the host-boundary trap, because (see
below) you cannot kill the running task directly.

### Why we can't just kill the task

The invocation runs in a **spawned** task, and spawning detaches it from the
request — dropping the `JoinHandle` doesn't cancel a task, it just detaches it
further. So closing the client connection never stops it. This is true for *both*
spawn kinds; `spawn_blocking` is merely strictly worse:

| | Auto-cancel on request/connection drop? | Explicit `handle.abort()`? |
|---|---|---|
| `tokio::spawn` (async task) | ❌ no (detached) | ✅ yes — dropped at next `.await` |
| `tokio::task::spawn_blocking` (today's HTTP path) | ❌ no (detached) | ❌ no — blocking thread runs to completion |

This is *why* cancellation must be an **explicit signal acted on from inside the
guest** (the actuator), not something you get for free by dropping a future or
killing a handle. Linking the request to the store does **not** give you a kill
lever — the store handle is inert until the host-boundary actuator reads it.

Why this is the cleanest (and what it deliberately avoids complecting):

- Cancellation is **self-inflicted from inside the guest**, so it works
  *regardless of `spawn_blocking`* (we never kill the blocking task from outside;
  it completes when the guest unwinds) and *regardless of future-drop* (which the
  spawn boundary severs anyway — verified empirically: dropping the client
  connection does **not** stop the invocation today).
- Therefore cancellation is **decoupled** from three things it must not be braided
  with: the dispatch model (`spawn_blocking`), starvation-avoidance (epoch), and
  implicit future lifecycle. Reverting `spawn_blocking` and adding epoch each
  become independent decisions that *compose* with this, not prerequisites.
- The only guest this cannot terminate is the pathological **pure CPU loop that
  makes no host calls** — which, by the What, produces no effects and is thus
  harmless. Terminating it needs epoch (**Layer 2**), which is separately
  motivated by `CPU_BOUND_GUEST_STARVATION.md`. If/when epoch lands for that
  reason, its callback reads the *same* handle and the CPU-looper is mopped up for
  free. Layer 1 never depends on Layer 2.

### Listener: the `/cancel` route via a host plugin

How does an external `/cancel` request reach the handle? Two facts about the
runtime force the shape:

- **There is no host-level path routing.** Inbound HTTP is routed purely by
  Host header → workload (`route_incoming_request`, `host/http.rs:808`); the
  HTTP server is a `HostHandler` (`host/http.rs:352`), *not* a plugin, and
  plugins have no hook for inbound traffic. So a `/cancel` path can only be
  handled *inside a guest component*.
- **Plugin instances span invocations.** The same `Arc<dyn HostPlugin>` is
  injected into every invocation's `Ctx` (`engine/ctx.rs:113`, populated from
  `metadata.plugins` at `engine/workload.rs:1145`). Plugin-held state is
  therefore visible across stores — the same pattern `InMemoryKeyValue` uses
  for its shared storage map (`plugin/wasi_keyvalue/in_memory.rs:66`).

The flow:

```
caller ──HTTP /cancel?id=<token>──▶ guest HTTP component (fresh store)
                                       │ calls imported cancel fn
                                       ▼
                              cancel host plugin
                                       │ registry[(workload_id, token)].store(true)
                                       ▼
                       running invocation's handle tripped
                                       │ next host call / epoch check
                                       ▼
                          trap → unwind → store drops
```

Design points:

- **The registry is its own value**, not plugin-owned: one
  `Arc<DashMap<(WorkloadId, Token), Handle>>` created at host build time and
  handed to *both* the engine (insert at store creation, remove via RAII
  guard) and the plugin (lookup on cancel). Plugin hooks are workload-level
  only (`plugin/mod.rs:71` — `on_workload_bind` etc.; there is no
  per-invocation hook), so insertion *must* happen in store-creation code —
  keeping the plugin a pure listener avoids complecting registry ownership
  into it.
- **Tenancy comes for free at this seam.** The plugin's host fn executes
  inside the *calling* `Ctx`, which carries `workload_id` / `component_id`
  (`engine/ctx.rs:101-103`). Keying the registry by `(workload_id, token)`
  and looking up only within the caller's scope means a tenant can only
  cancel its own invocations, even with a leaked token — no separate authz
  layer needed yet, and a real one slots into the same seam later. (The NATS
  verb has no caller identity, which is why this listener is primary.)
- **Boundary:** this scoping implies `/cancel` is served by the *same
  workload* as the invocation it cancels. Cross-workload cancel (an admin
  plane) would need an explicit trust model — widen the key deliberately,
  never accidentally.
- The plugin call runs in its own fresh store on its own task, concurrent
  with the target invocation — no store contention, and `spawn_blocking` on
  the target path is irrelevant to the listener.

### Trigger model: async submit

A token returned only when the invocation *finishes* is useless for
cancelling it. So the long-running trigger path returns immediately:

1. Mint the token (UUIDv4 — it doubles as an unguessable bearer credential
   until real authz lands; `Ctx.id` at `engine/ctx.rs:99` is the precedent
   but is never surfaced today).
2. **Insert the registry entry *before* spawning** the invocation — insert
   inside the spawned task and a fast `/cancel` races ahead of registration
   and silently no-ops. Mint → register → spawn (handle threaded into
   `SharedCtx`) → respond.
3. Return `202 Accepted` + the token. The HTTP handler today already spawns
   detached and merely *waits* on a oneshot for the response
   (`host/http.rs:931,960`); submit mode is "don't wait".
4. The RAII removal guard lives in the spawned task.

Result delivery (no connection is held open) — two composable options, per
[`LONG_RUNNING_WORKLOADS.md`](./LONG_RUNNING_WORKLOADS.md):

- **Poll:** a `/status/{id}` route, same plugin pattern as `/cancel`; the
  registry entry grows a state field (running / completed / cancelled /
  failed) plus the result or a pointer to it. `/status` enforces the same
  caller-scope check as `/cancel`, otherwise the token becomes a
  readable-by-anyone job id.
- **Push:** publish the result to NATS keyed by the token; callers subscribe
  via the existing NATS→SSE bridge.

### Epoch (Layer 2) stays per-invocation

A natural worry: the epoch ticker is **engine-global**, so does enabling it
cancel everyone? No — the ticker is just a clock. The *trap decision* is
per-store: each store arms its own `epoch_deadline_callback`, which closes
over **that invocation's** handle:

```rust
store.epoch_deadline_callback(move |_| {
    if cancel_handle.load(Ordering::Relaxed) {
        Ok(UpdateDeadline::Interrupt)   // only this invocation traps
    } else {
        Ok(UpdateDeadline::Continue(1)) // everyone else re-arms and continues
    }
});
```

Every running invocation pays an atomic flag-read per tick while executing
wasm — noise. Only the store whose handle was tripped returns `Interrupt`.
And because handle minting, registry insertion, and callback arming all
happen together in `new_store_from_metadata`, cancellability is decided in
one place and the pieces can't drift apart. This is exactly the shape the
spike test (`cancel_trigger_traps_running_guest`) proves.

Layer 2 also sharpens *when* a cancel lands. The host-boundary gate alone is
lazy — the flag is read at the next host call, which for a quiet stretch of
CPU work could be minutes away. For prompt cancellation across all guest
states:

- **Running wasm** → epoch callback traps within ~one tick.
- **Blocked in a host call** (where a long-running workload spends most of
  its time) → epoch can't reach it; the host-call wrapper must `select!` its
  real work against the handle and abandon the in-flight future on cancel —
  a strengthening of work-list item 4 (check *during* the call, not just on
  entry).

## Why this is necessary

- **Long-running workloads run for minutes** (the motivation for this whole
  branch). A misconfigured or mistaken run keeps writing to KV, sending messages,
  and making outbound calls with **no way to stop it**.
- **The only stop available today is the wrong granularity.** `workload.stop`
  targets the whole workload (all its in-flight requests), and even then it does
  not halt an in-flight, effect-producing invocation under `spawn_blocking` — it
  deregisters the service handle. There is no way to abort *one request*.
- **Dropping the client connection does nothing** — the invocation runs on
  detached and keeps producing effects (verified). So a caller who realizes their
  mistake cannot stop the damage by hanging up.
- **Humans and AI agents launch work and need a corrective abort.** Without it the
  only recourse is to wait the mistake out or nuke the entire workload (taking
  down unrelated requests). Stopping further effects **bounds the blast radius**
  of an erroneous invocation.

## What is / isn't implemented in wasmCloud today

**Present (the substrate Layer 1 builds on):**
- Per-invocation `Store` whose `SharedCtx` holds the active + all linked contexts
  (`engine/workload.rs:1166-1184`, `engine/ctx.rs:22-29`) → the all-or-nothing
  teardown substrate. The natural home for the handle.
- A NATS command dispatcher (`washlet/mod.rs:227` → `handle_command:315`) with
  `workload.start` / `workload.stop` / `workload.status` → the control channel a
  cancel verb plugs into.
- Host-call seams already exist: the dynamic inter-component wrapper
  (`engine/workload.rs` `func_new_async`, with a 600s `CALL_TIMEOUT` at `:931`)
  and the plugin host functions (keyvalue, blobstore, messaging, smtp, outgoing
  HTTP). They just don't consult any cancel state.

**Missing (what makes cancellation impossible today):**
- No per-invocation **cancellation handle** in `SharedCtx`.
- No **registry** mapping a token → handle.
- No **invocation token** minted at trigger time and surfaced to the caller
  (`Ctx.id` exists but is internal and never returned); the HTTP trigger path
  holds the connection open instead of returning a token (`host/http.rs:931`).
- Host calls **do not consult** any cancel state — no enforcement seam.
- No **cancel host plugin** (and no `invocation.cancel` control verb).
- No per-invocation plugin hook — plugin hooks are workload-level only
  (`plugin/mod.rs:71`), which is why registry insertion belongs in
  store-creation code, not in a plugin hook.
- (Layer 2, out of scope) no epoch interruption / ticker (`engine/mod.rs:821-846`
  has none, despite `CPU_BOUND_GUEST_STARVATION.md` implying otherwise).

## What we need to add (Layer 1 work list)

1. **Cancellation handle** in `SharedCtx` — e.g. an `Arc<AtomicBool>` (or
   `CancellationToken`). Created at store/invocation construction in
   `new_store_from_metadata`.
2. **Invocation token + registry** — mint a token at invocation start; a host-held
   `DashMap<(WorkloadId, Token), Handle>` (clone of the `Arc`), created at host
   build and handed to both the engine and the cancel plugin. Insert **before
   spawn** (registration race), remove at completion via an RAII guard so
   entries can't leak.
3. **Async submit trigger** — return `202` + the token immediately instead of
   awaiting the oneshot (`host/http.rs:931`); result delivery via `/status`
   poll and/or NATS→SSE push (ties into `LONG_RUNNING_WORKLOADS.md`).
4. **Host-boundary enforcement** — each host-call wrapper checks the handle and
   returns a **trap** if tripped (a trap, not a WIT `result::error`, so a guest
   can't catch-and-continue). Surfaces to cover: the dynamic inter-component
   wrapper, plus each plugin host fn (keyvalue, blobstore, messaging, smtp,
   outgoing HTTP). For prompt cancel of host-blocked guests, `select!` against
   the handle rather than checking only on entry. This is the bulk of the
   mechanical work.
5. **Cancel host plugin** — a plugin exposing a cancel import; guest `/cancel`
   route calls it; plugin looks up `(caller workload_id, token)` in the
   registry and trips the handle. Optionally also a NATS `invocation.cancel`
   arm in `handle_command` — same registry, same handle.
6. **Tests:** extend the spike to cancel through a *real* `WorkloadComponent`
   invocation that makes a host call — assert the trap, the store teardown, and
   that no further host calls fire after the cancel point. Also cover the
   registration race (cancel issued immediately after submit) and the tenancy
   scope (a different workload's token must not match).

## Deferred / open

- Terminating the pathological pure-CPU-loop (Layer 2 / epoch — see
  [Epoch (Layer 2) stays per-invocation](#epoch-layer-2-stays-per-invocation)
  for why it composes per-invocation).
- Aborting *in-flight* effects rather than letting them land (the `select!`
  strengthening of item 4 covers abandoning the host future; whether the
  underlying I/O is correctly cleaned up per host fn is still open).
- Result/status retention: how long a completed entry stays queryable in the
  registry, and where large results live (registry vs blobstore pointer).
- Cross-workload cancel (admin plane) — needs an explicit trust model before
  widening the `(workload_id, token)` key.
- Resource-cleanup correctness when the guest unwinds (host handles, DB
  connections, in-flight outbound requests).
- Whether to revert `spawn_blocking` and whether to add epoch — both now
  independent of this design.

## Pointers

- Spike test: `crates/wash-runtime/tests/integration_guest_cancellation.rs`
- Handle home: `crates/wash-runtime/src/engine/ctx.rs:22-29` (`SharedCtx`)
- Store creation (mint token + handle, register, arm callback — all here):
  `crates/wash-runtime/src/engine/workload.rs:1166-1184`
- Host-call enforcement seam: `engine/workload.rs` `func_new_async` (`CALL_TIMEOUT` at `:931`) + plugin host fns
- Plugin trait (workload-level hooks only): `crates/wash-runtime/src/plugin/mod.rs:71`
- Plugin injection into every `Ctx`: `engine/ctx.rs:113`, `engine/workload.rs:1145`
- Shared-plugin-state precedent: `crates/wash-runtime/src/plugin/wasi_keyvalue/in_memory.rs:66`
- HTTP routing (Host header only, no path routing): `crates/wash-runtime/src/host/http.rs:808`
- HTTP trigger path (oneshot wait → submit mode; `spawn_blocking`): `crates/wash-runtime/src/host/http.rs:931,960`
- NATS command listener (optional `invocation.cancel`): `crates/wash-runtime/src/washlet/mod.rs:227,315-346`
- Engine config (Layer 2 / epoch would go here): `crates/wash-runtime/src/engine/mod.rs:821-846`
