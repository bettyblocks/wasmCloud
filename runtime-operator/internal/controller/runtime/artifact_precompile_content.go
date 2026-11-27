package runtime

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"strings"

	batchv1 "k8s.io/api/batch/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// ContentAddressedConfig switches the precompile controller to
// content-addressed keys (see precompile_digest.go):
//
//  1. Resolve Artifact.Spec.Image to its manifest and Wasm layer digest (one
//     registry GET, with the Artifact's imagePullSecret).
//  2. Build the key from the layer digest, the target, the wasmtime version
//     and the engine profile.
//  3. If the object exists, record it without a Job (reason CacheHit).
//  4. If another Artifact in the namespace already runs a Job for the same
//     key, wait for it (reason WaitingForSharedJob).
//  5. Otherwise run the Job with the image pinned by manifest digest.
type ContentAddressedConfig struct {
	Resolver DigestResolver
	Store    PrecompiledObjectStore
	// EngineProfile is `wash-precompile --print-engine-profile` of the
	// worker image. The worker refuses a key with another profile.
	EngineProfile string
	// JobReader reads Jobs when looking for a shared Job. Use an uncached
	// reader (mgr.GetAPIReader) so that two Artifacts reconciled right after
	// each other do not both start a Job for the same key. Nil uses the
	// reconciler's client. A duplicate Job is harmless (it writes the same
	// bytes); this only avoids the extra pod.
	JobReader client.Reader
}

const (
	// contentKeyLabel is set on content-addressed Jobs. Its value is a hash
	// of the output URL, so Artifacts with the same bytes find each other's
	// Job.
	contentKeyLabel = "runtime.wasmcloud.dev/precompile-content-key"

	reasonCacheHit            condition.ConditionReason = "CacheHit"
	reasonWaitingForSharedJob condition.ConditionReason = "WaitingForSharedJob"
	reasonResolveFailed       condition.ConditionReason = "ResolveFailed"
)

// contentKeyLabelValue is a label-safe (<= 63 chars) name for an output URL.
func contentKeyLabelValue(outputURL string) string {
	sum := sha256.Sum256([]byte(outputURL))
	return hex.EncodeToString(sum[:20])
}

// isCurrentContentURL reports whether url is a content-addressed key of this
// operator's configuration (base, target, wasmtime version, engine profile).
func (r *PrecompileReconciler) isCurrentContentURL(url string) bool {
	rest, ok := strings.CutPrefix(url, contentKeyPrefix(r.ArtifactStore.BaseURL))
	if !ok {
		return false
	}
	hexPart, ok := strings.CutSuffix(rest,
		contentKeySuffix(r.Target, r.WasmtimeVersion, r.ContentAddressed.EngineProfile))
	return ok && len(hexPart) == 64 && strings.Trim(hexPart, "0123456789abcdef") == ""
}

func (r *PrecompileReconciler) reconcileContentAddressed(
	ctx context.Context, a *runtimev1alpha1.Artifact,
) (ctrl.Result, error) {
	// A variant of this configuration for this exact image ref is recorded:
	// do not ask the registry again on every resync. Like the legacy path,
	// this does not notice a tag that is re-pushed without an Artifact
	// change; a new Artifact (or a changed image ref) is resolved again.
	for _, v := range a.Status.Precompiled {
		if v.Target == r.Target && v.WasmtimeVersion == r.WasmtimeVersion &&
			v.ImageRef == a.Spec.Image && r.isCurrentContentURL(v.ArtifactURL) {
			return ctrl.Result{}, r.dropSupersededVariants(ctx, a, v)
		}
	}

	resolved, err := r.resolve(ctx, a)
	if err != nil {
		return ctrl.Result{}, r.reportResolveFailure(ctx, a, err)
	}
	outputURL, err := DigestKeyURL(r.ArtifactStore.BaseURL, resolved.LayerDigest,
		r.Target, r.WasmtimeVersion, r.ContentAddressed.EngineProfile)
	if err != nil {
		return ctrl.Result{}, r.reportResolveFailure(ctx, a, err)
	}
	pullRef, err := PinnedImageRef(a.Spec.Image, resolved.ManifestDigest)
	if err != nil {
		return ctrl.Result{}, r.reportResolveFailure(ctx, a, err)
	}
	desired, err := r.buildDesiredJob(a, pullRef, outputURL)
	if err != nil {
		return ctrl.Result{}, err
	}
	if desired.Labels == nil {
		desired.Labels = map[string]string{}
	}
	desired.Labels[contentKeyLabel] = contentKeyLabelValue(outputURL)
	variant := r.variantFor(a, outputURL)

	// This Artifact's own Job, if it is for this key. A Job for another key
	// (an older image, a tag that moved, another engine) is stale: it is
	// deleted and its deletion requeues us. Only the key counts: a Job that
	// pulls another manifest with the same layer writes the same bytes. Unlike the legacy path the
	// recorded variant is kept until the new one replaces it, so switching
	// this mode on does not take running workloads' variants away.
	var own *batchv1.Job
	var existing batchv1.Job
	err = r.Get(ctx, client.ObjectKeyFromObject(desired), &existing)
	switch {
	case apierrors.IsNotFound(err):
	case err != nil:
		return ctrl.Result{}, err
	case existing.DeletionTimestamp != nil:
		return ctrl.Result{}, nil
	case !outputMatches(&existing, desired):
		return ctrl.Result{}, r.deleteJob(ctx, &existing)
	default:
		own = &existing
	}

	hit, err := r.ContentAddressed.Store.Claim(ctx, outputURL)
	if err != nil {
		return ctrl.Result{}, fmt.Errorf("looking up %s: %w", outputURL, err)
	}
	if hit {
		reason := reasonCacheHit
		if own != nil && jobComplete(own) {
			reason = reasonJobSucceeded
		}
		return r.recordContentVariant(ctx, a, variant, reason)
	}

	switch {
	case own == nil:
		shared, err := r.sharedJobFor(ctx, desired)
		if err != nil {
			return ctrl.Result{}, err
		}
		if shared != nil {
			return ctrl.Result{}, r.waitForSharedJob(ctx, a, shared)
		}
		if err := r.Create(ctx, desired); err != nil && !apierrors.IsAlreadyExists(err) {
			return ctrl.Result{}, err
		}
		return r.handleInFlightJob(ctx, a)
	case jobComplete(own):
		// The Job completed, but the claim found no object: it was removed
		// after the Job wrote it. Run the Job again.
		ctrl.LoggerFrom(ctx).Info("precompile Job completed but its object is missing, running it again",
			"job", own.Name, "url", outputURL)
		return ctrl.Result{}, r.deleteJob(ctx, own)
	}
	if failed, msg := jobFailed(own); failed {
		return r.handleFailedJob(ctx, a, msg)
	}
	return r.handleInFlightJob(ctx, a)
}

func (r *PrecompileReconciler) resolve(ctx context.Context, a *runtimev1alpha1.Artifact) (ResolvedImage, error) {
	var creds *RegistryCredentials
	if a.Spec.ImagePullSecret != nil {
		s, err := MaterializeImagePullSecret(ctx, r.Client, a.Namespace, a.Spec.ImagePullSecret.Name, a.Spec.Image)
		if err != nil {
			return ResolvedImage{}, fmt.Errorf("reading imagePullSecret %q: %w", a.Spec.ImagePullSecret.Name, err)
		}
		if s.Username != "" || s.Password != "" {
			creds = &RegistryCredentials{Username: s.Username, Password: s.Password}
		}
	}
	res, err := r.ContentAddressed.Resolver.Resolve(ctx, a.Spec.Image, creds)
	if err != nil {
		return ResolvedImage{}, fmt.Errorf("resolving %s: %w", a.Spec.Image, err)
	}
	return res, nil
}

// reportResolveFailure records err on the Artifact and returns it, so the
// reconcile is retried with backoff (the registry may be down for a moment).
func (r *PrecompileReconciler) reportResolveFailure(
	ctx context.Context, a *runtimev1alpha1.Artifact, cause error,
) error {
	existing := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed)
	if existing.Status == condition.ConditionTrue && existing.Reason == reasonResolveFailed &&
		existing.Message == cause.Error() {
		return cause
	}
	base := a.DeepCopy()
	now := metav1.Now()
	a.Status.SetConditions(
		condition.Condition{
			Type:               runtimev1alpha1.ArtifactConditionPrecompileFailed,
			Status:             condition.ConditionTrue,
			Reason:             reasonResolveFailed,
			Message:            cause.Error(),
			LastTransitionTime: now,
		},
		condition.Condition{
			Type:               runtimev1alpha1.ArtifactConditionPrecompileProgressing,
			Status:             condition.ConditionFalse,
			Reason:             reasonResolveFailed,
			LastTransitionTime: now,
		},
	)
	if err := r.patchStatus(ctx, a, base); err != nil {
		return errors.Join(cause, err)
	}
	return cause
}

// recordContentVariant records variant and clears a stale PrecompileFailed.
func (r *PrecompileReconciler) recordContentVariant(
	ctx context.Context, a *runtimev1alpha1.Artifact, variant runtimev1alpha1.PrecompiledVariant,
	reason condition.ConditionReason,
) (ctrl.Result, error) {
	if a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed).Status == condition.ConditionTrue {
		base := a.DeepCopy()
		a.Status.SetConditions(condition.Condition{
			Type:               runtimev1alpha1.ArtifactConditionPrecompileFailed,
			Status:             condition.ConditionFalse,
			Reason:             reason,
			LastTransitionTime: metav1.Now(),
		})
		if err := r.patchStatus(ctx, a, base); err != nil {
			return ctrl.Result{}, err
		}
	}
	// The URL holds the digest, so it must match too: the same tag can now
	// name other bytes.
	return r.recordVariant(ctx, a, variant, reason, variantRecordedWithURL)
}

func variantRecordedWithURL(existing []runtimev1alpha1.PrecompiledVariant, v runtimev1alpha1.PrecompiledVariant) bool {
	for _, e := range existing {
		if sameVariant(e, v) && e.ArtifactURL == v.ArtifactURL {
			return true
		}
	}
	return false
}

func (r *PrecompileReconciler) deleteJob(ctx context.Context, job *batchv1.Job) error {
	err := r.Delete(ctx, job,
		client.PropagationPolicy(metav1.DeletePropagationBackground),
		client.GracePeriodSeconds(0),
	)
	return client.IgnoreNotFound(err)
}

// sharedJobFor returns a running Job of another Artifact in the same
// namespace that writes the same key, or nil. Failed and completed Jobs do
// not count: the caller then runs its own Job.
func (r *PrecompileReconciler) sharedJobFor(ctx context.Context, desired *batchv1.Job) (*batchv1.Job, error) {
	reader := r.ContentAddressed.JobReader
	if reader == nil {
		reader = r.Client
	}
	var jobs batchv1.JobList
	if err := reader.List(ctx, &jobs, client.InNamespace(desired.Namespace),
		client.MatchingLabels{contentKeyLabel: desired.Labels[contentKeyLabel]}); err != nil {
		return nil, err
	}
	for i := range jobs.Items {
		j := &jobs.Items[i]
		if j.Name == desired.Name || j.DeletionTimestamp != nil || !outputMatches(j, desired) {
			continue
		}
		if failed, _ := jobFailed(j); failed || jobComplete(j) {
			continue
		}
		return j, nil
	}
	return nil, nil
}

// outputMatches compares the --output of two precompile Jobs.
func outputMatches(a, b *batchv1.Job) bool {
	out := func(j *batchv1.Job) string {
		if len(j.Spec.Template.Spec.Containers) == 0 {
			return ""
		}
		args := j.Spec.Template.Spec.Containers[0].Args
		for i := 0; i+1 < len(args); i++ {
			if args[i] == "--output" {
				return args[i+1]
			}
		}
		return ""
	}
	return out(a) != "" && out(a) == out(b)
}

func (r *PrecompileReconciler) waitForSharedJob(
	ctx context.Context, a *runtimev1alpha1.Artifact, shared *batchv1.Job,
) error {
	msg := "waiting for Job " + shared.Name + ", which precompiles the same component"
	cond := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing)
	if cond.Status == condition.ConditionTrue && cond.Reason == reasonWaitingForSharedJob && cond.Message == msg {
		return nil
	}
	base := a.DeepCopy()
	a.Status.SetConditions(condition.Condition{
		Type:               runtimev1alpha1.ArtifactConditionPrecompileProgressing,
		Status:             condition.ConditionTrue,
		Reason:             reasonWaitingForSharedJob,
		Message:            msg,
		LastTransitionTime: metav1.Now(),
	})
	return r.patchStatus(ctx, a, base)
}

// artifactsWaitingForJob maps an event on a content-addressed Job to the
// Artifacts in its namespace that wait for a shared Job.
func (r *PrecompileReconciler) artifactsWaitingForJob(ctx context.Context, obj client.Object) []reconcile.Request {
	if _, ok := obj.GetLabels()[contentKeyLabel]; !ok {
		return nil
	}
	var artifacts runtimev1alpha1.ArtifactList
	if err := r.List(ctx, &artifacts, client.InNamespace(obj.GetNamespace())); err != nil {
		ctrl.LoggerFrom(ctx).Error(err, "listing Artifacts waiting for a shared precompile Job",
			"namespace", obj.GetNamespace())
		return nil
	}
	var reqs []reconcile.Request
	for i := range artifacts.Items {
		a := &artifacts.Items[i]
		cond := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing)
		if cond.Status == condition.ConditionTrue && cond.Reason == reasonWaitingForSharedJob {
			reqs = append(reqs, reconcile.Request{
				NamespacedName: types.NamespacedName{Namespace: a.Namespace, Name: a.Name},
			})
		}
	}
	return reqs
}
