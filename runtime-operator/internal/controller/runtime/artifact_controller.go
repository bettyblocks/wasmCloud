package runtime

import (
	"context"
	"fmt"
	"time"

	"k8s.io/apimachinery/pkg/runtime"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	"go.wasmcloud.dev/runtime-operator/v2/internal/precompile"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

const (
	artifactReconcileInterval = 5 * time.Minute
	artifactFinalizerName     = "runtime.wasmcloud.dev/artifact-finalizer"
)

// ArtifactReconciler reconciles a Workload object
type ArtifactReconciler struct {
	client.Client
	Scheme *runtime.Scheme

	// JobProducer enqueues precompile jobs. May be nil to disable the
	// precompile pipeline entirely (artifacts will simply lack the
	// Precompiled condition; WorkloadDeployments in Fallback mode still work).
	JobProducer precompile.JobProducer

	// TargetMatrix is the set of target triples to produce variants for.
	// Wired from operator config.
	TargetMatrix []string

	// WasmtimeVersion is stamped into job messages so the worker can flag
	// version mismatch.
	WasmtimeVersion string

	reconciler condition.AnyConditionedReconciler
}

func (r *ArtifactReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	return r.reconciler.Reconcile(ctx, req)
}

func (r *ArtifactReconciler) reconcileSync(ctx context.Context, artifact *runtimev1alpha1.Artifact) error {
	if artifact.Status.ObservedGeneration == artifact.Generation {
		return nil
	}

	condition.ForceStatusUpdate(ctx)
	artifact.Status.ObservedGeneration = artifact.Generation
	artifact.Status.ArtifactURL = artifact.Spec.Image

	// Skip remaining steps cause we touched the .Status field
	return condition.ErrSkipReconciliation()
}

func (r *ArtifactReconciler) reconcilePublished(_ context.Context, artifact *runtimev1alpha1.Artifact) error {
	if artifact.Status.ArtifactURL != "" {
		return nil
	}

	return condition.ErrStatusUnknown(fmt.Errorf("artifact not published"))
}

func (r *ArtifactReconciler) reconcileReady(_ context.Context, artifact *runtimev1alpha1.Artifact) error {
	if !artifact.Status.AllTrue(
		runtimev1alpha1.ArtifactConditionSync,
		runtimev1alpha1.ArtifactConditionPublished) {
		return fmt.Errorf("artifact is not ready")
	}
	return nil
}

// reconcilePrecompiled enqueues compile jobs for each target in the matrix
// that doesn't yet have a corresponding entry in status.precompiled. The
// condition flips True only once every matrix target has a published variant.
//
// When JobProducer is nil the precompile pipeline is disabled entirely; the
// condition stays Unknown and consumers (WorkloadDeployment in Strict mode)
// simply never gate-pass — same as if the worker were down.
func (r *ArtifactReconciler) reconcilePrecompiled(ctx context.Context, artifact *runtimev1alpha1.Artifact) error {
	log := ctrl.LoggerFrom(ctx)

	if r.JobProducer == nil || len(r.TargetMatrix) == 0 {
		return condition.ErrStatusUnknown(fmt.Errorf("precompile pipeline disabled"))
	}
	if artifact.Status.ArtifactURL == "" {
		return condition.ErrStatusUnknown(fmt.Errorf("artifact not yet published"))
	}

	var pending []string
	for _, target := range r.TargetMatrix {
		hit := false
		for _, v := range artifact.Status.Precompiled {
			if v.Target == target {
				hit = true
				break
			}
		}
		if !hit {
			pending = append(pending, target)
		}
	}
	if len(pending) == 0 {
		return nil
	}

	for _, target := range pending {
		job := precompile.PrecompileJob{
			ArtifactNamespace: artifact.Namespace,
			ArtifactName:      artifact.Name,
			ArtifactImage:     artifact.Spec.Image,
			Target:            target,
			WasmtimeVersion:   r.WasmtimeVersion,
		}
		if err := r.JobProducer.Submit(ctx, job); err != nil {
			log.Error(err, "submit precompile job", "target", target)
			return condition.ErrStatusUnknown(fmt.Errorf("submit precompile job for %s: %w", target, err))
		}
	}
	log.Info("submitted precompile jobs", "pending", pending)
	return condition.ErrStatusUnknown(fmt.Errorf("awaiting precompilation for %d target(s)", len(pending)))
}

// finalize blocks deletion of an Artifact while any WorkloadDeployment in the
// same namespace still references it. This prevents deployments from being
func (r *ArtifactReconciler) finalize(ctx context.Context, artifact *runtimev1alpha1.Artifact) error {
	log := ctrl.LoggerFrom(ctx)

	deploymentList := &runtimev1alpha1.WorkloadDeploymentList{}
	if err := r.List(ctx, deploymentList, client.InNamespace(artifact.Namespace)); err != nil {
		return err
	}

	for _, deployment := range deploymentList.Items {
		if deployment.DeletionTimestamp != nil {
			continue
		}
		for i := range deployment.Spec.Artifacts {
			configArtifact := &deployment.Spec.Artifacts[i]
			refName, err := resolveArtifactName(configArtifact)
			if err != nil {
				// A malformed artifact entry can't reference anything; skip.
				continue
			}
			if refName == artifact.Name {
				log.Info("artifact deletion blocked: still referenced by WorkloadDeployment",
					"deployment", fmt.Sprintf("%s/%s", deployment.Namespace, deployment.Name))
				return fmt.Errorf("artifact is still referenced by WorkloadDeployment %s/%s",
					deployment.Namespace, deployment.Name)
			}
		}
	}

	return nil
}

// SetupWithManager sets up the controller with the Manager.
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=artifacts,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=artifacts/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=artifacts/finalizers,verbs=update

func (r *ArtifactReconciler) SetupWithManager(mgr ctrl.Manager) error {
	reconciler := condition.NewConditionedReconciler(
		r.Client,
		r.Scheme,
		&runtimev1alpha1.Artifact{},
		artifactReconcileInterval)

	reconciler.SetFinalizer(artifactFinalizerName, r.finalize)
	reconciler.SetCondition(runtimev1alpha1.ArtifactConditionSync, r.reconcileSync)
	reconciler.SetCondition(runtimev1alpha1.ArtifactConditionPublished, r.reconcilePublished)
	reconciler.SetCondition(runtimev1alpha1.ArtifactConditionPrecompiled, r.reconcilePrecompiled)
	reconciler.SetCondition(condition.TypeReady, r.reconcileReady)

	r.reconciler = reconciler

	return ctrl.NewControllerManagedBy(mgr).
		For(&runtimev1alpha1.Artifact{}).
		Named("runtime-artifact").
		Complete(r)
}
