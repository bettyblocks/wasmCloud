# wash-precompile

Standalone worker with a frozen two-flag CLI:

```
wash-precompile --image <oci-ref> --output <scheme-url>
```

Pulls a wasm component from an OCI registry, precompiles it via `wasmtime::Engine::precompile_component`, and writes the resulting `.cwasm` bytes to the URL given by `--output` (`nats://` for production, `file://` for tests and local dev).

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

Should print clap's usage screen with `--image` and `--output` and exit 0.

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

## Content-addressed output keys

The operator can key the output by the sha256 of the `.wasm` (the OCI layer digest), so that every Artifact with the same bytes shares one object and the operator can skip the Job when it exists:

```
<base>/sha256/<hex>/<target>-<wasmtime version>-<engine profile>.cwasm
```

When the output key has a `sha256/<hex>/` segment, the worker checks two things before it writes:

- the pulled bytes hash to `<hex>`, and
- `<engine profile>` is the profile of this worker.

If either check fails the worker exits non-zero and writes nothing. A key with a `sha256/` segment that does not have this shape is an error. Other (legacy) keys are not checked.

The engine profile is a 16-character hex name for the engine settings this build compiles with (target, epoch interruption, native unwind info). Print it with:

```bash
wash-precompile --print-engine-profile
```

Set the operator's `-precompile-engine-profile` to this value for the worker image it runs.
