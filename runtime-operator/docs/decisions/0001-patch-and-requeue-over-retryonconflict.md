# 0001 — Fix Artifact status conflicts with merge-patch instead of `Update`

- Status: Accepted
- Date: 2026-07-08
- Scope: `PrecompileReconciler` (`internal/controller/runtime/artifact_precompile_controller.go`)

## Context

The `Artifact` object's status is written by **two** controllers:

- `ArtifactReconciler` (via the shared conditioned reconciler) — writes status
  with `Status().Patch(client.MergeFrom(...))`.
- `PrecompileReconciler` — wrote status with four `r.Status().Update(ctx, a)`
  calls.

`Update` sends the object's `resourceVersion` as an optimistic-lock
precondition. Because both controllers watch and write the same object,
`PrecompileReconciler`'s cached copy was regularly stale by the time it issued
its `Update`, so the precondition failed and the API server returned
`409 Conflict`. The conflicts requeued harmlessly but **flooded the logs**.

## Decision

Convert `PrecompileReconciler`'s four status writes from `Status().Update` to
`Status().Patch(ctx, a, client.MergeFrom(base))`, where each handler captures
`base := a.DeepCopy()` immediately before it mutates `a.Status`.

A merge-patch sends only the changed fields and carries **no `resourceVersion`
precondition**, so the two controllers' independent status fields (`Precompiled`
and the precompile conditions vs. the Artifact's own conditions) no longer
collide. This is the same pattern the rest of the operator already uses
(`host_pod_controller.go`, `conditioned_reconciler.go`).

We do **not** add `RetryOnConflict`. A conflict means the controller acted on
stale data; retrying only re-runs the *write*, not the *decision* behind it.
Returning the error and letting controller-runtime requeue re-runs the whole
reconcile on fresh data — so decisions heal too. Here, merge-patch removes the
conflict at the source, so requeue-on-error is enough (see references).

## Consequences

- No more 409 log spam from precompile status writes.
- Status writes are consistent with the rest of the operator.
- Merge-patch drops the optimistic-lock guard by default. That is correct for
  these single-writer status fields; if a future write needs a true
  compare-and-swap, opt in explicitly with
  `client.MergeFromWithOptions(base, client.MergeFromWithOptimisticLock{})` and
  still return the conflict to requeue rather than looping.
- If conflicts ever reappear *with* patches, treat it as a real contention
  signal (revisit field ownership or server-side apply) — not a reason to add
  retry.

## References

- Gardener — Kubernetes clients guide (conflict handling):
  https://github.com/gardener/gardener/blob/master/docs/development/kubernetes-clients.md
- Tim Ebert — Kubernetes Controllers at Scale: Clients, Caches, Conflicts,
  Patches Explained:
  https://medium.com/@timebertt/kubernetes-controllers-at-scale-clients-caches-conflicts-patches-explained-aa0f7a8b4332
- Alena Varkockova — Operators best practices: understanding conflict errors:
  https://alenkacz.medium.com/kubernetes-operators-best-practices-understanding-conflict-errors-d05353dff421
- controller-runtime `reconcile` package (requeue-on-error semantics):
  https://pkg.go.dev/sigs.k8s.io/controller-runtime/pkg/reconcile
