# cancellable-jobs

End-to-end demo of **per-invocation cancellation** (see
`crates/wash-runtime/docs/WORKLOAD_CANCELLATION.md`) as a product flow,
running under `wash dev`:

- `POST /create` returns a request-id immediately (async submit) and starts
  **10 concurrent invocations** of the counter component.
- Counters report their counts **directly to a long-lived SSE service over
  the workload's virtual loopback network** — the documented
  component→service channel ("components cannot call service exports via
  WIT, they can connect to TCP ports the service is listening on",
  [wash create-services docs](https://wasmcloud.com/docs/wash/developer-guide/create-services/)).
- `POST /cancel/<id>` trips the cancellation handle of all 10 invocations.
  Cancellation is enforced by the **runtime**, not by app code:
  - **Layer 1** — host functions check the handle and trap;
  - **Layer 2** — the epoch callback traps the guest **mid-wasm**, even in
    a pure CPU loop that never calls the host.
- `POST /create?mode=burn` showcases Layer 2: counters that compute
  silently with zero host calls — and still die within milliseconds of a
  cancel.

## Run it

```sh
# from this directory; wash must include the demo:jobs plugin (this repo's wash)
wash dev
```

Then, in other terminals:

```sh
# 1. create a job group
ID=$(curl -s -X POST http://127.0.0.1:8000/create)

# 2. follow the live event stream (-N disables curl buffering)
curl -N http://127.0.0.1:8000/events/$ID

# 3. cancel — the stream prints "event: cancelled" and closes;
#    all 10 counters trap
curl -X POST http://127.0.0.1:8000/cancel/$ID    # -> true
curl -X POST http://127.0.0.1:8000/cancel/$ID    # -> false (idempotent)

# 4. the Layer 2 showcase: pure-CPU counters (no host calls, no counts —
#    uncancellable by host-boundary checks alone)
ID=$(curl -s -X POST "http://127.0.0.1:8000/create?mode=burn")
curl -N http://127.0.0.1:8000/events/$ID &       # silent until terminal
curl -X POST http://127.0.0.1:8000/cancel/$ID    # -> true; watcher prints
                                                 #    "event: cancelled" ~instantly
```

## Architecture

```
curl ──POST /create[?mode=burn]──▶ frontend ──demo:jobs/control.create──┐
curl ──POST /cancel/<id>─────────▶ frontend ──demo:jobs/control.cancel─┤
                                                                       ▼
                                                              JobsPlugin (host)
                                                              · spawns 10 counter
                                                                invocations, captures
                                                                their cancel handles
                                                              · cancel: tenancy check
                                                                + trip all handles
                                                                       │ trips
                                                                       ▼
                                   runtime actuators read the handle:
                                   host-boundary checks (L1) + epoch callback (L2)
                                                                       │ trap
        counter ×10 ──"feed/count/done" over virtual loopback TCP──▶ sse-service
                                                                    (pinned service)
curl ──GET /events/<id>──▶ frontend ──"watch <id>" virtual TCP────▶ sse-service
        (byte pipe only)                                            pushes SSE frames
```

- **JobsPlugin** (`crates/wash/src/jobs_plugin.rs`) does only the two
  things a guest cannot: spawn background invocations (capturing each
  store's `cancel_handle`) and trip handles on cancel (host-side state,
  scoped to the creating workload). The data path never touches it.
- **Counters** own a TCP feed to the service: `feed <id> <index>`, then
  `count <n>` per second (sleep mode) or silence (burn mode), then `done`.
  A cancelled counter traps mid-run — its connection drops without `done`,
  which is how the service knows the group was cancelled rather than
  finished.
- **sse-service** (pure wstd command component, pinned as the workload's
  service) multiplexes feeds and watchers on one virtual port and pushes
  frames to watchers as counts arrive. No polling anywhere.
- Guests can never bind real host ports (the runtime virtualizes loopback
  per workload), so the client-facing leg enters through the wash HTTP
  server: the frontend's `/events` route is a dumb byte pipe into the
  service's virtual port.

## Known demo limits

- No replay: watchers see counts from the moment they connect; a watcher
  that connects after the group ended gets only the terminal event.
- A watcher on a request-id that never existed waits forever (the service
  cannot distinguish "unknown" from "not started yet").
- Sleep-mode counters self-bound at 300 counts (~5 min); burn-mode at a
  fixed iteration count, so an uncancelled burn also terminates eventually.
- Burn mode pins one blocking thread per counter at 100% CPU until
  cancelled — that is the point of the demo, but mind your laptop fans.
