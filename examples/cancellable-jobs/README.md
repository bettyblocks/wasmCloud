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
curl ──GET /events/<id>──▶ frontend ──"watch <id>" TCP──▶ sse-service (pinned, P3)
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

Ramps on a verified-fresh host per level, 10s windows, sleep mode
(healthy = ~10 events/s/conn, all 10 counters distinct).

The sse-service was originally a WASI 0.2 (wstd) component. After its
throughput fix (buffered line reads + per-watcher queues) it was flawless
to 12 groups, then **collapsed hard at exactly 13** (~1 event/conn,
silent, CPU-count independent). That wall was root-caused (2026-06-11)
to scheduling starvation in the guest executor, inherent to the WASI 0.2
poll model — see "the 0.2 wall" below — and fixed (2026-06-12) by
rewriting the service as a P3 component. Same protocol, same group/
watcher logic; only the runtime stack changed. Measured after the
rewrite:

| groups (×10 counters) | result |
|---|---|
| 13 / 20 / 32 / 50 | all conns healthy (~10/s each) |
| 62 | admission stops: pooling allocator's default 1000 core-instance quota (loud `failed to instantiate` errors; the 61 admitted groups stay perfectly healthy) |
| 80 / 150 (pool raised via `WASMTIME_POOLING_TOTAL_*` env) | all conns healthy — 150 groups = 1,500 concurrent counters, 16,350 events/10s |
| ~220 | `/create` latency exceeds 10s as the box saturates (graceful, loud) |

The failure modes after the rewrite are exactly what you want: a
configurable resource quota at admission time and CPU saturation —
linear, observable, tunable — instead of a silent scheduling cliff.

### The 0.2 wall, for the record

Evidence chain (all on the wstd version): counters stayed healthy through
the wedge; all 16 tokio workers idle-parked (not thread exhaustion; CPU
low); epoch, pool limits, and loopback mutexes ruled out; the service
kept *receiving* counts but each watcher task delivered exactly one frame
and never ran again; adding an eprintln per received message (= extra
reactor turns) partially restored delivery — a scheduling Heisenbug.

Mechanism: the WASI 0.2 poll ABI is stateless — every wait hands the host
the FULL pollable list, which rebuilds a readiness future per entry — and
wstd's reactor additionally runs that full-list check after nearly every
task turn. Per-turn cost is O(connections), turns scale with message rate
(10·N/s), so the single-threaded service's work grows ~O(N²); at 13
groups (~144 pollables) the one fiber's budget is exceeded and tasks
woken by guest-internal channels (the watcher queues) starve. Under P3
each task waits on its own waitable, host-side — there is no guest poll
list, so this cost structure does not exist. The ramp above confirms it.

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
