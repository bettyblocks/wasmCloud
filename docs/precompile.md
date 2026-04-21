# Component Pre-Compilation

When wasmCloud schedules a component on a host, wasmtime/Cranelift compiles it
in-process on first use. Compile is CPU-intensive and the resulting native
code sits in anonymous mmap, so a busy host paying both costs while it serves
requests has latency spikes and sustained RSS pressure.

The pre-compilation pipeline moves that work off the workload host. A
dedicated **precompile-worker** pool runs Cranelift against source `.wasm`
bytes, writes the resulting `.cwasm` to a content-addressed cache, and the
host loads via `Component::deserialize` — never invoking Cranelift in steady
state.

This document explains when to enable it, what behavior changes, and how to
operate it.

## Quick start

Enable in Helm values:

```yaml
precompile:
  enabled: true
  targets:
    - x86_64-unknown-linux-gnu   # match every host-pool target you run
  workers:
    minReplicas: 1
    maxReplicas: 10
```

Install KEDA in the cluster (prerequisite for worker autoscaling):

```
helm repo add kedacore https://kedacore.github.io/charts
helm install keda kedacore/keda --namespace keda --create-namespace
```

Then `helm install`/`upgrade` the runtime-operator chart as usual. A
post-install Job creates the JetStream stream, pull consumer, and object
store bucket.

No changes are needed to existing `Artifact` or `WorkloadDeployment`
resources — precompile is transparent to them. Add the opt-in field
(below) when you want Strict enforcement.

## Architecture

```
 ┌────────┐  apply Artifact   ┌──────────────────────┐
 │ User   ├──────────────────►│ ArtifactReconciler   │
 └────────┘                   │ (singleton)          │
                              └──┬───────────────┬───┘
                                 │ enqueue job   │ patch status.precompiled
                                 ▼               ▲
                    ┌────────────────────────────┴─┐
                    │ NATS JetStream work queue    │
                    └────┬─────────────────────┬───┘
                         │ consume             │ publish completion
                         ▼                     ▼
              ┌─────────────────────┐  ┌─────────────┐
              │ precompile-worker   │──┤  NATS Object│
              │ (KEDA scales 1..10) │  │  Store      │
              │ wasmtime::Engine    │  │  (cwasm)    │
              └─────────────────────┘  └─────┬───────┘
                                             │ fetch by URL
 ┌──────────┐ apply WorkloadDeployment       ▼
 │ User     ├───► WD reconciler → Workload → Host
 └──────────┘                                │
                                             ▼
                               ComponentLoader picks matching variant:
                                 match on (target, wasmtime_version, compat_hash)
                               → ArtifactStoreRegistry.fetch(nats://…)
                               → Component::deserialize (no Cranelift)
```

Three cache-key dimensions decide whether a variant is usable:
- **Target triple** — same arch/OS/libc as the host.
- **Wasmtime version** — exact, not SemVer-compatible.
- **Engine compat hash** — `Engine::precompile_compatibility_hash()` serialized
  to a stable hex string. Captures CPU feature baseline, allocator choice,
  compiler config.

Any mismatch skips the variant and falls back to inline compile (or fails,
depending on mode). Content-addressed cache keys mean no false positives.

## Modes: Fallback vs Strict

Set on a `WorkloadDeployment`:

```yaml
spec:
  precompileMode: Fallback   # default; or: Strict
```

| Mode | WD gate | Host behavior on variant miss |
|---|---|---|
| **Fallback** (default) | Proceeds when Artifact is `Published` | Pulls `.wasm` from OCI and compiles via `Component::new` |
| **Strict** | Also requires `status.precompiled[]` to cover every target in `hostPoolTargets` | Never reached — WD stays `Pending` until the worker finishes |

**Use Fallback for dev and for rollouts where deploy latency trumps strict
isolation.** First deploy of a new component still compiles once on the host,
but subsequent deploys hit the cache.

**Use Strict for production workloads where Cranelift must not run on the
workload host.** Ensure the Artifact exists and is precompiled before
rolling out the WorkloadDeployment — e.g. apply the Artifact in CI, wait for
its `Precompiled` condition, then apply the WorkloadDeployment.

## Inlining an image (auto-Artifact)

Instead of declaring an `Artifact` and referencing it, a WorkloadDeployment
can inline the image:

```yaml
spec:
  artifacts:
    - name: main
      image: ghcr.io/acme/my-component:v1.2.3
```

The controller upserts an Artifact named `auto-<sha256(image+pullSecret)[:12]>`
in the same namespace, labeled `runtime.wasmcloud.dev/auto-created=true`.
Two WorkloadDeployments inlining the same image converge on one Artifact —
the precompile pipeline runs once per unique `(image, pullSecret)` pair.

Explicit `artifactFrom` is still valid and preferred for shared or
GitOps-managed artifacts.

## Operating

### Checking Artifact status

```
kubectl get artifact <name> -o yaml
```

Look at `status.conditions` for `Precompiled`, and `status.precompiled[]`
for the list of published variants.

### Worker logs

```
kubectl logs -n <operator-ns> -l wasmcloud.com/name=precompile-worker
```

Each compile logs `compiling component` with namespace/name/target, then
`compile complete` with byte count and duration.

### Scaling behavior

KEDA polls the JetStream consumer every 15s. When lag (undelivered jobs per
replica) exceeds `precompile.workers.lagThreshold` (default 1), it scales up
toward `maxReplicas`. After 60s of idle, it scales back toward `minReplicas`.

`minReplicas: 1` keeps one warm worker so the first job of a burst pays no
pod-startup latency. You can scale to zero by setting `minReplicas: 0`, but
cold-start adds roughly 10–30s for pod scheduling and image pull.

### When a Strict WorkloadDeployment stays Pending

```
kubectl describe workloaddeployment <name>
```

Look at the `Artifact` condition message. Common causes:
- `artifact X not precompiled for target Y` — worker is still processing,
  or the Artifact's image is broken (check worker logs).
- `artifact X not found` — referenced Artifact doesn't exist. Check
  namespace and name.
- `artifact X not published` — the Artifact itself isn't ready yet; check
  its own conditions.

### Diagnosing a variant-not-loaded case

If you expected a precompiled variant to load but the host compiled inline,
it means one of `(target, wasmtime_version, compat_hash)` didn't match.

Host logs include: `no precompiled variant matched; compiling inline`.

Check the worker's startup log for its `target`, `wasmtime_version`, and
`compat_hash`, and compare against the host's (logged at startup and per
workload start). Any difference is the culprit.

The most common drift causes:
- Worker and host were built from different Cargo workspaces (different
  wasmtime versions) — fix by building both from this workspace.
- Host has different engine config than the worker — fix by reviewing
  `EngineBuilder::with_config` call sites.

## Trust boundary

`Component::deserialize` is an `unsafe` call — wasmtime trusts the native
code to be compatible with its runtime layout and assumes the bytes weren't
tampered with. Three layers protect it:

1. **Content-addressed cache keys.** The URL encodes
   `(digest, target, wasmtime_version)` and the host matches
   `(target, wasmtime_version, compat_hash)` against the Artifact status.
   A variant published under the wrong key is never loaded.
2. **Admission gating via Artifact CRD.** Only a controller with RBAC to
   patch Artifact status can publish variants. Policies (OPA/Kyverno) can
   gate which Artifacts are admitted in the first place.
3. **Version lockstep.** The worker binary and the host binary are built
   from the same Cargo workspace, so wasmtime version and engine config are
   identical by construction.

If you run a heterogeneous fleet (different wasmtime versions across hosts),
the compat hash check causes a cache miss on mismatched hosts — they fall
back to inline compile (Fallback mode) or stay Pending (Strict mode). No
unsafe cross-version loads are possible.

## What this doesn't change

- **Artifact CRD `artifactFrom` references are unchanged.** Existing
  deployments continue to work exactly as before.
- **Host behavior without a precompile pipeline is unchanged.** When
  `precompile.enabled=false` (or when no variant matches), the host takes
  the same code path as before this feature: pull `.wasm` from OCI,
  `Component::new`, run.
- **The in-process moka cache is preserved.** Variant fetches are cached by
  digest so repeated workload starts on the same host skip even the NATS
  round-trip.

## Non-goals (for now)

- Signing/attestation of `.cwasm` bytes beyond what the Artifact admission
  controller provides.
- Backends other than NATS JetStream Object Store (trait is pluggable; add a
  new scheme registration).
- Multi-target hosts. Each host compiles/runs for one target triple.
- Cleanup of orphaned auto-Artifacts — a label lets operators find them,
  but no automated eviction yet.

See `PRECOMPILER_PLAN.md` and `PRECOMPILER_IMPLEMENTATION.md` at the repo
root for the design and implementation history.
