# cancellable-counter

A minimal end-to-end demo of **per-invocation cancellation** built on epoch
interruption. Start some work, cancel it from a separate request, and watch
the work stop — proven by a progress count that freezes plus a `cancelled`
status. See the design in
[`crates/wash-runtime/docs/decisions/001-mvp-cancelling-epochs.md`](../../crates/wash-runtime/docs/decisions/001-mvp-cancelling-epochs.md).

## The pieces

Two P3 (component-model async) components, plus the host `JobsPlugin`
(`crates/wash/src/jobs_plugin.rs`, already registered by `wash dev`):

- **frontend** — the HTTP front door. Three routes:
  - `POST /do-work?mode=tick|cpu` → mints an id, `register`s **this
    invocation's own** cancellation handle under it, starts **one** linked
    call into the counter, and returns the id **immediately** (async submit).
    The linked call stays in flight inside this store, which is what keeps
    the detached store alive while the counter runs.
  - `POST /cancel?id=<id>` → trips the registered handle. The runtime's epoch
    callback reads it on the next ~10ms tick and traps the work's store; the
    plugin writes the terminal `cancelled` status.
  - `GET /status?id=<id>` → reads the plugin's status line for the id.
- **counter** — the work. An async-lifted `runner.run` export reporting
  progress to the plugin. `tick` counts to ten, one per second; `cpu` is a
  pure CPU burn with no progress reports.

The plugin owns each id's lifecycle: the cancellation handle plus a
`{ status, count }` record. The trapped work can never write its own
terminal state, so `cancel` writes `cancelled` from the live canceller
invocation — and the `count` is left frozen wherever the work stopped, which
is the proof it actually stopped.

## Run it

```sh
wash dev
```

In another terminal (HTTP ingress is on :8000):

### tick mode — progress freezes

```sh
id=$(curl -s -XPOST 'localhost:8000/do-work?mode=tick'); echo "id=$id"
sleep 3
curl -s -XPOST "localhost:8000/cancel?id=$id"   # -> true
curl -s "localhost:8000/status?id=$id"          # -> cancelled count=3
```

Let one run uncancelled and it reaches `finished count=10` after ~10s.

### cpu mode — runaway loop trapped by epoch

```sh
id=$(curl -s -XPOST 'localhost:8000/do-work?mode=cpu'); echo "id=$id"
sleep 3
curl -s -XPOST "localhost:8000/cancel?id=$id"   # -> true
curl -s "localhost:8000/status?id=$id"          # -> cancelled count=0
```

The CPU loop makes no host calls, so nothing but epoch interruption can stop
it — and it does, trapping the running wasm within ~10ms of the cancel.
Uncancelled, it eventually reaches `finished count=0`.

## What each mode shows

| | tick | cpu |
|---|---|---|
| observable proof | `count` freezes mid-climb (e.g. 3) | `finished` never appears (count stays 0) |
| how cancel lands | when the 1s sleep returns / next epoch tick (≤1s) | epoch traps the running wasm mid-loop (~10ms) |
| why it matters | cancel granularity = host-call cadence (the I/O blind spot) | epoch is the *only* thing that can stop a host-call-free loop |

## Tenancy

`cancel` and `status` are scoped to the workload that registered the id — a
workload can only touch its own work. Cross-workload cancel would need an
explicit trust model (deliberately out of scope).
