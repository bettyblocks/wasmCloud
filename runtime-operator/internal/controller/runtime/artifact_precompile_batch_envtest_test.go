package runtime

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/config"
	"sigs.k8s.io/controller-runtime/pkg/envtest"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// TestBatchPrecompileAgainstAPIServer runs the batch reconciler in a manager
// against a real API server: the server must accept the Indexed Job (with
// backoffLimitPerIndex and podFailurePolicy), and a Job status update must
// reach every Artifact of the batch through the owner watch.
func TestBatchPrecompileAgainstAPIServer(t *testing.T) {
	if os.Getenv("KUBEBUILDER_ASSETS") == "" {
		t.Skip("KUBEBUILDER_ASSETS not set; run via `make test` for envtest")
	}
	env := &envtest.Environment{
		CRDDirectoryPaths:     []string{filepath.Join("..", "..", "..", "config", "crd", "bases")},
		ErrorIfCRDPathMissing: true,
	}
	cfg, err := env.Start()
	if err != nil {
		t.Fatalf("start envtest: %v", err)
	}
	t.Cleanup(func() {
		if err := env.Stop(); err != nil {
			t.Errorf("stop envtest: %v", err)
		}
	})

	scheme := newBatchTestScheme(t)
	mgr, err := ctrl.NewManager(cfg, ctrl.Options{
		Scheme:     scheme,
		Metrics:    metricsserver.Options{BindAddress: "0"},
		Controller: config.Controller{SkipNameValidation: ptrTo(true)},
	})
	if err != nil {
		t.Fatal(err)
	}
	r := &PrecompileReconciler{
		Client:          mgr.GetClient(),
		Scheme:          scheme,
		WorkerImage:     "ghcr.io/x/wash-precompile:test",
		ArtifactStore:   ArtifactStoreConfig{BaseURL: "nats://store"},
		Target:          testTarget,
		WasmtimeVersion: testWasmtimeVersion,
		Batch: &PrecompileBatchConfig{
			Window: 100 * time.Millisecond, MaxParallelPods: 2, MaxItemsPerJob: 100,
			APIReader: mgr.GetAPIReader(),
		},
	}
	if err := r.SetupWithManager(mgr); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	go func() {
		if err := mgr.Start(ctx); err != nil {
			t.Errorf("manager: %v", err)
		}
	}()

	c, err := client.New(cfg, client.Options{Scheme: scheme})
	if err != nil {
		t.Fatal(err)
	}
	createTestNamespace(t, ctx, c, batchTestNS)
	for _, n := range []string{"a", "b", "c"} {
		a := &runtimev1alpha1.Artifact{
			ObjectMeta: metav1.ObjectMeta{Name: n, Namespace: batchTestNS},
			Spec:       runtimev1alpha1.ArtifactSpec{Image: "registry.local/" + n + ":1"},
		}
		if err := c.Create(ctx, a); err != nil {
			t.Fatal(err)
		}
	}

	var job batchv1.Job
	eventually(t, func() bool {
		var jobs batchv1.JobList
		if err := c.List(ctx, &jobs, client.InNamespace(batchTestNS)); err != nil || len(jobs.Items) != 1 {
			return false
		}
		job = jobs.Items[0]
		return true
	}, "one batch Job for three Artifacts")

	if *job.Spec.Completions != 2 || *job.Spec.CompletionMode != batchv1.IndexedCompletion {
		t.Errorf("unexpected Job spec: completions %d mode %s", *job.Spec.Completions, *job.Spec.CompletionMode)
	}
	if len(job.OwnerReferences) != 3 {
		t.Errorf("every Artifact must own the Job: %v", job.OwnerReferences)
	}
	var cm corev1.ConfigMap
	if err := c.Get(ctx, types.NamespacedName{Namespace: batchTestNS, Name: job.Name}, &cm); err != nil {
		t.Fatalf("ConfigMap: %v", err)
	}
	if len(cm.OwnerReferences) != 1 || cm.OwnerReferences[0].UID != job.UID {
		t.Errorf("the Job must own the ConfigMap: %v", cm.OwnerReferences)
	}

	// No Job controller runs in envtest; report index 1 complete by hand.
	job.Status.CompletedIndexes = "1"
	job.Status.Succeeded = 1
	job.Status.StartTime = ptrTo(metav1.Now())
	if err := c.Status().Update(ctx, &job); err != nil {
		t.Fatalf("update Job status: %v", err)
	}

	items, err := batchItemsOf(&job)
	if err != nil {
		t.Fatal(err)
	}
	for _, it := range items {
		want := it.Index == 1
		eventually(t, func() bool {
			var a runtimev1alpha1.Artifact
			if err := c.Get(ctx, types.NamespacedName{Namespace: batchTestNS, Name: it.Artifact}, &a); err != nil {
				return false
			}
			precompiled := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompiled).Status == condition.ConditionTrue
			progressing := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing)
			if want {
				return precompiled && len(a.Status.Precompiled) == 1
			}
			return !precompiled && progressing.Reason == reasonBatchJobInFlight
		}, "status of "+it.Artifact)
	}
}

func eventually(t *testing.T, cond func() bool, what string) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if cond() {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for: %s", what)
}
