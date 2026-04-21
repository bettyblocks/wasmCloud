# Execution Plan: Pre-Compiler Pipeline

Companion to `PRECOMPILER_PLAN.md`. That document answers *what* to build; this one answers *how to execute it* in this repo — commit-sized steps, verification commands, dependencies, merge checkpoints.

Branch: `poc/pre-compiler`.

## Toolchain prereqs

Before starting, confirm your environment can run the existing codegen paths:

```bash
# Protobuf codegen (runs bufbuild/buf in docker)
make proto

# CRD + deepcopy codegen (Go, controller-gen)
cd runtime-operator && make manifests generate

# Tests
cargo test -p wash-runtime
cd runtime-operator && make test

# Chart render sanity check
make helm-render
```

Install KEDA in your dev cluster now (only needed when you reach M4, but good to have ready):

```bash
helm repo add kedacore https://kedacore.github.io/charts
helm install keda kedacore/keda --namespace keda --create-namespace
```

## Milestones

- **M1** — wash-runtime trait layer. Pure Rust. No behavior change. Can merge standalone.
- **M2** — CRD, proto, WD controller extensions. Hand-populatable end-to-end. Can merge standalone.
- **M3** — Worker + reconciler fan-out. Real pipeline, tested locally.
- **M4** — Chart + KEDA. Deployable.
- **M5** — Polish. Metrics, docs, example.

Each milestone is a reasonable PR boundary. Merge M1 before starting M3; M2 and M3 can partly overlap once M2's proto changes are in.

---

## M1 — wash-runtime trait layer

**Goal:** Introduce `ArtifactStore` and `ComponentLoader` traits, NATS `ArtifactStore` impl, and plumb `ComponentLoader` into the `Host`. Behavior stays unchanged — when `precompiled[]` is empty (always, at this milestone), the loader falls back to the existing `Component::new` path.

### M1.1 — Scaffold the trait modules (no logic)

**Files (new):**
- `crates/wash-runtime/src/artifact_store/mod.rs`
- `crates/wash-runtime/src/component_loader/mod.rs`

**Files (modified):**
- `crates/wash-runtime/src/lib.rs` — add `pub mod artifact_store;` and `pub mod component_loader;`
- `crates/wash-runtime/Cargo.toml` — add `url = { workspace = true }` (already in workspace) and `twox-hash = "1"` if needed for the compat-hash finalization; add `artifact-store-nats` feature, default on

**What to write:**
- `ArtifactStore` trait + `ArtifactStoreRegistry` (scheme → impl map) with builder. See the design plan's code sketch.
- `ComponentLoader` trait, `ComponentSource`, `PrecompiledVariant` types. No default impl yet.

**Verify:**
```bash
cargo build -p wash-runtime
cargo test -p wash-runtime --lib
```

**Commit:** `feat(wash-runtime): scaffold ArtifactStore and ComponentLoader traits`

### M1.2 — `DefaultComponentLoader`

**Files (modified):**
- `crates/wash-runtime/src/component_loader/mod.rs`

**What to write:**
- `DefaultComponentLoader` struct (holds `ArtifactStoreRegistry`, `target: String`, `wasmtime_version: String`).
- `compat_hash(&Engine) -> String` helper that hashes `Engine::precompile_compatibility_hash()` to a stable hex string.
- `impl ComponentLoader for DefaultComponentLoader` — try each precompiled variant, match on `(target, wasmtime_version, compat_hash)`, fetch via registry, `unsafe Component::deserialize`; on no match, fall back to `fallback_bytes` + `Component::new`.
- Safety comment on the `unsafe` block calling out: bytes produced by worker built from the same Cargo workspace, matched on compat hash, admission-gated via Artifact CRD.

**Verify:**
```bash
cargo test -p wash-runtime --lib component_loader
```

Write these unit tests:
- `load_picks_matching_variant` — mock store returns known bytes, loader picks the right variant by (target, wasmtime_version, compat_hash).
- `load_falls_back_when_no_variant_matches` — empty variants list + fallback_bytes set → Component::new path exercised.
- `load_errors_when_no_variant_and_no_fallback` — both empty → explicit error.

The mock `ArtifactStore` impl lives in a `#[cfg(test)]` module.

**Commit:** `feat(wash-runtime): implement DefaultComponentLoader with fallback`

### M1.3 — NATS `ArtifactStore` impl

**Files (new):**
- `crates/wash-runtime/src/artifact_store/nats.rs`

**Files (modified):**
- `crates/wash-runtime/src/artifact_store/mod.rs` — `#[cfg(feature = "artifact-store-nats")] pub mod nats;`

**What to write:**
- `NatsArtifactStore` holding an `async_nats::jetstream::Context`.
- `fetch` parses `nats://bucket/object`, opens the object store, reads to end, returns `Bytes`.

**Verify:**
```bash
cargo build -p wash-runtime --features artifact-store-nats
```

Integration test (new file `crates/wash-runtime/tests/artifact_store_nats.rs`):
- testcontainers NATS (already a dev-dep), put bytes into a bucket, `NatsArtifactStore::fetch(url)`, assert round-trip.

**Commit:** `feat(wash-runtime): add NATS JetStream Object Store backend`

### M1.4 — Wire `ComponentLoader` into `Host`

**Files (modified):**
- `crates/wash-runtime/src/host/mod.rs`
- `crates/wash-runtime/src/engine/workload.rs`
- `crates/wash-runtime/src/types.rs`

**What to write:**

1. In `types.rs`, extend internal `Component` and `Service`:
   ```rust
   pub struct Component {
       // ...existing...
       pub precompiled: Vec<PrecompiledVariant>,
   }
   ```
   `PrecompiledVariant` is re-exported from `component_loader`.

2. In `host/mod.rs`:
   - Add `component_loader: Arc<dyn ComponentLoader>` to `Host`.
   - Add `HostBuilder::with_component_loader(...)` and `with_artifact_stores(...)`.
   - In `HostBuilder::build()`, default-construct a `DefaultComponentLoader` if none was set, with:
     - Target triple from `std::env::consts::ARCH` + `std::env::consts::OS` (or a small helper that assembles the canonical triple).
     - Wasmtime version from a build-script-set constant or `env!("CARGO_PKG_VERSION")` of the pinned wasmtime crate (see M1.5).
     - Empty registry if no stores registered.

3. In `engine/workload.rs`:
   - Find every `Component::new(engine, bytes)` call.
   - For each, convert to:
     ```rust
     let source = ComponentSource {
         name: /* from existing context */,
         oci_image: /* from existing context */,
         digest: /* from existing Component.digest */,
         precompiled: component.precompiled.clone(),
         fallback_bytes: Some(component.bytes.clone()),
     };
     let component = host.component_loader.load(&source, engine).await?;
     ```

**Verify:**

```bash
cargo test -p wash-runtime
```

All existing tests should pass unchanged. The `can_run_engine` test in `lib.rs:35` exercises the new path with empty `precompiled` and should succeed.

**Commit:** `feat(wash-runtime): route component loading through ComponentLoader`

### M1.5 — Wasmtime version + target constants

**Files (new):**
- None, add to existing `crates/wash-runtime/build.rs`

**What to write:**
Emit build-time constants so `DefaultComponentLoader` has a single source of truth:
```rust
// in build.rs
let wasmtime_version = env::var("DEP_WASMTIME_VERSION")
    .unwrap_or_else(|_| "unknown".into());
println!("cargo:rustc-env=WASH_WASMTIME_VERSION={wasmtime_version}");
```

If wasmtime doesn't emit `DEP_WASMTIME_VERSION`, parse it from `Cargo.toml` via the build script. Fallback: hardcode "43" with a TODO — the compat hash will catch real drift.

**Verify:**
```bash
cargo build -p wash-runtime
# Confirm the constant is read correctly in tests
```

**Commit:** `chore(wash-runtime): expose wasmtime version to loader via build script`

### M1 checkpoint

At this point:
- New traits exist, integrated into `Host`.
- Behavior is identical to before (all paths go through `fallback_bytes`).
- NATS store works, unit-tested.
- Integration test exists but isn't exercised by the runtime yet.

**Merge M1 to `poc/pre-compiler`** before starting M2.

---

## M2 — CRD, proto, WD controller extensions

**Goal:** Schema and dispatch layer changes. After this milestone, you can manually patch `Artifact.status.precompiled[]` with a NATS URL pointing at a hand-produced `.cwasm`, apply a Strict `WorkloadDeployment`, and observe the host loading it without compile.

### M2.1 — Extend `ArtifactStatus` + add condition

**Files (modified):**
- `runtime-operator/api/runtime/v1alpha1/artifact_types.go`

**What to write:**
```go
type PrecompiledVariant struct {
    Target          string `json:"target"`
    WasmtimeVersion string `json:"wasmtimeVersion"`
    CompatHash      string `json:"compatHash"`
    ArtifactURL     string `json:"artifactUrl"`
    Digest          string `json:"digest"`
}

type ArtifactStatus struct {
    // ...existing...
    Precompiled []PrecompiledVariant `json:"precompiled,omitempty"`
}

const ArtifactConditionPrecompiled condition.ConditionType = "Precompiled"
```

**Regenerate + verify:**
```bash
cd runtime-operator && make manifests generate
# Confirm charts/runtime-operator/crds/runtime.wasmcloud.dev_artifacts.yaml was updated
git diff charts/runtime-operator/crds/runtime.wasmcloud.dev_artifacts.yaml
```

Run envtest to confirm the updated CRD still loads:
```bash
cd runtime-operator && make test
```

**Commit:** `feat(operator): add PrecompiledVariant to Artifact status`

### M2.2 — Extend `WorkloadDeploymentSpec`

**Files (modified):**
- `runtime-operator/api/runtime/v1alpha1/workloaddeployment_types.go`

**What to write:**
```go
type PrecompileMode string

const (
    PrecompileModeFallback PrecompileMode = "Fallback"
    PrecompileModeStrict   PrecompileMode = "Strict"
)

type WorkloadDeploymentSpec struct {
    // ...existing...

    // +kubebuilder:default=Fallback
    // +kubebuilder:validation:Enum=Fallback;Strict
    PrecompileMode PrecompileMode `json:"precompileMode,omitempty"`

    // +kubebuilder:validation:Optional
    HostPoolTargets []string `json:"hostPoolTargets,omitempty"`
}

type WorkloadDeploymentArtifact struct {
    // +kubebuilder:validation:Optional
    ArtifactFrom *corev1.LocalObjectReference `json:"artifactFrom,omitempty"`

    // +kubebuilder:validation:Optional
    Image string `json:"image,omitempty"`

    // +kubebuilder:validation:Optional
    ImagePullSecret *corev1.LocalObjectReference `json:"imagePullSecret,omitempty"`
}
```

Add a CEL validation if your kubebuilder version supports it on the struct:
```go
// +kubebuilder:validation:XValidation:rule="has(self.artifactFrom) != has(self.image)",message="exactly one of artifactFrom or image must be set"
```

**Regenerate + verify:**
```bash
cd runtime-operator && make manifests generate
git diff charts/runtime-operator/crds/runtime.wasmcloud.dev_workloaddeployments.yaml
```

Apply a test WD to a kind cluster to confirm the validation fires:
```bash
kubectl apply -f - <<EOF
apiVersion: runtime.wasmcloud.dev/v1alpha1
kind: WorkloadDeployment
metadata: { name: bad, namespace: default }
spec:
  artifacts:
    - artifactFrom: { name: foo }
      image: ghcr.io/x:v1      # both set — should fail
EOF
```

**Commit:** `feat(operator): add precompile mode + inline image support to WorkloadDeployment`

### M2.3 — Extend proto

**Files (modified):**
- `proto/wasmcloud/runtime/v2/workload.proto`

**What to write:**
```proto
message PrecompiledVariant {
  string target = 1;
  string wasmtime_version = 2;
  string compat_hash = 3;
  string artifact_url = 4;
  string digest = 5;
}

message Component {
  // ...existing fields 1-7...
  repeated PrecompiledVariant precompiled = 8;
}

message Service {
  // ...existing fields 1-5...
  repeated PrecompiledVariant precompiled = 6;
}
```

**Regenerate + verify:**
```bash
make proto
git diff proto/ runtime-operator/pkg/rpc/ crates/wash-runtime/src/  # Rust proto is build.rs-generated, check build output
cargo build -p wash-runtime
cd runtime-operator && go build ./...
```

**Commit:** `feat(proto): add PrecompiledVariant to Component and Service dispatch messages`

### M2.4 — Rust proto → `ComponentSource` mapping

**Files (modified):**
- Whichever module in `crates/wash-runtime/` currently converts the dispatched `Component` proto into the internal `types::Component`. Grep for `proto_to_component` or similar.

**What to write:**
- Map the new `precompiled` proto field into internal `Component.precompiled` (which is `Vec<PrecompiledVariant>` from M1).
- The conversion from internal `Component` into `ComponentSource` (in `engine/workload.rs`) picks up `precompiled` automatically from M1.4.

**Verify:**
```bash
cargo test -p wash-runtime
```

**Commit:** `feat(wash-runtime): wire precompiled variants from dispatch proto into loader`

### M2.5 — WD auto-create helper

**Files (modified):**
- `runtime-operator/internal/controller/runtime/workload_deployment_controller.go`

**What to write:**

Helper method:
```go
func (r *WorkloadDeploymentReconciler) resolveOrUpsertArtifact(
    ctx context.Context,
    deployment *runtimev1alpha1.WorkloadDeployment,
    a *runtimev1alpha1.WorkloadDeploymentArtifact,
) (string, error) {
    if a.ArtifactFrom != nil {
        return a.ArtifactFrom.Name, nil
    }
    name := "auto-" + shortHashImage(a.Image, a.ImagePullSecret)
    artifact := &runtimev1alpha1.Artifact{
        ObjectMeta: metav1.ObjectMeta{
            Name:      name,
            Namespace: deployment.Namespace,
            Labels:    map[string]string{"runtime.wasmcloud.dev/auto-created": "true"},
        },
        Spec: runtimev1alpha1.ArtifactSpec{
            Image:           a.Image,
            ImagePullSecret: a.ImagePullSecret,
        },
    }
    if err := r.Create(ctx, artifact); err != nil && !apierrors.IsAlreadyExists(err) {
        return "", err
    }
    return name, nil
}

func shortHashImage(image string, pullSecret *corev1.LocalObjectReference) string {
    h := sha256.New()
    h.Write([]byte(image))
    if pullSecret != nil { h.Write([]byte(pullSecret.Name)) }
    return hex.EncodeToString(h.Sum(nil))[:12]
}
```

Update `reconcileArtifacts` to call it:
```go
for _, configArtifact := range deployment.Spec.Artifacts {
    name, err := r.resolveOrUpsertArtifact(ctx, deployment, &configArtifact)
    if err != nil { return err }
    // ...existing Get + Published check, using `name`
}
```

Also update `resolveArtifacts` (line 291) to call the same helper.

**Verify (unit test in `workload_deployment_controller_test.go`):**
- WD with only `image:` set → after reconcile, an Artifact named `auto-<hash>` exists in the same namespace with the `auto-created` label.
- Two WDs with the same `image:` → converge on a single Artifact (no error).
- WD with only `artifactFrom:` → no Artifact is created.

**Commit:** `feat(operator): auto-upsert Artifact when WorkloadDeployment inlines an image`

### M2.6 — WD Strict mode gate

**Files (modified):**
- `runtime-operator/internal/controller/runtime/workload_deployment_controller.go`

**What to write:**

Extend `reconcileArtifacts` after the Published check:
```go
mode := deployment.Spec.PrecompileMode
if mode == "" { mode = runtimev1alpha1.PrecompileModeFallback }

if mode == runtimev1alpha1.PrecompileModeStrict {
    targets := deployment.Spec.HostPoolTargets
    if len(targets) == 0 {
        targets = r.DefaultHostPoolTargets  // from operator config
    }
    for _, target := range targets {
        if !hasPrecompiledVariantForTarget(artifact, target) {
            return condition.ErrStatusUnknown(fmt.Errorf(
                "artifact %s not precompiled for target %s", name, target))
        }
    }
}
```

Add `DefaultHostPoolTargets` field to `WorkloadDeploymentReconciler`, wired from operator config (env var or flag in `cmd/main.go`; default to `["x86_64-linux-gnu"]`).

`hasPrecompiledVariantForTarget` is a trivial helper.

**Verify (unit test):**
- Strict WD with Artifact that has no variants → stays Pending.
- Strict WD with Artifact that has the matching variant → proceeds past the Artifact condition.
- Fallback WD (default) with no variants → proceeds.

**Commit:** `feat(operator): gate WorkloadDeployment on precompile readiness in Strict mode`

### M2.7 — Dispatch variant injection

**Files (modified):**
- `runtime-operator/internal/controller/runtime/workload_deployment_controller.go` (`resolveArtifacts`)

**What to write:**

When building the dispatch template, copy `artifact.Status.Precompiled` into the template's Component/Service fields (proto-generated Go types now have the new `Precompiled` field from M2.3).

**Verify:**
Apply a WD with an Artifact that has hand-populated status.precompiled → inspect the dispatched payload (log or proto dump) and confirm variants are present.

**Commit:** `feat(operator): inject precompiled variants into dispatch template`

### M2 checkpoint

Manual end-to-end test:

1. Run NATS locally and create the `wasmcloud-cwasm` object store.
2. Hand-produce a `.cwasm` for a test component (short Rust script that calls `Engine::precompile_component`).
3. Put the `.cwasm` into NATS at a known URL.
4. Apply an Artifact CR, then hand-patch its status:
   ```bash
   kubectl patch artifact foo --type=merge --subresource=status -p '{
     "status": {
       "precompiled": [{
         "target": "x86_64-linux-gnu",
         "wasmtimeVersion": "43.0.1",
         "compatHash": "<the hash your host computes>",
         "artifactUrl": "nats://wasmcloud-cwasm/<your-key>.cwasm",
         "digest": "<sha256 of the cwasm>"
       }],
       "conditions": [
         {"type": "Sync", "status": "True", "reason": "Ok"},
         {"type": "Published", "status": "True", "reason": "Ok"},
         {"type": "Precompiled", "status": "True", "reason": "Ok"}
       ]
     }
   }'
   ```
5. Apply a Strict WD referencing the Artifact.
6. Observe the host's logs: `loaded precompiled variant`. Confirm Cranelift did not run (e.g., via a debug log, or by timing — deserialize is ~ms, compile is seconds).

**Merge M2 to `poc/pre-compiler`** before starting M3. You now have a functional precompile *consumption* path.

---

## M3 — Worker + reconciler fan-out

**Goal:** Replace the manual `.cwasm` production with the automated worker pipeline.

### M3.1 — Scaffold worker crate

**Files (new):**
- `crates/precompile-worker/Cargo.toml`
- `crates/precompile-worker/src/main.rs`
- `crates/precompile-worker/src/job.rs`
- `crates/precompile-worker/src/runner.rs`

**Files (modified):**
- Workspace `Cargo.toml` — add `"crates/precompile-worker"` to `members`.

**What to write:**
- `Cargo.toml` depending on `wash-runtime` (for `Engine` construction), `async-nats`, `tokio`, `tracing`, `tracing-subscriber`, `anyhow`, `serde` / `serde_json`.
- `main.rs`: parse config from env (NATS URL, stream name, consumer name, object store bucket), initialize tracing, connect to NATS, enter the runner loop.
- `job.rs`: `PrecompileJob` and `PrecompileCompletion` types, serde derive.
- `runner.rs`: stub runner that just logs received jobs and ACKs (no compile yet).

**Verify:**
```bash
cargo build -p precompile-worker
cargo run -p precompile-worker # should connect to a local NATS and idle
```

**Commit:** `feat(precompile-worker): scaffold worker binary`

### M3.2 — Implement the compile loop

**Files (modified):**
- `crates/precompile-worker/src/runner.rs`

**What to write:**
- Extract engine-config construction into a shared `wash_runtime::engine::config::production_config()` function so worker and host use identical config. Call it from both.
- For each job:
  - Pull `.wasm` bytes from OCI (reuse `wash_runtime::oci`).
  - Construct `Engine` via the shared config.
  - `engine.precompile_component(&wasm_bytes)` → `.cwasm` bytes.
  - Compute `sha256` digest of `.cwasm`.
  - Put to NATS object store at key `{digest}-{target}-{wasmtime_version}.cwasm`.
  - Publish a `PrecompileCompletion` event on `wasmcloud.precompile.completion.{namespace}.{name}`.
  - ACK job.

Edge cases:
- Worker's compat hash != job's `requested_compat_hash` (if provided) → NACK with explanatory error.
- OCI pull fails → NACK-terminal, publish a completion event with error reason.
- Put to object store fails → NACK with retry.

**Verify:**

Integration test (`crates/precompile-worker/tests/runner.rs`):
- testcontainers NATS, testcontainers OCI registry (zot or distribution), push a `.wasm`, publish a job, run the worker, assert `.cwasm` appears in the bucket and completion event fires.

**Commit:** `feat(precompile-worker): implement compile + upload pipeline`

### M3.3 — Go precompile queue helpers

**Files (new):**
- `runtime-operator/internal/precompile/queue.go`
- `runtime-operator/internal/precompile/nats/producer.go`
- `runtime-operator/internal/precompile/nats/consumer.go`

**What to write:**
- `queue.go`: `JobProducer`, `CompletionConsumer` interfaces; `PrecompileJob`, `PrecompileCompletion` types (JSON-compatible with the Rust side).
- NATS impls using the existing NATS client plumbing in the operator (check `pkg/wasmbus/`).

**Verify:**
- Unit tests with a mock NATS.
- Integration test: start the worker binary and this producer, verify round-trip.

**Commit:** `feat(operator): NATS-backed precompile job queue helpers`

### M3.4 — Artifact reconciler fan-out

**Files (modified):**
- `runtime-operator/internal/controller/runtime/artifact_controller.go`
- `runtime-operator/cmd/main.go`

**What to write:**

In `artifact_controller.go`:
- Add `Producer precompile.JobProducer` and `TargetMatrix []string` to `ArtifactReconciler`.
- New `reconcilePrecompiled` method (see design plan for sketch).
- Register the condition in `SetupWithManager`.

In `cmd/main.go`:
- Construct the NATS-backed producer.
- Start a goroutine subscribing to `wasmcloud.precompile.completion.>` that patches Artifact.status.precompiled on each completion event.

**Verify:**

Envtest:
- Create an Artifact, assert jobs are submitted for every target.
- Simulate a completion event via the NATS client, assert `status.precompiled[]` is patched and the Precompiled condition flips True.

Manual end-to-end in kind:
- Deploy operator + worker + NATS + a real registry.
- Apply an Artifact, watch the worker logs, see the compile happen, verify Artifact status.

**Commit:** `feat(operator): fan out precompile jobs and track completions`

### M3 checkpoint

Real end-to-end now works: applying an Artifact triggers actual compilation, and `Artifact.status.precompiled[]` populates automatically. Strict WDs block until the worker finishes; Fallback WDs proceed immediately.

**Merge M3 to `poc/pre-compiler`**.

---

## M4 — Chart + KEDA

**Goal:** Turn the worker into a real Deployment with autoscaling, bootstrap the NATS resources via the chart.

### M4.1 — Chart templates

**Files (new):**
- `charts/runtime-operator/templates/precompile/_helpers.tpl`
- `charts/runtime-operator/templates/precompile/deployment.yaml`
- `charts/runtime-operator/templates/precompile/serviceaccount.yaml`
- `charts/runtime-operator/templates/precompile/rbac.yaml`
- `charts/runtime-operator/templates/precompile/scaledobject.yaml`
- `charts/runtime-operator/templates/precompile/bootstrap-job.yaml` (stream + bucket creation)

**Files (modified):**
- `charts/runtime-operator/values.yaml` — add the `precompile:` block from the design plan.
- `charts/runtime-operator/README.md` — KEDA prerequisite note.

All templates guard on `{{- if .Values.precompile.enabled }}`.

**Verify:**
```bash
make helm-render
# Inspect the rendered output
helm template charts/runtime-operator --set precompile.enabled=true | less
```

Lint:
```bash
helm lint charts/runtime-operator
```

**Commit:** `feat(chart): add precompile worker Deployment, ScaledObject, bootstrap`

### M4.2 — Docker image for worker

**Files (new):**
- `crates/precompile-worker/Dockerfile`
- `.github/workflows/precompile-worker.yaml` (or wherever existing image CI lives — grep for existing wash-runtime image workflow)

**Files (modified):**
- `Makefile` — add `precompile-worker-image` target alongside the existing image builds.

**What to write:**
- Multi-stage Dockerfile, cargo-chef style if the existing crates use it.
- CI workflow mirroring the existing runtime-operator image build.

**Verify:**
```bash
make precompile-worker-image
docker run --rm ghcr.io/wasmcloud/precompile-worker:dev --help
```

**Commit:** `ci(precompile-worker): build and publish container image`

### M4.3 — End-to-end kind test

**Files (new):**
- `test/e2e/precompile/precompile_test.go` or equivalent in the existing e2e harness.

**What to write:**
- kind cluster bootstrap, KEDA install, chart install with `precompile.enabled=true`.
- Apply a sample Artifact + Strict WD.
- Assert WD reaches Running; assert host never invoked Cranelift (metric or log assertion).
- Assert KEDA scales workers on burst.

**Verify:**
```bash
cd runtime-operator && make test-e2e
```

**Commit:** `test(e2e): end-to-end precompile pipeline in kind`

### M4 checkpoint

Production-shaped deploy. **Merge M4 to `poc/pre-compiler`**, then evaluate whether `poc/pre-compiler` itself is ready to merge to `main` (or upstream).

---

## M5 — Polish

Not strictly required for a working POC. Ship incrementally.

### M5.1 — Metrics

Emit from worker:
- `precompile_jobs_total{status=ok|error}`
- `precompile_duration_seconds` histogram
- `precompile_artifact_bytes` gauge

Emit from host's `ComponentLoader`:
- `component_loader_hits_total{source=precompiled|fallback}`
- `component_loader_fetch_duration_seconds`

The host already has a `Meters` type (`src/observability.rs`); extend it.

### M5.2 — Docs

- `docs/precompile.md` — architecture, mode comparison, troubleshooting.
- Link from main README.

### M5.3 — Example

- `examples/precompile-demo/` — a complete working example with an Artifact + WD.

### M5.4 — Orphan cleanup controller

- New small controller that periodically scans for `auto-created=true` Artifacts with no active references and deletes them after a TTL.

---

## Parallelization notes

If more than one person works on this:

- M1 is sequential by necessity (everything else depends on it).
- Once M2.3 (proto changes) is merged, M2 and M3 can proceed in parallel — the proto is the only real coupling point.
- M4 chart work can start as soon as M3.1/M3.2 produce a runnable worker binary; don't wait for the reconciler.
- M5 items are all independent.

## Risk and rollback

Every milestone is additive to existing behavior. Rollback per milestone:

- **M1**: set no `ComponentLoader` → builder default kicks in → empty `precompiled[]` means fallback always → zero behavior change vs. today.
- **M2**: leave WorkloadDeployments on `precompileMode: Fallback` (default). Leave Artifact `status.precompiled[]` empty. Everything behaves as before M1.
- **M3**: disable the precompile controller loop in the operator (a feature flag) → no jobs submitted → artifacts never precompile → WDs stay on Fallback → behavior as before M1.
- **M4**: `precompile.enabled: false` in values.yaml → chart doesn't render precompile resources.

No step changes existing APIs. All additions are optional, all defaults preserve current behavior.

---

## Key commands reference

| Purpose | Command |
|---|---|
| Build wash-runtime | `cargo build -p wash-runtime` |
| Test wash-runtime | `cargo test -p wash-runtime` |
| Build worker | `cargo build -p precompile-worker` |
| Regenerate proto | `make proto` |
| Regenerate CRD + deepcopy | `cd runtime-operator && make manifests generate` |
| Operator unit + envtest | `cd runtime-operator && make test` |
| Operator e2e | `cd runtime-operator && make test-e2e` |
| Chart render | `make helm-render` |
| Kind dev cluster | `make kind-setup` |
| Chart install | `make helm-install` |
