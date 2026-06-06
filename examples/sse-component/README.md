# SSE Component

A WASM-component-only Server-Sent Events broker. Many HTTP clients connect to the
same `scope_key`; a single publish — either via HTTP or via the WIT messaging
plugin — fans out to all of them.

## Pieces

| Component | Role |
| --- | --- |
| `sse-service` | Long-running TCP server on `127.0.0.1:9090`. Tracks SSE connections by `scope_key`, replays missed events, broadcasts pushes. |
| `http-gateway` | WASI HTTP component. Proxies SSE/`/push`/`/connections` to `sse-service`. Also exposes `/publish` (see below). |
| `nats-bridge` | Subscribes to `wasmcloud:messaging/handler` and forwards each message to `sse-service`'s `/push` using the NATS subject as the `scope_key`. |
| `test-publisher` | Standalone WASI HTTP component that publishes via WIT messaging. Not used in the `wash dev` flow — the gateway's own `/publish` route replaces it there. |

## Running with `wash dev` (no Kubernetes, no external NATS)

This is the fastest way to demo the fan-out. It runs the whole stack in one
process and uses the in-memory `wasmcloud:messaging` plugin instead of a real
NATS broker, so external `nats pub` doesn't apply — publish via HTTP instead.

### Prereqs

- Rust toolchain with the `wasm32-wasip2` target:
  ```bash
  rustup target add wasm32-wasip2
  ```
- `wash` CLI from this repo.

### Start the host

```bash
cd examples/sse-component
wash dev
```

This reads `.wash/config.yaml`, builds the workspace to
`target/wasm32-wasip2/release/`, runs `sse-service.wasm` as the workload's
service, loads `http-gateway` (main HTTP component) and `nats-bridge` (messaging
subscriber), and serves HTTP on `http://0.0.0.0:8000`.

### Demo: multiple terminals on the same stream

In **three separate terminals**, subscribe to the same `scope_key`:

```bash
curl -N -H "Accept: text/event-stream" http://localhost:8000/events.demo
```

Each `curl` hangs open. Confirm the registrations:

```bash
curl http://localhost:8000/connections/events.demo
```

In a fourth terminal, **publish** — this substitutes for `nats pub` under
`wash dev`:

```bash
curl -X POST http://localhost:8000/publish \
  -H 'Content-Type: application/json' \
  -d '{"subject":"events.demo","data":"hello fan-out"}'
```

All three subscriber terminals print the same `data: hello fan-out` event line
within milliseconds. The flow is:

```
POST /publish (gateway)
  → consumer::publish (WIT)
  → InMemoryMessaging plugin
  → nats-bridge::handle_message
  → TCP /push on sse-service:9090
  → broadcast to every SSE client on scope_key "events.demo"
```

### Demo: per-scope isolation

In a fifth terminal, subscribe to a different subject:

```bash
curl -N -H "Accept: text/event-stream" http://localhost:8000/events.other
```

Publishing to `events.demo` shows up in terminals 1–3 only. Publishing to
`events.other` shows up only in terminal 5.

### Skipping the bridge: direct `/push`

You can also push straight to the sse-service via the gateway, bypassing the
WIT messaging path:

```bash
curl -X POST http://localhost:8000/push \
  -H 'Content-Type: application/json' \
  -d '{"scope_key":"events.demo","data":"hello direct"}'
```

This is useful for debugging whether an issue lives in the bridge or in the
SSE service.

