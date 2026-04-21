# Plan: Out-of-Process Component Pre-Compiler

## Context

Today, when a component first lands on a wasmCloud host, wasmtime/Cranelift compiles it in-process on the same host that's running workloads. The compile step is CPU-intensive and the resulting native code lives in anonymous mmap. Both impact co-located workloads: latency spikes during compile, sustained RSS pressure afterwards.

The goal is to split component compilation off the workload host into a dedicated, resource-isolated worker pool. Workload hosts should never invoke Cranelift in steady state — only `Component::deserialize` on pre-produced `.cwasm` bytes.

### Shape

- **One singleton reconciler** (the existing `ArtifactReconciler`, extended) watches Artifact CRs and fans out compile jobs to a NATS JetStream work queue.
- **A KEDA-scaled worker Deployment** (1–10 replicas) consumes the queue, runs `Engine::precompile_component`, pushes `.cwasm` bytes to a NATS JetStream Object Store, and publishes completion events.
- **The Artifact reconciler** watches completions and patches `Artifact.status.precompiled[]` with variant references.
- **The WorkloadDeployment reconciler** (existing) gates on precompile readiness (`Strict` mode) or passes through (`Fallback` mode, default) and injects variant references into the dispatch payload.
- **The host's new `ComponentLoader`** picks the variant matching its own `(target, wasmtime_version, compat_hash)`, fetches `.cwasm` via a pluggable `ArtifactStore`, deserializes. On miss (Fallback) it compiles inline.

### Scope of this plan

- Everything above.
- The WorkloadDeployment ergonomics change: allow inlining an image (`spec.artifacts[].image`) and auto-upsert an Artifact for it.
- Chart plumbing: precompiler Deployment, KEDA ScaledObject, NATS stream + bucket bootstrap, values.yaml.

### Non-goals (v1)

- S3, OCI, or other `ArtifactStore` backends beyond NATS. The trait is pluggable; start with NATS only.
- Signing / attestation of `.cwasm` artifacts. Trust model is "admission-gated via Artifact CRD + OPA" as a follow-up. The `unsafe` boundary is documented.
- Garbage collection of orphaned auto-created Artifacts. A label makes them findable; a cleanup controller is future work.
- Multi-target scheduling on a single host. Each host compiles/runs for one target triple.
- Cross-namespace Artifact sharing. Auto-created Artifacts live in the WD's namespace.

---

## Target Architecture

```
                                                                ┌─────────────────────┐
                                                                │  OCI Registry       │
                                                                │  (source .wasm)     │
                                                                └──────┬──────────────┘
                                                                       │ pull .wasm
                                                                       │
   ┌──────────────┐   1. apply        ┌──────────────────────────┐     │
   │  User / CD   ├───────────────────► Artifact CR              │     │
   └──────────────┘  (or auto-created │  status.precompiled[]    │     │
          │          by WD controller)│  status.conditions[]     │     │
          │                           └────┬─────────────────────┘     │
          │                                │ 2. reconcile              │
          │                                ▼                           │
          │                 ┌──────────────────────────────┐           │
          │                 │ ArtifactReconciler (singleton│           │
          │                 │ leader-elected, existing)    │           │
          │                 └──┬─────────────────────┬─────┘           │
          │                    │ 3. enqueue job/     │ 8. patch status │
          │                    │    target           │   on completion │
          │                    ▼                     ▲                 │
          │         ┌──────────────────────────────────────┐           │
          │         │ NATS JetStream "wasmcloud-precompile"│           │
          │         │   work queue (durable consumer)      │           │
          │         └────┬───────────────────────────┬─────┘           │
          │              │ 4. lag > threshold        │ 5. pull         │
          │              ▼                           ▼                 │
          │       ┌──────────────┐              ┌───────────────────┐  │
          │       │ KEDA         │ scale 1..10  │ Precompile Worker │──┤
          │       │ ScaledObject ├─────────────►│ (Rust Deployment) │  │
          │       └──────────────┘              │                   │  │
          │                                     │ 6. compile        │  │
          │                                     │ 7. put + notify   │  │
          │                                     └────────┬──────────┘  │
          │                                              ▼             │
          │                          ┌───────────────────────────────┐ │
          │                          │ ArtifactStore (NATS JS Object │ │
          │                          │ Store — default backend)      │ │
          │                          │ URL: nats://bucket/…cwasm     │ │
          │                          └──────────────┬────────────────┘ │
          │                                         ▲                  │
          │                                         │ 12. fetch        │
          │ 9. apply                                │                  │
          └────► WorkloadDeployment ────┐           │                  │
                (refs Artifact)         │           │                  │
                                        ▼           │                  │
                          ┌──────────────────────┐  │                  │
                          │ WD Reconciler        │  │                  │
                          │  condition chain:    │  │                  │
                          │  Artifact → Sync     │  │                  │
                          │  → Deploy → Scale    │  │                  │
                          │                      │  │                  │
                          │  [new] Strict mode:  │  │                  │
                          │   gate on            │  │                  │
                          │   Precompiled for    │  │                  │
                          │   host-pool target   │  │                  │
                          │                      │  │                  │
                          │  [new] Sync injects  │  │                  │
                          │   status.precompiled │  │                  │
                          │   into dispatch tmpl │  │                  │
                          └──────────┬───────────┘  │                  │
                                     │ 11. dispatch │                  │
                                     ▼              │                  │
                        ┌─────────────────────────┐ │                  │
                        │ wash-runtime Host       │ │                  │
                        │  ComponentLoader.load() │─┘                  │
                        │   ├─ find matching      │                    │
                        │   │   (target, compat)  │                    │
                        │   │   variant           │                    │
                        │   ├─ ArtifactStore      │                    │
                        │   │   .fetch(url)       │                    │
                        │   ├─ Component::        │                    │
                        │   │   deserialize       │                    │
                        │   │   (no Cranelift)    │                    │
                        │   └─ miss + Fallback:   │                    │
                        │      pull .wasm ────────┼────────────────────┘
                        │      Component::new     │
                        └─────────────────────────┘
```

---

## Design decisions

### URL as the cross-language contract

The reference stored in `Artifact.status.precompiled[].artifactUrl` is a URL with a scheme. Host and precompile worker both dispatch on scheme:

```
nats://{bucket}/{digest}-{target}-{wasmtime_version}.cwasm
```

- Content-addressed filename (digest-based) → dedup across Artifacts; cache is naturally shared.
- Scheme-based dispatch → adding S3/OCI/etc. later is a new file, not a redesign.
- Stable across process/language boundaries → Rust host and Go worker agree without sharing code.

### Three cache-key dimensions, all required

A precompiled variant only matches a host if all three line up:

1. **Target triple** — e.g. `x86_64-linux-gnu`. Encoded in the filename and in the variant struct.
2. **Wasmtime version** — exact. Any drift between worker and host must be detected, not absorbed.
3. **Engine compat hash** — the result of `wasmtime::Engine::precompile_compatibility_hash()` serialized to a stable string. Covers Cranelift config, CPU feature baseline, allocator choice, etc.

Guarantee these stay in lockstep by building the worker binary from the same Cargo workspace as `wash-runtime`. Cargo.lock handles the version pin. The compat hash is a runtime safety net; if it ever mismatches, the host treats the variant as unusable.

### Two modes: Strict vs. Fallback

Per-WorkloadDeployment field `precompileMode`:

| Mode | WD gate | Host behavior on miss |
|---|---|---|
| `Fallback` (default) | Proceeds when Artifact is `Published` | Pulls .wasm from OCI, `Component::new` inline |
| `Strict` | Also requires `Precompiled` for the host-pool target(s) | Never reached — WD doesn't dispatch until variant is ready |

Default is Fallback to keep backward compatibility. Production workloads opt in to Strict to guarantee zero Cranelift on the workload host.

### Auto-create Artifact from inline image

Make `WorkloadDeploymentArtifact` a oneof between `artifactFrom` (reference existing) and `image` (inline). Auto-created Artifacts:

- Named deterministically: `auto-<sha256(image+pullSecret)[:12]>`. Multiple WDs using the same image converge on one Artifact → one precompile → shared cache.
- Labeled `runtime.wasmcloud.dev/auto-created=true` so ops and a future cleanup controller can find them.
- No `ownerReference`. The existing Artifact finalizer (artifact_controller.go:66) already blocks deletion while any WD references it, which is exactly the semantics we want for multi-referenced resources.

### Content-addressed put + idempotent job submission

Two independent reasons we never duplicate work:

- The reconciler deduplicates job submission by `(artifact.digest, target, wasmtime_version)`. Existing job in the stream → skip.
- Even if a duplicate slips through, the worker writes to a content-addressed object key. Duplicate puts produce the same bytes at the same key.

### Fallback-first rollout

Ship with Fallback as the only mode behavior actually wired into the runtime. Strict mode lands as a WD controller gate, not a host-side enforcement. The host never refuses to run a workload — the operator refuses to dispatch it. This keeps the blast radius of the new code path contained: if the host-side loader has a bug, Fallback still produces a working component via the existing code path.

---

## Phased build plan

Each phase is independently shippable and independently useful. A phase can be merged to `main` (or the POC branch) before the next phase starts.

### Phase 1 — wash-runtime seams (no new infrastructure)

**Goal:** Host-side plumbing to consume precompiled variants, with no dependency on operator, worker, or chart. Can be tested in isolation by hand-crafting `ComponentSource` values.

#### Files

| Action | Path |
|---|---|
| New | `crates/wash-runtime/src/artifact_store/mod.rs` |
| New | `crates/wash-runtime/src/artifact_store/nats.rs` |
| New | `crates/wash-runtime/src/component_loader/mod.rs` |
| Extend | `crates/wash-runtime/src/engine/workload.rs` |
| Extend | `crates/wash-runtime/src/host/mod.rs` |
| Extend | `crates/wash-runtime/src/lib.rs` |
| Extend | `crates/wash-runtime/Cargo.toml` |

#### `artifact_store/mod.rs`

```rust
use std::{collections::HashMap, sync::Arc};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use bytes::Bytes;

#[async_trait]
pub trait ArtifactStore: Send + Sync + 'static {
    async fn fetch(&self, reference: &str) -> anyhow::Result<Bytes>;
}

#[derive(Default, Clone)]
pub struct ArtifactStoreRegistry {
    by_scheme: Arc<HashMap<String, Arc<dyn ArtifactStore>>>,
}

impl ArtifactStoreRegistry {
    pub fn builder() -> ArtifactStoreRegistryBuilder { Default::default() }

    pub async fn fetch(&self, url: &str) -> anyhow::Result<Bytes> {
        let (scheme, _) = url.split_once("://")
            .with_context(|| format!("invalid artifact url: {url}"))?;
        let store = self.by_scheme.get(scheme)
            .ok_or_else(|| anyhow!("no artifact store registered for scheme {scheme}"))?;
        store.fetch(url).await
    }
}

#[derive(Default)]
pub struct ArtifactStoreRegistryBuilder {
    by_scheme: HashMap<String, Arc<dyn ArtifactStore>>,
}

impl ArtifactStoreRegistryBuilder {
    pub fn register(mut self, scheme: impl Into<String>, store: Arc<dyn ArtifactStore>) -> Self {
        self.by_scheme.insert(scheme.into(), store);
        self
    }
    pub fn build(self) -> ArtifactStoreRegistry {
        ArtifactStoreRegistry { by_scheme: Arc::new(self.by_scheme) }
    }
}
```

#### `artifact_store/nats.rs` (feature-gated `artifact-store-nats`, default on)

```rust
use async_nats::jetstream;
use anyhow::Context;

pub struct NatsArtifactStore {
    jetstream: jetstream::Context,
}

impl NatsArtifactStore {
    pub fn new(client: async_nats::Client) -> Self {
        Self { jetstream: jetstream::new(client) }
    }
}

#[async_trait::async_trait]
impl super::ArtifactStore for NatsArtifactStore {
    async fn fetch(&self, reference: &str) -> anyhow::Result<bytes::Bytes> {
        let url = url::Url::parse(reference)?;
        anyhow::ensure!(url.scheme() == "nats", "not a nats:// url");
        let bucket = url.host_str().context("no bucket in url")?;
        let object = url.path().trim_start_matches('/');
        let store = self.jetstream.get_object_store(bucket).await?;
        let mut reader = store.get(object).await?;
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf).await?;
        Ok(buf.into())
    }
}
```

#### `component_loader/mod.rs`

```rust
use std::sync::Arc;
use anyhow::Context;
use async_trait::async_trait;
use wasmtime::{component::Component, Engine};

use crate::artifact_store::ArtifactStoreRegistry;

#[derive(Debug, Clone)]
pub struct PrecompiledVariant {
    pub target: String,
    pub wasmtime_version: String,
    pub compat_hash: String,
    pub url: String,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct ComponentSource {
    pub name: String,
    pub oci_image: String,
    pub digest: Option<String>,
    pub precompiled: Vec<PrecompiledVariant>,
    pub fallback_bytes: Option<bytes::Bytes>, // populated by operator-side image pull, if applicable
}

#[async_trait]
pub trait ComponentLoader: Send + Sync {
    async fn load(&self, source: &ComponentSource, engine: &Engine) -> anyhow::Result<Component>;
}

pub struct DefaultComponentLoader {
    pub stores: ArtifactStoreRegistry,
    pub target: String,        // e.g. "x86_64-linux-gnu"
    pub wasmtime_version: String,
}

impl DefaultComponentLoader {
    fn compat_hash(&self, engine: &Engine) -> String {
        // Hash the wasmtime-provided opaque Hash impl into a stable string.
        use std::hash::{Hasher, Hash};
        let mut hasher = twox_hash::XxHash64::with_seed(0);
        engine.precompile_compatibility_hash().hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

#[async_trait]
impl ComponentLoader for DefaultComponentLoader {
    async fn load(&self, source: &ComponentSource, engine: &Engine) -> anyhow::Result<Component> {
        let my_hash = self.compat_hash(engine);
        for variant in &source.precompiled {
            if variant.target == self.target
                && variant.wasmtime_version == self.wasmtime_version
                && variant.compat_hash == my_hash
            {
                let bytes = self.stores.fetch(&variant.url).await
                    .with_context(|| format!("fetching precompiled variant {}", variant.url))?;
                // SAFETY: bytes produced by trusted precompile worker linked against the
                // same wasmtime version, admission-gated via Artifact CRD, matched on
                // engine compat hash. Any drift fails the equality checks above.
                let component = unsafe { Component::deserialize(engine, &bytes) }?;
                tracing::debug!(name = %source.name, url = %variant.url, "loaded precompiled variant");
                return Ok(component);
            }
        }

        // Fallback: inline compile
        let bytes = source.fallback_bytes.clone()
            .context("no matching precompiled variant and no fallback bytes available")?;
        tracing::info!(name = %source.name, "no precompiled variant matched; compiling inline");
        Component::new(engine, &bytes)
    }
}
```

#### Integration point in `engine/workload.rs`

Find every call site of `Component::new(engine, bytes)` (there are a handful — `UnresolvedWorkload::new` constructors, `intersect_interfaces`, and a few test paths). Replace with `ComponentLoader::load`. The `bytes` field on the internal `Component` type (`types.rs:62`) gets kept as `fallback_bytes` on `ComponentSource`.

The loader is held on `Host` via a new field:

```rust
pub struct Host {
    // ...existing...
    pub(crate) component_loader: Arc<dyn ComponentLoader>,
}
```

Defaulted in `HostBuilder::build()` to a `DefaultComponentLoader` constructed from the engine's detected target triple, `env!("CARGO_PKG_VERSION")` of wasmtime (or a wasmtime-exposed const if there is one), and an `ArtifactStoreRegistry` with a `NatsArtifactStore` if a NATS client was provided.

#### `HostBuilder` additions

```rust
impl HostBuilder {
    pub fn with_component_loader(mut self, loader: Arc<dyn ComponentLoader>) -> Self { ... }
    pub fn with_artifact_stores(mut self, stores: ArtifactStoreRegistry) -> Self { ... }
}
```

#### Cargo.toml

```toml
[features]
default = [..., "artifact-store-nats"]
artifact-store-nats = []

[dependencies]
url = { workspace = true }
twox-hash = "1"  # or any stable fast hasher; only used to finalize engine compat hash
```

`async_nats` is already a dependency. No new NATS dep.

#### Verification

- Unit test: construct a `DefaultComponentLoader`, hand it a `ComponentSource` with a mock `ArtifactStore` returning precompiled bytes, assert `load` returns a `Component`.
- Integration test (new file in `crates/wash-runtime/tests/`): spin up a NATS container (testcontainers is already a dep), push precompiled bytes (produced by calling `Engine::precompile_component` in the test itself) into a bucket, assert the loader fetches and deserializes them.
- Confirm no regression in existing tests (which run with `precompiled: vec![]` and `fallback_bytes: Some(bytes)`).

### Phase 2 — CRD & dispatch extension

**Goal:** Make the schema changes and wire `Artifact.status.precompiled[]` end-to-end, with variants populated by hand for testing. No worker yet.

#### Files

| Action | Path |
|---|---|
| Extend | `runtime-operator/api/runtime/v1alpha1/artifact_types.go` |
| Extend | `runtime-operator/api/runtime/v1alpha1/workloaddeployment_types.go` |
| Regenerate | `runtime-operator/api/runtime/v1alpha1/zz_generated.deepcopy.go` |
| Regenerate | `charts/runtime-operator/crds/runtime.wasmcloud.dev_artifacts.yaml` |
| Regenerate | `charts/runtime-operator/crds/runtime.wasmcloud.dev_workloaddeployments.yaml` |
| Extend | `proto/wasmcloud/runtime/v2/workload.proto` |
| Regenerate | `runtime-operator/pkg/rpc/wasmcloud/runtime/v2/workload.pb.go` |
| Regenerate | `crates/wash-runtime/src/rpc/...` (Rust-side generated protobuf — the build script handles this) |
| Extend | `runtime-operator/internal/controller/runtime/artifact_controller.go` |
| Extend | `runtime-operator/internal/controller/runtime/workload_deployment_controller.go` |

#### ArtifactStatus extension

```go
type PrecompiledVariant struct {
    Target          string `json:"target"`
    WasmtimeVersion string `json:"wasmtimeVersion"`
    CompatHash      string `json:"compatHash"`
    ArtifactURL     string `json:"artifactUrl"`
    Digest          string `json:"digest"`
}

type ArtifactStatus struct {
    condition.ConditionedStatus `json:",inline"`
    ObservedGeneration int64 `json:"observedGeneration,omitempty"`
    ArtifactURL        string `json:"artifactUrl,omitempty"`

    // NEW
    Precompiled []PrecompiledVariant `json:"precompiled,omitempty"`
}

const ArtifactConditionPrecompiled condition.ConditionType = "Precompiled"
```

The `Precompiled` condition is `True` when `status.precompiled[]` contains an entry for every target in the operator's configured target matrix.

`artifact_controller.go` gets a fourth reconciler method `reconcilePrecompiled`, initially a no-op that returns `ErrStatusUnknown` until Phase 3 wires the real logic. Registered alongside `Sync`/`Published` in `SetupWithManager`.

#### WorkloadDeployment extension

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

    // HostPoolTargets is the set of host target triples this deployment may run on.
    // Controls which precompiled variants must be present in Strict mode.
    // Defaults to the cluster-wide target matrix if unset.
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

Add a CEL validation rule: `has(self.artifactFrom) != has(self.image)`.

#### WD controller changes

Extend `reconcileArtifacts` (around line 37):

```go
func (r *WorkloadDeploymentReconciler) reconcileArtifacts(ctx context.Context, deployment *runtimev1alpha1.WorkloadDeployment) error {
    mode := deployment.Spec.PrecompileMode
    if mode == "" { mode = runtimev1alpha1.PrecompileModeFallback }
    targets := deployment.Spec.HostPoolTargets
    if len(targets) == 0 { targets = defaultTargetMatrix }  // from operator config

    for _, configArtifact := range deployment.Spec.Artifacts {
        name, err := r.resolveOrUpsertArtifact(ctx, deployment, &configArtifact)
        if err != nil { return err }

        artifact := &runtimev1alpha1.Artifact{}
        if err := r.Get(ctx, client.ObjectKey{Name: name, Namespace: deployment.Namespace}, artifact); err != nil {
            if apierrors.IsNotFound(err) {
                return condition.ErrStatusUnknown(fmt.Errorf("artifact %s not found", name))
            }
            return err
        }
        if !artifact.Status.AllTrue(runtimev1alpha1.ArtifactConditionPublished) {
            return condition.ErrStatusUnknown(fmt.Errorf("artifact %s not published", name))
        }

        // NEW: Strict mode gate
        if mode == runtimev1alpha1.PrecompileModeStrict {
            for _, target := range targets {
                if !hasPrecompiledVariantForTarget(artifact, target) {
                    return condition.ErrStatusUnknown(fmt.Errorf(
                        "artifact %s not precompiled for target %s", name, target))
                }
            }
        }
    }
    return nil
}

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
    err := r.Create(ctx, artifact)
    if err != nil && !apierrors.IsAlreadyExists(err) {
        return "", err
    }
    return name, nil
}
```

`reconcileSync` / `resolveArtifacts` (line 291) gets a small extension: when building the dispatch template, copy `artifact.Status.Precompiled` into the template's component/service dispatch struct.

#### Proto extension

In `proto/wasmcloud/runtime/v2/workload.proto`, extend both `Component` and `Service`:

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

Additive, wire-compatible. Existing hosts receiving messages without the new field behave identically.

#### Verification

- Unit test WD controller: fixture with an Artifact that has no precompiled variants, `precompileMode: Strict` → WD stays Pending; flip mode to Fallback → proceeds.
- Apply a WD with inline `image:` → Artifact named `auto-<hash>` appears.
- End-to-end manual test: hand-patch an Artifact's `status.precompiled[]` with a real NATS URL pointing at a `.cwasm` you produced locally, apply a Strict WD, observe the host loading via the Phase 1 `ComponentLoader` and running without local compile.

### Phase 3 — Worker + reconciler fan-out

**Goal:** Replace the manual `.cwasm` production from Phase 2 with the automated worker pipeline.

#### Files

| Action | Path |
|---|---|
| New | `crates/precompile-worker/Cargo.toml` |
| New | `crates/precompile-worker/src/main.rs` |
| New | `crates/precompile-worker/src/job.rs` |
| New | `crates/precompile-worker/src/runner.rs` |
| Extend | `runtime-operator/internal/controller/runtime/artifact_controller.go` |
| New | `runtime-operator/internal/precompile/queue.go` |

#### Worker binary (`crates/precompile-worker`)

Rust binary in the same workspace. Depends on `wash-runtime` to reuse `Engine` construction → identical wasmtime version and compat hash by construction.

Responsibilities:

1. Connect to NATS, open the `wasmcloud-precompile` JetStream stream as a durable pull consumer.
2. For each job:
   - Decode `PrecompileJob { artifact_ref, artifact_url (oci), target, pull_secret, requested_compat_hash }`.
   - Pull `.wasm` from OCI (reuse the existing `crates/wash-runtime/src/oci.rs` client).
   - Construct an `Engine` with the same config as production hosts.
   - Verify `engine.precompile_compatibility_hash()` == `requested_compat_hash`. Mismatch → NACK with a clear error.
   - `engine.precompile_component(&wasm_bytes)` → `.cwasm` bytes.
   - Compute digest of `.cwasm`.
   - Put to NATS Object Store at `{digest}-{target}-{wasmtime_version}.cwasm`.
   - Publish a completion event on `wasmcloud.precompile.completion.{artifact_namespace}.{artifact_name}` with the new variant's metadata.
   - ACK the job.

Completion events are idempotent — the worker can safely redeliver the same completion after a restart; the reconciler dedupes by content digest.

#### Queue helper (Go side, `runtime-operator/internal/precompile/queue.go`)

```go
type JobProducer interface {
    Submit(ctx context.Context, job PrecompileJob) error
}

type CompletionConsumer interface {
    Subscribe(ctx context.Context, handler func(PrecompileCompletion)) error
}

type PrecompileJob struct {
    ArtifactNamespace string
    ArtifactName      string
    ArtifactImage     string
    PullSecretName    string
    Target            string
    RequestedCompatHash string
    WasmtimeVersion   string
}
```

Two NATS-backed implementations of these interfaces in `runtime-operator/internal/precompile/nats/`.

#### Reconciler fan-out

Extend `artifact_controller.go` with a new condition reconciler `reconcilePrecompiled`:

```go
func (r *ArtifactReconciler) reconcilePrecompiled(ctx context.Context, artifact *runtimev1alpha1.Artifact) error {
    targets := r.TargetMatrix  // from operator config
    pendingTargets := []string{}

    for _, target := range targets {
        if !hasPrecompiledVariantForTarget(artifact, target) {
            pendingTargets = append(pendingTargets, target)
        }
    }

    if len(pendingTargets) == 0 {
        return nil // condition flips True
    }

    // Enqueue jobs for pending targets. Idempotent: jobs include a dedup key.
    for _, target := range pendingTargets {
        job := precompile.PrecompileJob{
            ArtifactNamespace: artifact.Namespace,
            ArtifactName:      artifact.Name,
            ArtifactImage:     artifact.Spec.Image,
            Target:            target,
            WasmtimeVersion:   r.WasmtimeVersion,
            // RequestedCompatHash left empty; worker verifies against its own engine
        }
        if err := r.Producer.Submit(ctx, job); err != nil {
            return err
        }
    }

    return condition.ErrStatusUnknown(fmt.Errorf("awaiting precompilation for %d target(s)", len(pendingTargets)))
}
```

A separate goroutine in the operator subscribes to completion events and patches `artifact.Status.Precompiled`, triggering a re-reconcile.

Register `ArtifactConditionPrecompiled` in `SetupWithManager`, between `Published` and `Ready`.

#### Verification

- Run NATS locally, run the worker, manually produce an Artifact, observe:
  - Job arrives on the stream.
  - Worker pulls OCI, compiles, puts cwasm.
  - Completion event arrives, reconciler patches Artifact status.
  - Artifact condition `Precompiled` flips to True.
- Restart the worker mid-job → job gets re-delivered, compile re-runs, content-addressed put is idempotent, status converges.

### Phase 4 — Chart + autoscaling

**Goal:** Turn the worker into a real deployment with KEDA scaling and NATS resource bootstrap.

#### Files

| Action | Path |
|---|---|
| New | `charts/runtime-operator/templates/precompile/deployment.yaml` |
| New | `charts/runtime-operator/templates/precompile/scaledobject.yaml` |
| New | `charts/runtime-operator/templates/precompile/stream.yaml` |
| New | `charts/runtime-operator/templates/precompile/bucket.yaml` |
| New | `charts/runtime-operator/templates/precompile/serviceaccount.yaml` |
| Extend | `charts/runtime-operator/values.yaml` |
| Extend | `charts/runtime-operator/Chart.yaml` |
| Extend | `charts/runtime-operator/README.md` (KEDA prerequisite) |

#### `values.yaml` block

```yaml
precompile:
  # -- Enable the out-of-process precompile pipeline.
  # When false: hosts compile components inline (legacy behavior).
  enabled: true

  # -- Target triples to precompile for. Must include every target any host pool will run on.
  targets:
    - x86_64-linux-gnu

  workers:
    image:
      repository: ghcr.io/wasmcloud/precompile-worker
      tag: ""  # defaults to appVersion
    minReplicas: 1
    maxReplicas: 10
    resources:
      requests:
        cpu: 500m
        memory: 256Mi
      limits:
        cpu: "2"
        memory: 2Gi
    # -- KEDA trigger: scale up when queue lag exceeds this.
    lagThreshold: 1

  store:
    backend: nats    # nats | s3 (s3 reserved for future)
    nats:
      bucket: wasmcloud-cwasm
      replicas: 1
      maxBytes: 10Gi
    # s3:
    #   endpoint: ...
    #   bucket: ...
    #   credentialsSecretName: ...

  queue:
    stream: wasmcloud-precompile
    consumer: workers
    replicas: 1
```

#### Deployment + ScaledObject

```yaml
# scaledobject.yaml
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata:
  name: {{ include "runtime-operator.fullname" . }}-precompile-worker
spec:
  scaleTargetRef:
    name: {{ include "runtime-operator.fullname" . }}-precompile-worker
  minReplicaCount: {{ .Values.precompile.workers.minReplicas }}
  maxReplicaCount: {{ .Values.precompile.workers.maxReplicas }}
  triggers:
  - type: nats-jetstream
    metadata:
      natsServerMonitoringEndpoint: "{{ include "nats.monitoringUrl" . }}"
      account: "$G"
      stream: {{ .Values.precompile.queue.stream }}
      consumer: {{ .Values.precompile.queue.consumer }}
      lagThreshold: "{{ .Values.precompile.workers.lagThreshold }}"
```

The worker Deployment is standard. Important: separate ServiceAccount from the main operator so we can restrict RBAC (workers only need to patch Artifact status, not full operator rights).

#### NATS resource bootstrap

Two options for creating the stream and bucket:

- **(A)** Kubernetes `Job`s in a pre-install/pre-upgrade hook that run `nats` CLI against the in-cluster NATS.
- **(B)** NACK (NATS Controller) custom resources if the user is running NACK.

Start with (A) — no extra controller dependency. A single Job that runs:

```bash
nats stream add wasmcloud-precompile --subjects 'wasmcloud.precompile.jobs.>' --storage file --retention workqueue --replicas 1
nats object add wasmcloud-cwasm --replicas 1
```

#### Chart dependencies

KEDA is a soft prerequisite: the chart documents it but does not bundle it. If `precompile.enabled: false`, none of the precompile templates render and KEDA is not required. README gets a note.

#### Verification

- `helm install` into a fresh kind cluster with KEDA pre-installed.
- Apply Artifact, watch the worker pod get scheduled, scale up under load, scale back to 1 at idle.
- Kill a worker mid-compile, verify queue redelivery works.

### Phase 5 — Polish

- **Metrics.** Emit Prometheus metrics from the worker (jobs consumed, compile duration histogram, cache hits vs. misses) and from the host's `ComponentLoader` (precompile cache hit rate). These feed the decision about whether to move workloads to Strict.
- **Docs.** `docs/precompile.md` with the architecture diagram, mode comparison, troubleshooting table.
- **Example.** End-to-end example in `examples/precompile-demo/` showing Artifact + WD + observing the pipeline.
- **Cleanup of orphaned auto-Artifacts.** Small controller that deletes Artifacts with the `auto-created` label that have no referencing WDs and no recent use. Optional; can ship later.

---

## File-by-file summary

### New files

| Path | Purpose |
|---|---|
| `crates/wash-runtime/src/artifact_store/mod.rs` | `ArtifactStore` trait + `ArtifactStoreRegistry` |
| `crates/wash-runtime/src/artifact_store/nats.rs` | NATS JetStream Object Store impl |
| `crates/wash-runtime/src/component_loader/mod.rs` | `ComponentLoader` trait, `DefaultComponentLoader`, `ComponentSource`, `PrecompiledVariant` |
| `crates/precompile-worker/Cargo.toml` | Worker binary crate manifest |
| `crates/precompile-worker/src/main.rs` | Worker entry point |
| `crates/precompile-worker/src/job.rs` | Job + completion types |
| `crates/precompile-worker/src/runner.rs` | Compile loop |
| `runtime-operator/internal/precompile/queue.go` | Queue producer/consumer interfaces |
| `runtime-operator/internal/precompile/nats/producer.go` | NATS job producer |
| `runtime-operator/internal/precompile/nats/consumer.go` | NATS completion consumer |
| `charts/runtime-operator/templates/precompile/deployment.yaml` | Worker Deployment |
| `charts/runtime-operator/templates/precompile/scaledobject.yaml` | KEDA ScaledObject (1–10) |
| `charts/runtime-operator/templates/precompile/stream.yaml` | NATS stream bootstrap Job |
| `charts/runtime-operator/templates/precompile/bucket.yaml` | NATS object store bucket bootstrap Job |
| `charts/runtime-operator/templates/precompile/serviceaccount.yaml` | Worker ServiceAccount + RBAC |

### Modified files

| Path | Change |
|---|---|
| `crates/wash-runtime/src/lib.rs` | Expose `artifact_store`, `component_loader` modules |
| `crates/wash-runtime/src/engine/workload.rs` | Replace `Component::new` call sites with `ComponentLoader::load` |
| `crates/wash-runtime/src/host/mod.rs` | `component_loader` field, `HostBuilder` setters, loader default construction |
| `crates/wash-runtime/src/types.rs` | Add `precompiled: Vec<PrecompiledVariant>` to internal `Component` and `Service` |
| `crates/wash-runtime/Cargo.toml` | `artifact-store-nats` feature, new deps (`url`, `twox-hash`) |
| `proto/wasmcloud/runtime/v2/workload.proto` | `PrecompiledVariant` message; `repeated PrecompiledVariant precompiled` on `Component` and `Service` |
| `runtime-operator/api/runtime/v1alpha1/artifact_types.go` | `Precompiled []PrecompiledVariant`, `ArtifactConditionPrecompiled` constant |
| `runtime-operator/api/runtime/v1alpha1/workloaddeployment_types.go` | `PrecompileMode`, `HostPoolTargets`, inline `Image`/`ImagePullSecret` on `WorkloadDeploymentArtifact` |
| `runtime-operator/internal/controller/runtime/artifact_controller.go` | New `reconcilePrecompiled`, completion-event subscription, wiring |
| `runtime-operator/internal/controller/runtime/workload_deployment_controller.go` | Strict-mode gate in `reconcileArtifacts`, variant injection in `resolveArtifacts`, auto-upsert helper |
| `charts/runtime-operator/values.yaml` | `precompile:` block |
| `charts/runtime-operator/Chart.yaml` | KEDA prerequisite annotation |
| `charts/runtime-operator/crds/runtime.wasmcloud.dev_artifacts.yaml` | Regenerated from types |
| `charts/runtime-operator/crds/runtime.wasmcloud.dev_workloaddeployments.yaml` | Regenerated from types |
| `runtime-operator/pkg/rpc/wasmcloud/runtime/v2/workload.pb.go` | Regenerated from proto |

---

## Open questions to decide during build

1. **Wasmtime version exposure.** Is there a constant we can read from `wasmtime` the crate, or do we bake it into `wash-runtime` via `build.rs`? A mismatch here is a latent bug, so we want it pulled directly from the wasmtime crate if possible.
2. **Host-pool target discovery.** Right now `HostPoolTargets` is a user-provided list on the WD. Should the operator auto-discover it by introspecting which Hosts are registered for this WD's placement? For v1, stick with user-provided + cluster default; discovery is future work.
3. **Job submission path on WD create.** When a Strict WD references an Artifact that exists but has no precompiled variants yet, should the WD controller nudge the Artifact reconciler (e.g. patch an annotation to force requeue)? Probably not — the Artifact reconciler already requeues on its own interval. The WD just has to be patient.
4. **Completion event transport.** NATS subject vs. the worker writing directly to Artifact status via K8s API. Subject is cleaner (workers don't need k8s RBAC, all writes funnel through the reconciler) — go with that.
5. **`Component::deserialize_file` vs `::deserialize`.** File-backed (mmap) is kernel-evictable and saves RSS; in-memory is simpler. Since we're fetching bytes over NATS, we have `Bytes` in hand. Options: write to a tmpfile and `deserialize_file` (gains mmap), or `deserialize` directly. Start with `deserialize`; revisit if RSS pressure is measurable.
6. **Engine config alignment.** The worker must construct its `Engine` with the exact same `Config` the host uses. Factor out the engine-config construction in `wash-runtime` so both binaries call the same function.

---

## Testing strategy

| Layer | Test |
|---|---|
| `ComponentLoader` unit | Mock `ArtifactStore` returning known precompiled bytes; assert `load` picks the right variant, falls back correctly. |
| `NatsArtifactStore` integration | testcontainers NATS, put object, fetch, assert round-trip. |
| Worker integration | testcontainers NATS, publish a job, run worker, assert cwasm lands in object store + completion event fires. |
| Artifact reconciler integration | envtest Kubernetes, apply Artifact, assert jobs get submitted and status converges when completions arrive. |
| WD reconciler integration | envtest, apply WD in Strict mode referencing an un-precompiled Artifact, assert it stays Pending; fake a status patch, assert it proceeds. |
| End-to-end (kind) | Full chart install with KEDA, apply Artifact + WD, assert workload runs and host never invoked Cranelift (observable via metric). |

---

## Future work (not in this plan)

- **S3 / OCI `ArtifactStore` backends.** Trait and registry are already pluggable; adding a backend is a new file.
- **OPA-gated Artifact admission.** When the signing/attestation story lands, the CRD admission webhook runs OPA policies before letting Artifacts transition to `Ready`.
- **Cross-cluster artifact sharing.** Push cwasms to an OCI registry as OCI artifacts; any cluster with matching engine config pulls directly.
- **Build-time precompile in CI.** `wash precompile` CLI produces cwasm alongside the .wasm push; the cluster sees Artifact.status.precompiled populated on day one with no worker involvement.
- **Multi-target scheduling.** Hosts advertise their target; the WD placement controller picks matching hosts for each variant.
- **Garbage collection of auto-Artifacts.** Cleanup controller keyed on the `auto-created` label.

---

## Key files reference

| File | Role today |
|---|---|
| `crates/wash-runtime/src/engine/workload.rs` (2738 lines) | Unresolved→Resolved workload transition, calls `Component::new` |
| `crates/wash-runtime/src/host/mod.rs` (1126 lines) | Host coordinator, owns plugins + HTTP handler |
| `crates/wash-runtime/src/oci.rs` (813 lines) | OCI pull client, reused by worker |
| `runtime-operator/internal/controller/runtime/artifact_controller.go` (114 lines) | Artifact reconciler — `Sync`/`Published`/`Ready` conditions |
| `runtime-operator/internal/controller/runtime/workload_deployment_controller.go` (329 lines) | WD reconciler — `Artifact`/`Sync`/`Deploy`/`Scale`/`Ready` conditions |
| `proto/wasmcloud/runtime/v2/workload.proto` | Dispatch wire format, `Component`/`Service` messages |
| `charts/runtime-operator/crds/runtime.wasmcloud.dev_artifacts.yaml` | Artifact CRD — `spec.image`, `status.artifactUrl` |
| `charts/runtime-operator/values.yaml` | Chart config |
