# cancellable-jobs

End-to-end demo of **per-invocation cancellation** (see
`crates/wash-runtime/docs/WORKLOAD_CANCELLATION.md`) as a product flow,
built on the P3 (component-model async) platform features:

- `POST /create` — the **P3 frontend component** registers a request-id,
  spawns **ten concurrent WIT calls** into the counter component
  (`demo:jobs/runner.run`, an async-lifted export wired
  component-to-component by the workload linker), and returns its HTTP
  response **immediately** while the counters keep running — platform
  async submit: the P3 HTTP driver keeps the invocation's store alive
  while the linked calls are in flight.
- Counters report straight to a long-lived **SSE service** over the
  workload's virtual loopback network (the documented component→service
  channel); `GET /events/<id>` pipes that stream into a **live streaming
  response body**.
- `POST /cancel/<id>` trips one cancellation handle — the `/create`
  invocation's store — and the runtime's actuators (host-boundary checks +
  the epoch callback) tear down the frontend's background task **and all
  ten in-flight counter calls together**.
- `POST /create?mode=burn` — the Layer 2 showcase: CPU-bound counters that
  are killed **mid-computation** by epoch interruption (the trap lands
  within milliseconds of cancel).

## Run it

```sh
# from this directory; wash must be built with --features wasip3
wash dev
```

Then, in other terminals:

```sh
ID=$(curl -s -X POST http://127.0.0.1:8000/create)
curl -N http://127.0.0.1:8000/events/$ID          # live counts, 10 counters
curl -X POST http://127.0.0.1:8000/cancel/$ID     # -> true; stream prints
                                                  #    "event: cancelled"
curl -X POST http://127.0.0.1:8000/cancel/$ID     # -> false (idempotent)

# Layer 2 showcase: CPU-burning counters (no counts — they just burn)
ID=$(curl -s -X POST "http://127.0.0.1:8000/create?mode=burn")
curl -N http://127.0.0.1:8000/events/$ID &        # silent until terminal
curl -X POST http://127.0.0.1:8000/cancel/$ID     # -> true; watcher prints
                                                  #    "event: cancelled";
                                                  #    host log shows
                                                  #    "wasm trap: interrupt"
```

## Architecture

```
curl ──POST /create[?mode=burn]─▶ frontend (P3) ── control.register ──▶ JobsPlugin
                                     │                                  (host: id →
                                     │ spawns 10 CONCURRENT WIT calls    cancel handle)
                                     ▼
                              counter component ×10 in-flight calls
                              (async-lifted runner.run, one shared store)
                                     │ "feed/count/done" over virtual loopback TCP
                                     ▼
curl ──GET /events/<id>──▶ frontend ──"watch <id>" TCP──▶ sse-service (pinned, P2 wstd)
        (streaming response body, live via the P3 HTTP driver)

curl ──POST /cancel/<id>──▶ frontend ── control.cancel ──▶ JobsPlugin trips the handle
                                              │
                       runtime actuators read it: host-boundary checks (L1)
                       + epoch callback (L2) ──▶ the /create store traps —
                       frontend bg task + all 10 counter calls die together
```

- **JobsPlugin** (`crates/wash/src/jobs_plugin.rs`) is down to the one
  thing a guest cannot do: the cancellation registry. `register` maps the
  *calling invocation's own* handle under the id (tenancy-scoped to the
  workload); `cancel` trips it. No spawning, no data path.
- **Spawning is guest-side now**: the frontend calls the counter's
  async-lifted export through the runtime's concurrent linked-call
  trampolines — ten suspending calls interleave within one store (this
  used to trap outright: "cannot block a synchronous task").
- **One store = one group**: separate counter *components*, but linked
  calls execute in the `/create` invocation's store — group cancel is a
  single handle, and counters are concurrent, not parallel.

## Measured load ceiling (see `loadtest.sh`)

Ramp on a verified-fresh host per level, 10s windows, sleep mode, after
the sse-service throughput fix (buffered line reads + per-watcher
queues — before the fix, fan-out already degraded at 8 groups):

| groups (×10 counters) | watcher events | verdict |
|---|---|---|
| 1–12 | ~98/conn (~10/s) | perfect |
| 13+ (tested 14/16/18/20/40/…) | ~1/conn, 12 total | hard collapse |

The remaining wall is **not in the demo components**: it sits at exactly
13 concurrent groups, is independent of CPU count (8-core run handles 10
groups perfectly; 16-core collapses at 13), independent of epoch
interruption (verified disabled), and is not pool exhaustion (no
instantiation errors — everything wedges silently, including groups that
were healthy below the threshold). Suspect: cross-store contention in
wasmtime 44's component-async scheduling (the #11869/#11870 family —
each group holds two long-driven P3 stores plus ten in-flight concurrent
linked calls). Runtime correctness holds throughout: register/cancel
work at every level and teardown cancels trap all groups (the
`wasm trap: interrupt` ERROR lines during cleanup are those cancels
working as designed). Follow-up: instrument the host (tokio-console /
custom tracing in the driver) to find what the 13th group blocks on.

## Known demo limits

- Counters in one store are cooperatively scheduled: the burn loop yields
  every ~100M iterations so siblings and the response delivery can
  progress (wasm in a store is single-threaded; a non-yielding CPU loop
  also wedges `task.return` — wasmtime #11869). **Cancellation does not
  depend on those yields** — the epoch trap lands mid-loop.
- The sse-service finalizes a group ("done"/"cancelled") two seconds after
  its last feed drops, to tolerate staggered counter start-up.
- No replay: watchers see counts from the moment they connect.
- Sleep-mode counters self-bound at 300 counts; burn-mode at a fixed
  iteration count.
- The demo's P3 guests build as wasm32-wasip1 core modules componentized
  by `components/builder` (see `build.sh`) — the wasm32-wasip2 toolchain
  cannot link component-model-async guests yet.
