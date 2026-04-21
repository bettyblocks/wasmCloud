# Precompile demo

A minimal end-to-end example of the out-of-process pre-compilation pipeline.

The demo deploys a single `hello-world` HTTP component, declared via the
inline `image:` form so the operator auto-creates its backing `Artifact`.
With precompile enabled in the chart, the worker pool produces a `.cwasm`
variant and the runtime host loads it via `Component::deserialize` — no
Cranelift on the workload host.

## Prerequisites

- `kind` cluster with KEDA installed:

  ```
  helm repo add kedacore https://kedacore.github.io/charts
  helm install keda kedacore/keda --namespace keda --create-namespace
  ```

- wasmCloud Helm chart installed with `precompile.enabled=true`. Example:

  ```
  helm install wasmcloud ../../charts/runtime-operator \
    --create-namespace -n wasmcloud \
    --set precompile.enabled=true \
    --set precompile.workers.minReplicas=1 \
    --set precompile.workers.maxReplicas=5
  ```

## Walkthrough

Apply the demo manifests:

```
kubectl apply -f manifests/
```

Two resources land in the cluster:

- `WorkloadDeployment/precompile-demo` — 1 replica of a hello-world HTTP
  component, with `precompileMode: Strict` so the controller refuses to
  dispatch until the worker publishes a matching variant.
- No standalone `Artifact` resource — the inline `image:` field on the
  deployment's artifact entry auto-creates one named
  `auto-<sha256(image)[:12]>`.

Observe the pipeline:

```bash
# Watch the auto-created Artifact
kubectl get artifact -l runtime.wasmcloud.dev/auto-created=true

# Watch its conditions
kubectl get artifact <auto-name> -o jsonpath='{.status.conditions}' | jq

# See precompiled variants as they appear
kubectl get artifact <auto-name> -o jsonpath='{.status.precompiled}' | jq
```

The worker emits a compile log per job:

```bash
kubectl logs -n wasmcloud -l wasmcloud.com/name=precompile-worker --tail=50
```

You should see `compiling component` followed by `compile complete` with a
size in bytes and a duration in ms.

Once the `Precompiled` condition is True, the `WorkloadDeployment` proceeds
through the rest of its condition chain (`Sync` → `Deploy` → `Scale`) and
the host starts the workload. The host's log will include
`loaded precompiled component` — no inline compile.

## Cleanup

```
kubectl delete -f manifests/
```

The auto-created Artifact stays around (its deletion is blocked by the WD
finalizer while referenced, then orphaned). With
`--auto-artifact-cleanup-interval=5m` on the operator (off by default), the
cleaner deletes orphans after their TTL (default 24h).

## Try it in Fallback mode

Edit `manifests/workloaddeployment.yaml` and change `precompileMode` from
`Strict` to `Fallback`. Re-apply. The WD will no longer block on the worker
— it will dispatch immediately, and the host will compile inline on first
use if the variant isn't ready yet. Subsequent replicas (and later deploys
of the same image) hit the cache once the worker catches up.

## Notes

- Because the demo uses a single, well-known image, two invocations (from
  the same namespace) converge on the same auto-Artifact and share the
  precompile cache. Re-applying the manifests is a no-op for the worker.
- The `Strict` gate depends on the operator's target matrix matching your
  host pool. If your hosts run on `aarch64` but the chart's default
  `precompile.targets: [x86_64-unknown-linux-gnu]` doesn't include it,
  Strict mode will stay Pending forever. Set `precompile.targets` to match.
