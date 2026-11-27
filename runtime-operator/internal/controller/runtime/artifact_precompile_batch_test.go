package runtime

import (
	"context"
	"encoding/json"
	"strings"
	"sync"
	"testing"
	"time"

	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"
	"sigs.k8s.io/controller-runtime/pkg/event"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

const batchTestNS = "apps"

func TestParseIndexes(t *testing.T) {
	got, err := parseIndexes("0,2-4,7")
	if err != nil {
		t.Fatal(err)
	}
	for _, i := range []int{0, 2, 3, 4, 7} {
		if !got[i] {
			t.Errorf("index %d missing from %v", i, got)
		}
	}
	if len(got) != 5 {
		t.Errorf("expected 5 indexes, got %v", got)
	}
	if got, err := parseIndexes(""); err != nil || len(got) != 0 {
		t.Errorf("empty list: %v, %v", got, err)
	}
	for _, bad := range []string{"a", "1-", "3-1", "1,,2"} {
		if _, err := parseIndexes(bad); err == nil {
			t.Errorf("%q: expected an error", bad)
		}
	}
}

func TestItemResultFromMessage(t *testing.T) {
	msg := `{"total":3,"ok":[0,2],"failed":[{"index":1,"permanent":true,"error":"manifest unknown"}]}`

	if s, ok := itemResultFromMessage(msg, 0, 2); !ok || !s.succeeded {
		t.Errorf("item 0: %+v %v", s, ok)
	}
	s, ok := itemResultFromMessage(msg, 1, 2)
	if !ok || s.succeeded || !s.permanent || s.message != "manifest unknown" {
		t.Errorf("item 1: %+v %v", s, ok)
	}
	if _, ok := itemResultFromMessage(msg, 5, 2); ok {
		t.Error("item 5 is not in the message")
	}
	if _, ok := itemResultFromMessage("", 0, 1); ok {
		t.Error("an empty message has no result")
	}
	if _, ok := itemResultFromMessage("Error: oom", 0, 137); ok {
		t.Error("a message that is not JSON has no result")
	}
	s, ok = itemResultFromMessage(`{"error":"bad batch file","permanent":true}`, 3, 2)
	if !ok || s.succeeded || !s.permanent {
		t.Errorf("fatal message: %+v %v", s, ok)
	}
}

// --- batcher ---

type flushRecorder struct {
	mu      sync.Mutex
	flushes [][]types.NamespacedName
	ch      chan struct{}
}

func newFlushRecorder() *flushRecorder { return &flushRecorder{ch: make(chan struct{}, 100)} }

func (f *flushRecorder) flush(_ batchKey, names []types.NamespacedName) {
	f.mu.Lock()
	f.flushes = append(f.flushes, names)
	f.mu.Unlock()
	f.ch <- struct{}{}
}

func (f *flushRecorder) wait(t *testing.T, n int) [][]types.NamespacedName {
	t.Helper()
	for range n {
		select {
		case <-f.ch:
		case <-time.After(5 * time.Second):
			t.Fatalf("timed out waiting for %d flushes", n)
		}
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.flushes
}

func nn(name string) types.NamespacedName {
	return types.NamespacedName{Namespace: batchTestNS, Name: name}
}

func TestBatcherCollectsArtifactsInOneWindow(t *testing.T) {
	rec := newFlushRecorder()
	b := newArtifactBatcher(50*time.Millisecond, 100, rec.flush)
	key := batchKey{Namespace: batchTestNS}

	for _, n := range []string{"a", "b", "c", "a"} {
		b.Add(key, nn(n))
	}
	if !b.Pending(key, nn("a")) {
		t.Error("a must be pending before the window ends")
	}
	flushes := rec.wait(t, 1)
	if len(flushes) != 1 || len(flushes[0]) != 3 {
		t.Fatalf("expected one flush of 3 Artifacts, got %v", flushes)
	}
}

func TestBatcherFlushesAtMaxItems(t *testing.T) {
	rec := newFlushRecorder()
	b := newArtifactBatcher(time.Hour, 2, rec.flush)
	key := batchKey{Namespace: batchTestNS}

	b.Add(key, nn("a"))
	b.Add(key, nn("b"))
	flushes := rec.wait(t, 1)
	if len(flushes[0]) != 2 {
		t.Fatalf("expected a flush of 2, got %v", flushes)
	}
}

func TestBatcherKeepsKeysApart(t *testing.T) {
	rec := newFlushRecorder()
	b := newArtifactBatcher(20*time.Millisecond, 100, rec.flush)

	b.Add(batchKey{Namespace: "x"}, types.NamespacedName{Namespace: "x", Name: "a"})
	b.Add(batchKey{Namespace: "x", PullSecret: "s"}, types.NamespacedName{Namespace: "x", Name: "b"})
	b.Add(batchKey{Namespace: "y"}, types.NamespacedName{Namespace: "y", Name: "c"})
	if flushes := rec.wait(t, 3); len(flushes) != 3 {
		t.Fatalf("expected 3 flushes, got %v", flushes)
	}
}

func TestBatcherZeroWindowFlushesAtOnce(t *testing.T) {
	rec := newFlushRecorder()
	b := newArtifactBatcher(0, 100, rec.flush)
	b.Add(batchKey{Namespace: batchTestNS}, nn("a"))
	if flushes := rec.wait(t, 1); len(flushes[0]) != 1 {
		t.Fatalf("got %v", flushes)
	}
}

func TestBatcherMaxDelayBoundsATrickle(t *testing.T) {
	rec := newFlushRecorder()
	b := newArtifactBatcher(40*time.Millisecond, 1000, rec.flush)
	key := batchKey{Namespace: batchTestNS}

	start := time.Now()
	stop := time.After(400 * time.Millisecond)
	tick := time.NewTicker(10 * time.Millisecond)
	defer tick.Stop()
	n := 0
loop:
	for {
		select {
		case <-tick.C:
			b.Add(key, nn(string(rune('a'+n%26))+strings.Repeat("x", n/26)))
			n++
		case <-rec.ch:
			break loop
		case <-stop:
			t.Fatal("a steady trickle kept the group from flushing")
		}
	}
	// maxDelay is 4x the window (160 ms); allow scheduler noise.
	if took := time.Since(start); took > 350*time.Millisecond {
		t.Errorf("flush took %v", took)
	}
}

func TestBatcherRecentlyAssigned(t *testing.T) {
	b := newArtifactBatcher(time.Second, 10, func(batchKey, []types.NamespacedName) {})
	b.Assigned(nn("a"), "nats://s/a/v1")
	if !b.RecentlyAssigned(nn("a"), "nats://s/a/v1") {
		t.Error("a was just assigned")
	}
	if b.RecentlyAssigned(nn("a"), "nats://s/a/v2") {
		t.Error("a new output (image) is not assigned")
	}
	b.now = func() time.Time { return time.Now().Add(2 * assignmentTTL) }
	if b.RecentlyAssigned(nn("a"), "nats://s/a/v1") {
		t.Error("the assignment must expire")
	}
}

// --- Job spec ---

func newBatchTestScheme(t *testing.T) *runtime.Scheme {
	t.Helper()
	s := runtime.NewScheme()
	for _, add := range []func(*runtime.Scheme) error{
		corev1.AddToScheme, batchv1.AddToScheme, runtimev1alpha1.AddToScheme,
	} {
		if err := add(s); err != nil {
			t.Fatal(err)
		}
	}
	return s
}

func batchArtifact(name, image string) *runtimev1alpha1.Artifact {
	return &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{
			Name: name, Namespace: batchTestNS, UID: types.UID("uid-" + name), Generation: 1,
		},
		Spec: runtimev1alpha1.ArtifactSpec{Image: image},
	}
}

func newBatchReconciler(t *testing.T, maxPods int, objs ...client.Object) (*PrecompileReconciler, client.Client) {
	t.Helper()
	s := newBatchTestScheme(t)
	c := fake.NewClientBuilder().
		WithScheme(s).
		WithObjects(objs...).
		WithStatusSubresource(&runtimev1alpha1.Artifact{}, &batchv1.Job{}).
		Build()
	r := &PrecompileReconciler{
		Client:          c,
		Scheme:          s,
		WorkerImage:     "ghcr.io/x/wash-precompile:test",
		ArtifactStore:   ArtifactStoreConfig{BaseURL: "nats://store", Env: []corev1.EnvVar{{Name: "NATS_URL", Value: "nats://n:4222"}}},
		Target:          testTarget,
		WasmtimeVersion: testWasmtimeVersion,
		Batch:           &PrecompileBatchConfig{Window: time.Hour, MaxParallelPods: maxPods, MaxItemsPerJob: 1000, APIReader: c},
		requeue:         make(chan event.GenericEvent, 100),
	}
	// A window of an hour: tests call flushBatch themselves.
	r.batcher = newArtifactBatcher(time.Hour, 1000, func(batchKey, []types.NamespacedName) {})
	return r, c
}

func TestBuildBatchSpreadsItemsOverIndexes(t *testing.T) {
	r, _ := newBatchReconciler(t, 2)
	arts := []*runtimev1alpha1.Artifact{
		batchArtifact("a", "r/a:1"), batchArtifact("b", "r/b:1"), batchArtifact("c", "r/c:1"),
	}
	job, cm, items := r.buildBatch(batchKey{Namespace: batchTestNS}, arts)

	if *job.Spec.Completions != 2 || *job.Spec.Parallelism != 2 {
		t.Errorf("completions/parallelism: %d/%d", *job.Spec.Completions, *job.Spec.Parallelism)
	}
	if *job.Spec.CompletionMode != batchv1.IndexedCompletion {
		t.Error("the Job must be Indexed")
	}
	if *job.Spec.MaxFailedIndexes != 2 || *job.Spec.BackoffLimitPerIndex != batchBackoffLimitPerIndex {
		t.Error("one failed index must not fail the others")
	}
	if job.Spec.TTLSecondsAfterFinished == nil {
		t.Error("finished batch Jobs must be cleaned up")
	}
	rules := job.Spec.PodFailurePolicy.Rules
	if rules[0].Action != batchv1.PodFailurePolicyActionFailIndex ||
		rules[0].OnExitCodes.Values[0] != workerExitPermanent {
		t.Errorf("exit code %d must fail the index without retry: %+v", workerExitPermanent, rules[0])
	}
	if job.Spec.Template.Spec.RestartPolicy != corev1.RestartPolicyNever {
		t.Error("podFailurePolicy needs restartPolicy Never")
	}

	wantIndex := map[string][2]int{"a": {0, 0}, "b": {1, 0}, "c": {0, 1}}
	for _, it := range items {
		if got := [2]int{it.Index, it.Item}; got != wantIndex[it.Artifact] {
			t.Errorf("%s: index/item %v, want %v", it.Artifact, got, wantIndex[it.Artifact])
		}
	}
	if cm.Data["0"] != "r/a:1 "+items[0].Output+"\nr/c:1 "+items[2].Output+"\n" {
		t.Errorf("batch file 0: %q", cm.Data["0"])
	}
	if strings.Count(cm.Data["1"], "\n") != 1 {
		t.Errorf("batch file 1: %q", cm.Data["1"])
	}
	if cm.Name != job.Name || job.Spec.Template.Spec.Volumes[0].ConfigMap.Name != job.Name {
		t.Error("the Job must mount its own ConfigMap")
	}
	if len(job.OwnerReferences) != 3 {
		t.Errorf("every Artifact must own the Job: %v", job.OwnerReferences)
	}
	for _, o := range job.OwnerReferences {
		if o.Controller != nil && *o.Controller {
			t.Error("a batch Job has no single controller")
		}
	}
	c := job.Spec.Template.Spec.Containers[0]
	if strings.Join(c.Args, " ") != "--batch-file /etc/precompile-batch/$(JOB_COMPLETION_INDEX)" {
		t.Errorf("args: %v", c.Args)
	}
	got, err := batchItemsOf(job)
	if err != nil || len(got) != 3 {
		t.Errorf("item manifest: %v %v", got, err)
	}
}

func TestBuildBatchUsesFewerPodsThanMaxForSmallBatches(t *testing.T) {
	r, _ := newBatchReconciler(t, 10)
	job, cm, _ := r.buildBatch(batchKey{Namespace: batchTestNS}, []*runtimev1alpha1.Artifact{batchArtifact("a", "r/a:1")})
	if *job.Spec.Completions != 1 || len(cm.Data) != 1 {
		t.Errorf("one Artifact needs one pod, got %d", *job.Spec.Completions)
	}
}

func TestBuildBatchMountsThePullSecret(t *testing.T) {
	r, _ := newBatchReconciler(t, 10)
	job, _, _ := r.buildBatch(batchKey{Namespace: batchTestNS, PullSecret: "regcred"},
		[]*runtimev1alpha1.Artifact{batchArtifact("a", "r/a:1")})
	vols := job.Spec.Template.Spec.Volumes
	if len(vols) != 2 || vols[1].Secret == nil || vols[1].Secret.SecretName != "regcred" {
		t.Errorf("volumes: %+v", vols)
	}
}

// --- reconcile flow (fake client) ---

func reconcileArtifact(t *testing.T, r *PrecompileReconciler, name string) {
	t.Helper()
	if _, err := r.Reconcile(context.Background(), ctrl.Request{NamespacedName: nn(name)}); err != nil {
		t.Fatalf("reconcile %s: %v", name, err)
	}
}

func getArtifact(t *testing.T, c client.Client, name string) *runtimev1alpha1.Artifact {
	t.Helper()
	var a runtimev1alpha1.Artifact
	if err := c.Get(context.Background(), nn(name), &a); err != nil {
		t.Fatal(err)
	}
	return &a
}

func onlyBatchJob(t *testing.T, c client.Client) *batchv1.Job {
	t.Helper()
	var jobs batchv1.JobList
	if err := c.List(context.Background(), &jobs, client.InNamespace(batchTestNS)); err != nil {
		t.Fatal(err)
	}
	if len(jobs.Items) != 1 {
		t.Fatalf("expected one batch Job, got %d", len(jobs.Items))
	}
	return &jobs.Items[0]
}

func setJobStatus(t *testing.T, c client.Client, job *batchv1.Job, completed, failed string, conds ...batchv1.JobCondition) {
	t.Helper()
	job.Status.CompletedIndexes = completed
	if failed != "" {
		job.Status.FailedIndexes = &failed
	}
	job.Status.Conditions = conds
	if err := c.Status().Update(context.Background(), job); err != nil {
		t.Fatal(err)
	}
}

func addWorkerPod(t *testing.T, c client.Client, job *batchv1.Job, index int, exitCode int32, msg string) {
	t.Helper()
	p := &corev1.Pod{
		ObjectMeta: metav1.ObjectMeta{
			Name:      job.Name + "-" + string(rune('0'+index)),
			Namespace: job.Namespace,
			Labels:    map[string]string{batchv1.JobNameLabel: job.Name},
			Annotations: map[string]string{
				batchv1.JobCompletionIndexAnnotation: string(rune('0' + index)),
			},
			OwnerReferences: []metav1.OwnerReference{{
				APIVersion: "batch/v1", Kind: "Job", Name: job.Name, UID: job.UID,
			}},
		},
		Status: corev1.PodStatus{ContainerStatuses: []corev1.ContainerStatus{{
			Name: precompileContainerName,
			State: corev1.ContainerState{Terminated: &corev1.ContainerStateTerminated{
				ExitCode: exitCode, Message: msg, FinishedAt: metav1.Now(),
			}},
		}}},
	}
	if err := c.Create(context.Background(), p); err != nil {
		t.Fatal(err)
	}
}

func cond(a *runtimev1alpha1.Artifact, ct condition.ConditionType) condition.Condition {
	return a.Status.GetCondition(ct)
}

func TestBatchFlowRecordsResultsPerArtifact(t *testing.T) {
	a, b, c := batchArtifact("a", "r/a:1"), batchArtifact("b", "r/b:1"), batchArtifact("c", "r/c:1")
	r, cl := newBatchReconciler(t, 2, a, b, c)

	for _, n := range []string{"a", "b", "c"} {
		reconcileArtifact(t, r, n)
		if !r.batcher.Pending(batchKey{Namespace: batchTestNS}, nn(n)) {
			t.Fatalf("%s must be queued", n)
		}
		if got := cond(getArtifact(t, cl, n), runtimev1alpha1.ArtifactConditionPrecompileProgressing); got.Reason != reasonBatchQueued {
			t.Errorf("%s: progressing reason %q", n, got.Reason)
		}
	}

	r.flushBatch(batchKey{Namespace: batchTestNS}, []types.NamespacedName{nn("a"), nn("b"), nn("c")})
	job := onlyBatchJob(t, cl)
	var cm corev1.ConfigMap
	if err := cl.Get(context.Background(), types.NamespacedName{Namespace: batchTestNS, Name: job.Name}, &cm); err != nil {
		t.Fatalf("ConfigMap: %v", err)
	}
	if len(cm.OwnerReferences) != 1 || cm.OwnerReferences[0].Name != job.Name {
		t.Errorf("the Job must own the ConfigMap: %v", cm.OwnerReferences)
	}

	// Index 0 (a, c) is running, index 1 (b) completed.
	setJobStatus(t, cl, job, "1", "")
	for _, n := range []string{"a", "b", "c"} {
		reconcileArtifact(t, r, n)
	}
	if got := getArtifact(t, cl, "b"); len(got.Status.Precompiled) != 1 ||
		cond(got, runtimev1alpha1.ArtifactConditionPrecompiled).Status != condition.ConditionTrue {
		t.Errorf("b must be precompiled: %+v", got.Status)
	}
	if got := getArtifact(t, cl, "a"); len(got.Status.Precompiled) != 0 ||
		cond(got, runtimev1alpha1.ArtifactConditionPrecompileProgressing).Reason != reasonBatchJobInFlight {
		t.Errorf("a must still be in flight: %+v", got.Status)
	}

	// Index 0 fails: a (item 0) was fine, c (item 1) has a permanent error.
	addWorkerPod(t, cl, job, 0, workerExitPermanent,
		`{"total":2,"ok":[0],"failed":[{"index":1,"permanent":true,"error":"invalid wasm"}]}`)
	setJobStatus(t, cl, job, "1", "0", batchv1.JobCondition{Type: batchv1.JobFailed, Status: corev1.ConditionTrue})
	for _, n := range []string{"a", "c"} {
		reconcileArtifact(t, r, n)
	}
	if got := getArtifact(t, cl, "a"); len(got.Status.Precompiled) != 1 {
		t.Errorf("a succeeded inside a failed index and must be precompiled: %+v", got.Status)
	}
	got := getArtifact(t, cl, "c")
	failed := cond(got, runtimev1alpha1.ArtifactConditionPrecompileFailed)
	if failed.Status != condition.ConditionTrue || failed.Reason != reasonBatchPermanentErr || failed.Message != "invalid wasm" {
		t.Errorf("c must have failed permanently: %+v", failed)
	}
	if len(got.Status.Precompiled) != 0 {
		t.Error("c must not be precompiled")
	}

	// A failed Artifact is not queued again until its spec changes.
	r.batcher = newArtifactBatcher(time.Hour, 1000, func(batchKey, []types.NamespacedName) {})
	reconcileArtifact(t, r, "c")
	if r.batcher.Pending(batchKey{Namespace: batchTestNS}, nn("c")) {
		t.Error("a failed Artifact must not be queued again for the same spec")
	}
}

func TestBatchFailedIndexWithoutMessageFailsItsItems(t *testing.T) {
	a := batchArtifact("a", "r/a:1")
	r, cl := newBatchReconciler(t, 1, a)
	r.flushBatch(batchKey{Namespace: batchTestNS}, []types.NamespacedName{nn("a")})
	job := onlyBatchJob(t, cl)
	// OOM kill: no termination message.
	addWorkerPod(t, cl, job, 0, 137, "")
	setJobStatus(t, cl, job, "", "0")

	reconcileArtifact(t, r, "a")
	failed := cond(getArtifact(t, cl, "a"), runtimev1alpha1.ArtifactConditionPrecompileFailed)
	if failed.Status != condition.ConditionTrue || failed.Reason != reasonBatchFailed {
		t.Errorf("a must have failed: %+v", failed)
	}
}

func TestBatchImageChangeDuringABatchQueuesTheNewImage(t *testing.T) {
	a := batchArtifact("a", "r/a:1")
	r, cl := newBatchReconciler(t, 1, a)
	r.flushBatch(batchKey{Namespace: batchTestNS}, []types.NamespacedName{nn("a")})
	job := onlyBatchJob(t, cl)

	cur := getArtifact(t, cl, "a")
	cur.Spec.Image = "r/a:2"
	if err := cl.Update(context.Background(), cur); err != nil {
		t.Fatal(err)
	}
	// The old batch completes; its result is for the old image.
	setJobStatus(t, cl, job, "0", "", batchv1.JobCondition{Type: batchv1.JobComplete, Status: corev1.ConditionTrue})

	reconcileArtifact(t, r, "a")
	got := getArtifact(t, cl, "a")
	if len(got.Status.Precompiled) != 0 {
		t.Errorf("the result of the old image must be ignored: %+v", got.Status.Precompiled)
	}
	if !r.batcher.Pending(batchKey{Namespace: batchTestNS}, nn("a")) {
		t.Error("the new image must be queued")
	}
}

func TestBatchDoesNotQueueAnArtifactThatWasJustAssigned(t *testing.T) {
	a := batchArtifact("a", "r/a:1")
	r, cl := newBatchReconciler(t, 1, a)
	r.flushBatch(batchKey{Namespace: batchTestNS}, []types.NamespacedName{nn("a")})
	// Simulate a cache that does not see the Job yet.
	job := onlyBatchJob(t, cl)
	if err := cl.Delete(context.Background(), job); err != nil {
		t.Fatal(err)
	}
	reconcileArtifact(t, r, "a")
	if r.batcher.Pending(batchKey{Namespace: batchTestNS}, nn("a")) {
		t.Error("an Artifact that was just put in a Job must not be queued again")
	}
}

func TestBatchSkipsArtifactsThatAreAlreadyPrecompiled(t *testing.T) {
	a := batchArtifact("a", "r/a:1")
	a.Status.Precompiled = []runtimev1alpha1.PrecompiledVariant{{
		Target: testTarget, WasmtimeVersion: testWasmtimeVersion, ImageRef: "r/a:1",
	}}
	r, cl := newBatchReconciler(t, 1, a)
	r.flushBatch(batchKey{Namespace: batchTestNS}, []types.NamespacedName{nn("a")})
	var jobs batchv1.JobList
	if err := cl.List(context.Background(), &jobs); err != nil {
		t.Fatal(err)
	}
	if len(jobs.Items) != 0 {
		t.Error("no Job for an Artifact that is already precompiled")
	}
}

func TestBatchItemManifestRoundTrips(t *testing.T) {
	items := []batchItem{{Artifact: "a", UID: "u", Image: "r/a:1", Output: "nats://s/a", Index: 1, Item: 2}}
	raw, _ := json.Marshal(items)
	job := &batchv1.Job{ObjectMeta: metav1.ObjectMeta{
		Annotations: map[string]string{precompileBatchItemsAnnotation: string(raw)},
	}}
	got, err := batchItemsOf(job)
	if err != nil || len(got) != 1 || got[0] != items[0] {
		t.Errorf("got %v, %v", got, err)
	}
	if _, err := batchItemsOf(&batchv1.Job{}); err == nil {
		t.Error("a Job without a manifest must error")
	}
}
