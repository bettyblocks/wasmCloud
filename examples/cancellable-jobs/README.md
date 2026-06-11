# cancellable-jobs

End-to-end demo of **per-invocation cancellation** (see
`crates/wash-runtime/docs/WORKLOAD_CANCELLATION.md`) as a product flow,
running under `wash dev`:

- `POST /create` returns a request-id immediately (async submit) and starts
  **10 concurrent invocations** of the counter component.
- Each counter counts once per second and reports through a host-plugin
  import — which doubles as the **cancellation actuator**.
- A long-lived **service component** owns the client-facing SSE streams
  (components stay short-lived; long-lived connections belong in services).
- `POST /cancel/<id>` trips the cancellation handle of all 10 invocations:
  each one **traps at its next host call**, and SSE clients receive
  `event: cancelled` before the stream closes.

## Run it

```sh
# from this directory; wash must include the demo:jobs plugin (this repo's wash)
wash dev
```

Then, in another terminal:

```sh
# 1. create a job group (returns the request-id)
ID=$(curl -s -X POST http://127.0.0.1:8000/create)

# 2. follow the live event stream (-N disables curl buffering)
curl -N http://127.0.0.1:8000/events/$ID

# 3. cancel from a third terminal — the stream above prints
#    "event: cancelled" and closes; all 10 counters trap within ~1s
curl -X POST http://127.0.0.1:8000/cancel/$ID    # -> true
curl -X POST http://127.0.0.1:8000/cancel/$ID    # -> false (idempotent)
```

Expected stream:

```
event: count
data: {"index":3,"count":7}
...10 counts/second, one per counter...
event: cancelled
data: {}
<EOF>
```

With `RUST_LOG=info,wash::jobs_plugin=debug` the host log shows each
cancelled counter unwinding:
`counter ended with error: counter N ended: ... invocation cancelled`.

## Architecture

```
curl ──POST /create──────▶ frontend (per-request component)
                              │ demo:jobs/control.create        ┐
curl ──POST /cancel/<id>─▶ frontend                             │ JobsPlugin
                              │ demo:jobs/control.cancel        │ (host, in
                                                                │ crates/wash)
        counter ×10 (component invocations, spawned by plugin)  │
                              │ demo:jobs/reporter.report ──────┤   ← actuator:
                              │   (traps when handle tripped)   │     traps the
                                                                │     invocation
curl ──GET /events/<id>──▶ frontend ──virtual loopback TCP──▶ sse-service
        (byte pipe only)                                       (pinned service)
                                                                │ demo:jobs/events
                                                                │ subscribe/poll-next
```

- **JobsPlugin** (`crates/wash/src/jobs_plugin.rs`, registered in `wash dev`)
  holds the cross-invocation state: `request-id → { creator workload,
  state, 10 cancel handles, broadcast channel }`. On create it spawns the
  counter invocations programmatically and captures each store's
  `cancel_handle`; on cancel it verifies the **caller's workload created
  the group** (tenancy seam), trips every handle, and broadcasts.
- **Counters** trap inside `report` (`ensure_not_cancelled`) — the
  invocation unwinds, its store is torn down, no further effects happen.
- **sse-service** is a `wstd` command component pinned as the workload's
  service. Guests cannot bind real host ports (the runtime virtualizes
  loopback per workload, `engine/workload.rs` / `sockets/tcp.rs`), so the
  service listens on the **virtual** `127.0.0.1:8081` and the frontend's
  `/events` route pipes bytes between the wash HTTP ingress and that
  virtual connection. State and stream handling live in the service; the
  frontend invocation is a dumb pipe.

## Files

```
.wash/config.yaml      frontend = main component, counter = sidecar,
                       sse-service = dev service; demo:jobs interfaces are
                       declared explicitly (one entry per interface — wash
                       dev's auto-extraction merges by namespace:package,
                       which would let the first component shadow the rest)
wit/demo-jobs/         the demo:jobs package (source of truth; copies live
                       in each component's wit/deps and inline in the
                       plugin's bindgen! — keep them in sync)
components/frontend    per-request HTTP component (control + /events pipe)
components/counter     counter component (runner export, reporter import)
components/sse-service wstd service (events import, virtual TCP listener)
```

## Known demo limits

- Counters self-bound at 300 counts (~5 min); an uncancelled group then
  broadcasts `event: done`.
- Registry entries (groups) are kept until workload unbind — no eviction.
- One `spawn_blocking` thread is held per open `/events` connection (the
  pipe) for the duration of the stream.
- The cancellation handle is only checked at host-call boundaries; a
  counter sleeping its 1s tick dies at the *next* `report`, so cancel
  latency is up to ~1s here. (Epoch interruption — Layer 2 in the design
  doc — would make this immediate even mid-computation.)
