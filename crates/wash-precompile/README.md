# wash-precompile

Standalone worker with a frozen two-flag CLI:

```
wash-precompile --image <oci-ref> --output <scheme-url>
```

Pulls a wasm component from an OCI registry, precompiles it via `wasmtime::Engine::precompile_component`, and writes the resulting `.cwasm` bytes to the URL given by `--output` (`nats://` for production, `file://` for tests and local dev).

With exactly one `--image/--output` pair and no `--batch-file`, the behaviour (output, error text, exit code 1 on any error) is the same as before. The operator depends on this.

## Batch mode

One process can precompile many components:

```
wash-precompile --image <ref-a> --output <url-a> --image <ref-b> --output <url-b>
wash-precompile --batch-file <path>          # one `<image> <output>` pair per line
```

`--image/--output` pairs are matched by position; the items of `--batch-file` come after them. Blank lines and lines starting with `#` are ignored.

- All items share one wasmtime engine, one registry client and one NATS connection.
- At most `--concurrency` (env `PRECOMPILE_CONCURRENCY`, default: the CPUs available to the process) compiles run at the same time. Pulls and writes of up to twice that many items overlap with them.
- A failed item does not stop the others.
- Each finished item prints one tab-separated line to stdout:
  `ok <index> <image> <output> <ms>ms` or `fail <index> <image> <output> permanent|retryable <error>`.
- At the end a JSON summary is written to `--termination-log` (env `PRECOMPILE_TERMINATION_LOG`, default `/dev/termination-log`), so Kubernetes shows it as the container's termination message. It is kept under the 4096-byte kubelet limit by shortening the error texts:

  ```json
  {"total":3,"ok":[0,2],"failed":[{"index":1,"permanent":true,"error":"failed to pull ..."}]}
  ```

  When the batch cannot start (for example an invalid batch file) the summary is `{"error":"...","permanent":true}`.

Exit codes in batch mode:

| Code | Meaning |
| ---- | ------- |
| 0 | every item succeeded |
| 1 | at least one item failed, and at least one of the failures may pass on a retry (network, auth, rate limit, NATS) |
| 2 | every failure is permanent: invalid wasm, image or tag not found, invalid reference, invalid output URL, or invalid input. clap also uses 2 for a usage error |

A Kubernetes `podFailurePolicy` can use exit code 2 to fail an index without retrying it.

The operator-side `PrecompileReconciler` constructs Jobs that invoke this binary.

## Build the binary

```bash
cargo build -p wash-precompile --release
```

The release binary lands at `target/release/wash-precompile`.

## Build the container image

The Dockerfile builds a static musl binary with `cargo-chef` and packages it into a `chainguard/wolfi-base` image. Build context is the workspace root:

```bash
docker build -f crates/wash-precompile/Dockerfile -t wash-precompile:dev .
```

Override the tag for pushing to a registry:

```bash
docker build -f crates/wash-precompile/Dockerfile \
    -t ghcr.io/your-org/wash-precompile:v0.1.0 .
```

Smoke-test the image:

```bash
docker run --rm wash-precompile:dev --help
```

Should print clap's usage screen with `--image`, `--output` and the batch flags and exit 0.

## Run locally against a `file://` output

```bash
cargo run -p wash-precompile -- \
    --image ghcr.io/wasmcloud/components/http-hello-world-rust:0.1.0 \
    --output file:///tmp/out.cwasm
```

For `nats://`, set `NATS_URL`. For pulling from a private registry, set `DOCKER_CONFIG` pointing at a directory containing a `config.json` (the same format `kubectl create secret docker-registry` produces).

## Insecure registries

By default images are pulled over HTTPS. To pull from a registry that only speaks plain HTTP (e.g. a local dev registry), list its host in `INSECURE_REGISTRIES` (comma-separated):

```bash
INSECURE_REGISTRIES=localhost:5000 cargo run -p wash-precompile -- \
    --image localhost:5000/http-hello-world:0.1.0 \
    --output file:///tmp/out.cwasm
```

Registries are matched by host exactly as they appear in the OCI reference; anything not listed is pulled over HTTPS.

## Tests

```bash
# Unit tests
cargo test -p wash-precompile

# Integration tests (requires Docker for testcontainers NATS)
cargo test -p wash-precompile -- --ignored
```
