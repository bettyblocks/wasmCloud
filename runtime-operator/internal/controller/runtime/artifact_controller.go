package runtime

import (
	"context"
	"fmt"
	"strings"
	"time"

	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	"go.wasmcloud.dev/runtime-operator/api/condition"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/api/runtime/v1alpha1"
)

const (
	artifactReconcileInterval = 5 * time.Minute
)

// archToWasmtimeTriple maps Kubernetes arch names to Wasmtime cross-compilation target triples.
var archToWasmtimeTriple = map[string]string{
	"amd64": "x86_64-unknown-linux-gnu",
	"arm64": "aarch64-unknown-linux-gnu",
}

var defaultTargetArchitectures = []string{"amd64", "arm64"}

// ArtifactReconciler reconciles a Workload object
type ArtifactReconciler struct {
	client.Client
	Scheme *runtime.Scheme

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
	artifact.Status.CompiledArtifacts = nil

	// Skip remaining steps cause we touched the .Status field
	return condition.ErrSkipReconciliation()
}

func (r *ArtifactReconciler) reconcilePublished(_ context.Context, artifact *runtimev1alpha1.Artifact) error {
	if artifact.Status.ArtifactURL != "" {
		return nil
	}

	return condition.ErrStatusUnknown(fmt.Errorf("artifact not published"))
}

func (r *ArtifactReconciler) reconcileCompile(ctx context.Context, artifact *runtimev1alpha1.Artifact) error {
	if artifact.Spec.CompileImage == "" || artifact.Spec.CompiledImageRepository == "" {
		return nil
	}

	targetArchs := artifact.Spec.TargetArchitectures
	if len(targetArchs) == 0 {
		targetArchs = defaultTargetArchitectures
	}

	jobName := fmt.Sprintf("%s-compile-%d", artifact.Name, artifact.Generation)

	job := &batchv1.Job{}
	err := r.Get(ctx, types.NamespacedName{Name: jobName, Namespace: artifact.Namespace}, job)
	if err != nil && !apierrors.IsNotFound(err) {
		return err
	}

	if apierrors.IsNotFound(err) {
		newJob := r.buildCompileJob(artifact, jobName, targetArchs)
		if err := ctrl.SetControllerReference(artifact, newJob, r.Scheme); err != nil {
			return err
		}
		return r.Create(ctx, newJob)
	}

	if job.Status.Succeeded > 0 {
		if artifact.Status.CompiledArtifacts == nil {
			artifact.Status.CompiledArtifacts = make(map[string]string)
		}
		for _, arch := range targetArchs {
			artifact.Status.CompiledArtifacts[arch] = compiledArtifactURL(artifact, arch)
		}
		condition.ForceStatusUpdate(ctx)
		return nil
	}

	for _, cond := range job.Status.Conditions {
		if cond.Type == batchv1.JobFailed && cond.Status == corev1.ConditionTrue {
			return fmt.Errorf("compile job %s failed: %s", jobName, cond.Message)
		}
	}

	return condition.ErrStatusUnknown(fmt.Errorf("compile job %s is running", jobName))
}

func (r *ArtifactReconciler) reconcileReady(_ context.Context, artifact *runtimev1alpha1.Artifact) error {
	required := []condition.ConditionType{
		runtimev1alpha1.ArtifactConditionSync,
		runtimev1alpha1.ArtifactConditionPublished,
	}
	if artifact.Spec.CompileImage != "" {
		required = append(required, runtimev1alpha1.ArtifactConditionCompiled)
	}

	if !artifact.Status.AllTrue(required...) {
		return fmt.Errorf("artifact is not ready")
	}
	return nil
}

func (r *ArtifactReconciler) buildCompileJob(artifact *runtimev1alpha1.Artifact, jobName string, targetArchs []string) *batchv1.Job {
	script := buildCompileScript(artifact.Status.ArtifactURL, artifact.Spec.CompiledImageRepository, artifact.Generation, targetArchs)

	var imagePullSecrets []corev1.LocalObjectReference
	if artifact.Spec.ImagePullSecret != nil {
		imagePullSecrets = append(imagePullSecrets, *artifact.Spec.ImagePullSecret)
	}
	if artifact.Spec.CompiledImagePushSecret != nil {
		imagePullSecrets = append(imagePullSecrets, *artifact.Spec.CompiledImagePushSecret)
	}

	backoffLimit := int32(3)
	return &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{
			Name:      jobName,
			Namespace: artifact.Namespace,
		},
		Spec: batchv1.JobSpec{
			BackoffLimit: &backoffLimit,
			Template: corev1.PodTemplateSpec{
				Spec: corev1.PodSpec{
					RestartPolicy:    corev1.RestartPolicyNever,
					ImagePullSecrets: imagePullSecrets,
					Containers: []corev1.Container{
						{
							Name:    "compile",
							Image:   artifact.Spec.CompileImage,
							Command: []string{"/bin/sh", "-exc"},
							Args:    []string{script},
						},
					},
				},
			},
		},
	}
}

func buildCompileScript(sourceImage, targetRepository string, generation int64, targetArchs []string) string {
	var sb strings.Builder
	sb.WriteString("mkdir -p /workspace\n")
	sb.WriteString(fmt.Sprintf("oras pull %s -o /workspace/\n", sourceImage))

	for _, arch := range targetArchs {
		triple, ok := archToWasmtimeTriple[arch]
		if !ok {
			continue
		}
		compiledFile := fmt.Sprintf("/workspace/%s.cwasm", arch)
		sb.WriteString(fmt.Sprintf("wasmtime compile --target %s /workspace/*.wasm -o %s\n", triple, compiledFile))
		sb.WriteString(fmt.Sprintf("oras push %s:%s-%d %s:application/vnd.wasmcloud.compiled.wasm\n",
			targetRepository, arch, generation, compiledFile))
	}

	return sb.String()
}

func compiledArtifactURL(artifact *runtimev1alpha1.Artifact, arch string) string {
	return fmt.Sprintf("%s:%s-%d", artifact.Spec.CompiledImageRepository, arch, artifact.Generation)
}

// SetupWithManager sets up the controller with the Manager.
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=artifacts,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=artifacts/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=runtime.wasmcloud.dev,resources=artifacts/finalizers,verbs=update
// +kubebuilder:rbac:groups=batch,resources=jobs,verbs=get;list;watch;create;delete

func (r *ArtifactReconciler) SetupWithManager(mgr ctrl.Manager) error {
	reconciler := condition.NewConditionedReconciler(
		r.Client,
		r.Scheme,
		&runtimev1alpha1.Artifact{},
		artifactReconcileInterval)

	reconciler.SetCondition(runtimev1alpha1.ArtifactConditionSync, r.reconcileSync)
	reconciler.SetCondition(runtimev1alpha1.ArtifactConditionPublished, r.reconcilePublished)
	reconciler.SetCondition(runtimev1alpha1.ArtifactConditionCompiled, r.reconcileCompile)
	reconciler.SetCondition(condition.TypeReady, r.reconcileReady)

	r.reconciler = reconciler

	return ctrl.NewControllerManagedBy(mgr).
		For(&runtimev1alpha1.Artifact{}).
		Owns(&batchv1.Job{}).
		Named("runtime-artifact").
		Complete(r)
}
