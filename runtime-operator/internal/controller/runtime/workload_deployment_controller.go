package runtime

import (
	"context"
	"fmt"
	"strings"
	"time"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

const (
	workloadDeploymentReconcileInterval = 1 * time.Minute
	workloadDeploymentNameIndex         = "workload.deployment.name"
)

// WorkloadDeploymentReconciler reconciles a WorkloadReplicaSet object
type WorkloadDeploymentReconciler struct {
	client.Client
	Scheme     *runtime.Scheme
	reconciler condition.AnyConditionedReconciler

	// DefaultHostPoolTargets is the target matrix used when a WorkloadDeployment
	// omits HostPoolTargets. Wired from operator config in cmd/main.go.
	DefaultHostPoolTargets []string
}

func (r *WorkloadDeploymentReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	return r.reconciler.Reconcile(ctx, req)
}

func (r *WorkloadDeploymentReconciler) reconcileArtifacts(ctx context.Context, deployment *runtimev1alpha1.WorkloadDeployment) error {
	mode := deployment.Spec.PrecompileMode
	if mode == "" {
		mode = runtimev1alpha1.PrecompileModeFallback
	}
	targets := deployment.Spec.HostPoolTargets
	if len(targets) == 0 {
		targets = r.defaultHostPoolTargets()
	}

	for i := range deployment.Spec.Artifacts {
		configArtifact := &deployment.Spec.Artifacts[i]

		// If the WD inlines an image, upsert the corresponding auto-Artifact
		// (no-op when artifactFrom is used).
		if err := upsertInlineArtifact(ctx, r.Client, deployment.Namespace, configArtifact); err != nil {
			return err
		}

		name, err := resolveArtifactName(configArtifact)
		if err != nil {
			return err
		}

		artifact := &runtimev1alpha1.Artifact{}
		if err := r.Get(ctx, client.ObjectKey{Name: name, Namespace: deployment.Namespace}, artifact); err != nil {
			if client.IgnoreNotFound(err) == nil {
				return condition.ErrStatusUnknown(fmt.Errorf("artifact %s not found", name))
			}
			return err
		}
		if !artifact.Status.AllTrue(runtimev1alpha1.ArtifactConditionPublished) {
			return condition.ErrStatusUnknown(fmt.Errorf("artifact %s not published", name))
		}

		// Strict mode: every referenced Artifact must have a precompiled
		// variant for every target in the host pool. Any miss keeps the
		// reconciler requeuing.
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

// defaultHostPoolTargets returns the cluster-wide target matrix used when a
// WorkloadDeployment omits HostPoolTargets. For M2 we hardcode a single
// target; in M3 this moves to operator config.
func (r *WorkloadDeploymentReconciler) defaultHostPoolTargets() []string {
	if len(r.DefaultHostPoolTargets) > 0 {
		return r.DefaultHostPoolTargets
	}
	return []string{"x86_64-unknown-linux-gnu"}
}

// hasPrecompiledVariantForTarget reports whether `artifact.Status.Precompiled`
// contains an entry for `target`. Wasmtime version and compat hash are
// checked on the host side at load time — the WD controller only gates on
// target coverage.
func hasPrecompiledVariantForTarget(artifact *runtimev1alpha1.Artifact, target string) bool {
	for _, v := range artifact.Status.Precompiled {
		if v.Target == target {
			return true
		}
	}
	return false
}

func (r *WorkloadDeploymentReconciler) reconcileSync(ctx context.Context, deployment *runtimev1alpha1.WorkloadDeployment) error {
	if !deployment.Status.AllTrue(runtimev1alpha1.WorkloadDeploymentConditionArtifact) {
		return condition.ErrStatusUnknown(fmt.Errorf("artifacts not ready"))
	}

	if deployment.Status.CurrentReplicaSet == nil {
		return condition.ErrStatusUnknown(fmt.Errorf("no Active Replicas"))
	}

	currentReplica := &runtimev1alpha1.WorkloadReplicaSet{}

	if err := r.Get(ctx, client.ObjectKey{Name: deployment.Status.CurrentReplicaSet.Name, Namespace: deployment.Namespace}, currentReplica); err != nil {
		if client.IgnoreNotFound(err) == nil {
			return condition.ErrStatusUnknown(fmt.Errorf("current ReplicaSet not found"))
		}
		return err
	}

	templateCopy := deployment.Spec.Template.DeepCopy()
	if err := resolveArtifacts(ctx, r.Client, deployment.Namespace, templateCopy, deployment.Spec.Artifacts); err != nil {
		return err
	}
	if deployment.Spec.Kubernetes != nil {
		templateCopy.Spec.Kubernetes = deployment.Spec.Kubernetes.DeepCopy()
	}

	if want, got := currentReplica.Spec.Template.Hash(), templateCopy.Hash(); want != got {
		return fmt.Errorf("template hash mismatch: want %s, got %s", want, got)
	}

	return nil
}

func (r *WorkloadDeploymentReconciler) reconcileDeploy(ctx context.Context, deployment *runtimev1alpha1.WorkloadDeployment) error {
	// Nothing to do if deployment is synced
	if deployment.Status.AllTrue(
		runtimev1alpha1.WorkloadDeploymentConditionArtifact,
		runtimev1alpha1.WorkloadDeploymentConditionSync) {
		return nil
	}

	// cant proceed if artifacts are not ready
	if !deployment.Status.AllTrue(runtimev1alpha1.WorkloadDeploymentConditionArtifact) {
		return condition.ErrStatusUnknown(fmt.Errorf("artifacts not ready"))
	}

	replicaSetTemplate := deployment.Spec.WorkloadReplicaSetSpec.DeepCopy()
	if err := resolveArtifacts(ctx, r.Client, deployment.Namespace, &replicaSetTemplate.Template, deployment.Spec.Artifacts); err != nil {
		return err
	}

	// Propagate deployment-level service reference into each workload's spec
	// so the WorkloadRouteReconciler can find it via the field index.
	if deployment.Spec.Kubernetes != nil {
		replicaSetTemplate.Template.Spec.Kubernetes = deployment.Spec.Kubernetes.DeepCopy()
	}

	replicaSetName := fmt.Sprintf("%s-%s", deployment.Name, randHash())

	newReplicaSet := &runtimev1alpha1.WorkloadReplicaSet{
		ObjectMeta: metav1.ObjectMeta{
			Namespace:   deployment.Namespace,
			Name:        replicaSetName,
			Labels:      deployment.Labels,
			Annotations: deployment.Annotations,
		},
		Spec: *replicaSetTemplate,
	}
	if err := controllerutil.SetControllerReference(deployment, newReplicaSet, r.Scheme); err != nil {
		return err
	}
	if err := r.Create(ctx, newReplicaSet); err != nil {
		return err
	}

	if deployment.Spec.DeployPolicy != runtimev1alpha1.WorkloadDeployPolicyRecreate {
		// the default policy is rolling
		deployment.Status.PreviousReplicaSet = deployment.Status.CurrentReplicaSet
	}
	deployment.Status.CurrentReplicaSet = &corev1.LocalObjectReference{Name: newReplicaSet.GetName()}

	// Skip remaining reconciliation steps & update API Server because we touched a '.Status' field
	condition.ForceStatusUpdate(ctx)
	return condition.ErrSkipReconciliation()
}

func (r *WorkloadDeploymentReconciler) reconcileScale(ctx context.Context, deployment *runtimev1alpha1.WorkloadDeployment) error {
	if !deployment.Status.AllTrue(runtimev1alpha1.WorkloadDeploymentConditionSync, runtimev1alpha1.WorkloadDeploymentConditionDeploy) {
		return condition.ErrStatusUnknown(fmt.Errorf("deployment is not synced or not deployed"))
	}

	if deployment.Status.CurrentReplicaSet == nil {
		return fmt.Errorf("current ReplicaSet is nil")
	}
	curSetName := deployment.Status.CurrentReplicaSet.Name

	var prevSetName string
	if deployment.Status.PreviousReplicaSet != nil {
		prevSetName = deployment.Status.PreviousReplicaSet.Name
	}

	currentReplica := &runtimev1alpha1.WorkloadReplicaSet{}
	if err := r.Get(ctx, client.ObjectKey{
		Namespace: deployment.Namespace,
		Name:      curSetName,
	}, currentReplica); err != nil {
		return err
	}

	// If all we touched was the replica-count, update-it
	if deployment.Spec.Replicas != nil {
		currentReplica.Spec.Replicas = deployment.Spec.Replicas
		if err := r.Update(ctx, currentReplica); err != nil {
			return err
		}
	}

	expectedReplicas := int32(0)
	if deployment.Spec.Replicas != nil {
		expectedReplicas = *deployment.Spec.Replicas
	}
	currentReplicas := int32(0)
	readyReplicas := int32(0)
	unavailableReplicas := int32(0)

	// Delete any ReplicaSet except current & previous
	replicaSets := &runtimev1alpha1.WorkloadReplicaSetList{}
	if err := r.List(ctx, replicaSets, client.InNamespace(deployment.Namespace), client.MatchingFields{workloadDeploymentNameIndex: deployment.Name}); err != nil {
		return err
	}
	for _, rs := range replicaSets.Items {
		if rsStatus := rs.Status.Replicas; rsStatus != nil {
			currentReplicas += rsStatus.Current
			readyReplicas += rsStatus.Ready
			unavailableReplicas += rsStatus.Unavailable
		}
		if rs.Name == curSetName || rs.Name == prevSetName {
			continue
		}
		if err := r.Delete(ctx, &rs); err != nil {
			return err
		}
	}

	condition.ForceStatusUpdate(ctx)
	deploymentStatus := &runtimev1alpha1.ReplicaSetStatus{
		Expected:    expectedReplicas,
		Current:     currentReplicas,
		Ready:       readyReplicas,
		Unavailable: unavailableReplicas,
	}
	deployment.Status.Replicas = deploymentStatus

	if prevSetName != "" {
		switch deployment.Spec.DeployPolicy {
		case runtimev1alpha1.WorkloadDeployPolicyRecreate:
			// For Policy=Recreate, delete previous replica set too
		default:
			// For Policy=RollingUpdate, only delete previous replica set when current replica set is ready
			if !currentReplica.Status.IsAvailable() {
				return fmt.Errorf("current ReplicaSet is not available yet")
			}
		}

		if err := r.Delete(ctx, &runtimev1alpha1.WorkloadReplicaSet{
			ObjectMeta: metav1.ObjectMeta{
				Name:      prevSetName,
				Namespace: deployment.Namespace,
			},
		}); err != nil {
			if client.IgnoreNotFound(err) != nil {
				return err
			}
		}

		deployment.Status.PreviousReplicaSet = nil
		return condition.ErrSkipReconciliation()
	}

	return nil
}

func (r *WorkloadDeploymentReconciler) reconcileReady(_ context.Context, deployment *runtimev1alpha1.WorkloadDeployment) error {
	if !deployment.Status.AllTrue(
		runtimev1alpha1.WorkloadDeploymentConditionArtifact,
		runtimev1alpha1.WorkloadDeploymentConditionSync,
		runtimev1alpha1.WorkloadDeploymentConditionDeploy,
		runtimev1alpha1.WorkloadDeploymentConditionScale) {
		return condition.ErrStatusUnknown(fmt.Errorf("deployment is not synced, deployed or scaled"))
	}
	return nil
}

// SetupWithManager sets up the controller with the Manager.
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=workloaddeployments,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=workloaddeployments/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=workloaddeployments/finalizers,verbs=update

func (r *WorkloadDeploymentReconciler) SetupWithManager(mgr ctrl.Manager) error {
	reconciler := condition.NewConditionedReconciler(
		r.Client,
		r.Scheme,
		&runtimev1alpha1.WorkloadDeployment{},
		workloadDeploymentReconcileInterval)

	reconciler.SetCondition(runtimev1alpha1.WorkloadDeploymentConditionArtifact, r.reconcileArtifacts)
	reconciler.SetCondition(runtimev1alpha1.WorkloadDeploymentConditionSync, r.reconcileSync)
	reconciler.SetCondition(runtimev1alpha1.WorkloadDeploymentConditionDeploy, r.reconcileDeploy)
	reconciler.SetCondition(runtimev1alpha1.WorkloadDeploymentConditionScale, r.reconcileScale)
	reconciler.SetCondition(condition.TypeReady, r.reconcileReady)

	r.reconciler = reconciler

	// NOTE(lxf): We only touch Replicas that have been setup via Deployment
	deploymentGvk, err := gvkForType(&runtimev1alpha1.WorkloadDeployment{}, r.Scheme)
	if err != nil {
		return err
	}

	err = mgr.GetFieldIndexer().IndexField(context.Background(), &runtimev1alpha1.WorkloadReplicaSet{}, workloadDeploymentNameIndex, func(rawObj client.Object) []string {
		if ownerName, ok := isOwnedByController(rawObj, deploymentGvk); ok {
			return []string{ownerName}
		}

		return []string{}
	})
	if err != nil {
		return err
	}

	return ctrl.NewControllerManagedBy(mgr).
		For(&runtimev1alpha1.WorkloadDeployment{}).
		Owns(&runtimev1alpha1.WorkloadReplicaSet{}).
		Named("workload-deployment").
		Complete(r)
}

func resolveArtifacts(ctx context.Context, kubeClient client.Client, namespace string, tpl *runtimev1alpha1.WorkloadReplicaTemplate, artifactsFrom []runtimev1alpha1.WorkloadDeploymentArtifact) error {
	artifactMap := make(map[string]runtimev1alpha1.Artifact)
	for i := range artifactsFrom {
		a := &artifactsFrom[i]
		name, err := resolveArtifactName(a)
		if err != nil {
			return err
		}
		artifact := &runtimev1alpha1.Artifact{}
		if err := kubeClient.Get(ctx, client.ObjectKey{Name: name, Namespace: namespace}, artifact); err != nil {
			if client.IgnoreNotFound(err) == nil {
				return condition.ErrStatusUnknown(fmt.Errorf("artifact %s not found", name))
			}
			return err
		}
		artifactMap[a.Name] = *artifact
	}

	for i, comp := range tpl.Spec.Components {
		if !strings.HasPrefix(comp.Image, "artifact://") {
			continue
		}
		artifactName := strings.TrimPrefix(comp.Image, "artifact://")
		artifact, ok := artifactMap[artifactName]
		if !ok {
			return fmt.Errorf("artifact %s not found in deployment spec", artifactName)
		}
		comp.Image = artifact.Status.ArtifactURL
		comp.Precompiled = append([]runtimev1alpha1.PrecompiledVariant(nil), artifact.Status.Precompiled...)
		tpl.Spec.Components[i] = comp
	}

	if tpl.Spec.Service != nil {
		if strings.HasPrefix(tpl.Spec.Service.Image, "artifact://") {
			artifactName := strings.TrimPrefix(tpl.Spec.Service.Image, "artifact://")
			artifact, ok := artifactMap[artifactName]
			if !ok {
				return fmt.Errorf("artifact %s not found in deployment spec", artifactName)
			}
			tpl.Spec.Service.Image = artifact.Status.ArtifactURL
			tpl.Spec.Service.Precompiled = append([]runtimev1alpha1.PrecompiledVariant(nil), artifact.Status.Precompiled...)
		}
	}

	return nil
}
