package runtime

import (
	"context"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"
	"sigs.k8s.io/controller-runtime/pkg/event"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

func TestOutputKeyOf(t *testing.T) {
	cases := []struct {
		base, url, want string
		ok              bool
	}{
		{gcTestBaseURL, gcTestBaseURL + "/" + gcTestKey, gcTestKey, true},
		{gcTestBaseURL + "/", gcTestBaseURL + "/" + gcTestKey, gcTestKey, true},
		{gcTestBaseURL, "nats://other/" + gcTestKey, "", false},
		{gcTestBaseURL, gcTestBaseURL + "/", "", false},
		{"file:///tmp/out", "file:///tmp/out/" + gcTestKey, "", false},
	}
	for _, c := range cases {
		got, ok := outputKeyOf(c.base, c.url)
		if got != c.want || ok != c.ok {
			t.Errorf("outputKeyOf(%q, %q) = %q, %v; want %q, %v", c.base, c.url, got, ok, c.want, c.ok)
		}
	}
}

func TestArtifactNameFromKey(t *testing.T) {
	cases := []struct {
		key, want string
		ok        bool
	}{
		{gcTestKey, "a", true},
		{"noslash.cwasm", "", false},
		{"/img/x.cwasm", "", false},
		{"a/", "a", false},
	}
	for _, c := range cases {
		got, ok := artifactNameFromKey(c.key)
		if ok != c.ok || (ok && got != c.want) {
			t.Errorf("artifactNameFromKey(%q) = %q, %v; want %q, %v", c.key, got, ok, c.want, c.ok)
		}
	}
}

// precompileFixture is an Artifact with a running (not Complete) Job, a
// reconciler on a fake client, and an embedded NATS with the precompile bucket.
type precompileFixture struct {
	r        *PrecompileReconciler
	artifact *runtimev1alpha1.Artifact
	job      *batchv1.Job
	store    nats.ObjectStore
	nc       *nats.Conn
	key      string
}

func newPrecompileFixture(t *testing.T, jobCreated time.Time) *precompileFixture {
	t.Helper()
	nc := startEmbeddedNats(t)
	js, err := nc.JetStream()
	if err != nil {
		t.Fatalf("jetstream: %v", err)
	}
	store, err := js.CreateObjectStore(&nats.ObjectStoreConfig{Bucket: gcTestBucket})
	if err != nil {
		t.Fatalf("create object store: %v", err)
	}

	a := &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{Name: testArtifactName, Namespace: metav1.NamespaceDefault, UID: "artifact-uid"},
		Spec:       runtimev1alpha1.ArtifactSpec{Image: testImageV1},
	}
	r := &PrecompileReconciler{
		Scheme:          gcScheme(t),
		ArtifactStore:   ArtifactStoreConfig{BaseURL: gcTestBaseURL},
		Target:          testTarget,
		WasmtimeVersion: testWasmtimeVersion,
		NatsConn:        nc,
	}
	job, err := r.buildDesiredJob(a)
	if err != nil {
		t.Fatalf("build job: %v", err)
	}
	job.CreationTimestamp = metav1.NewTime(jobCreated.Truncate(time.Second))
	r.Client = fake.NewClientBuilder().WithScheme(r.Scheme).
		WithObjects(a, job).
		WithStatusSubresource(&runtimev1alpha1.Artifact{}).
		WithIndex(&runtimev1alpha1.Artifact{}, artifactNameIndex,
			func(o client.Object) []string { return []string{o.GetName()} }).
		Build()

	key, ok := outputKeyOf(gcTestBaseURL, r.outputURLOf(a))
	if !ok {
		t.Fatalf("no output key for %q", r.outputURLOf(a))
	}
	return &precompileFixture{r: r, artifact: a, job: job, store: store, nc: nc, key: key}
}

func (f *precompileFixture) reconcile(t *testing.T) *runtimev1alpha1.Artifact {
	t.Helper()
	req := ctrl.Request{NamespacedName: types.NamespacedName{Namespace: f.artifact.Namespace, Name: f.artifact.Name}}
	if _, err := f.r.Reconcile(context.Background(), req); err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	var got runtimev1alpha1.Artifact
	if err := f.r.Get(context.Background(), req.NamespacedName, &got); err != nil {
		t.Fatalf("get artifact: %v", err)
	}
	return &got
}

func TestReconcile_RecordsVariantWhenOutputStoredBeforeJobComplete(t *testing.T) {
	f := newPrecompileFixture(t, time.Now())
	if _, err := f.store.PutBytes(f.key, []byte("cwasm")); err != nil {
		t.Fatalf("put: %v", err)
	}

	got := f.reconcile(t)

	if len(got.Status.Precompiled) != 1 || got.Status.Precompiled[0].ArtifactURL != f.r.outputURLOf(f.artifact) {
		t.Fatalf("expected the current variant recorded, got %+v", got.Status.Precompiled)
	}
	if got.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompiled).Status != condition.ConditionTrue {
		t.Fatalf("expected Precompiled=True, got %+v", got.Status.Conditions)
	}
}

func TestReconcile_WaitsWhenOutputNotStored(t *testing.T) {
	f := newPrecompileFixture(t, time.Now())

	got := f.reconcile(t)

	if len(got.Status.Precompiled) != 0 {
		t.Fatalf("expected no variant yet, got %+v", got.Status.Precompiled)
	}
	if got.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing).Status != condition.ConditionTrue {
		t.Fatalf("expected PrecompileProgressing=True, got %+v", got.Status.Conditions)
	}
}

// An object under the same key from before this Job (a re-pushed image tag)
// is not this Job's output.
func TestReconcile_IgnoresOutputOlderThanJob(t *testing.T) {
	f := newPrecompileFixture(t, time.Now().Add(time.Hour))
	if _, err := f.store.PutBytes(f.key, []byte("old cwasm")); err != nil {
		t.Fatalf("put: %v", err)
	}

	got := f.reconcile(t)

	if len(got.Status.Precompiled) != 0 {
		t.Fatalf("expected the older object to be ignored, got %+v", got.Status.Precompiled)
	}
}

// Without a NATS connection (or with a non-nats store), Job Complete is the
// only success signal, as before.
func TestReconcile_WithoutNatsConnWaitsForJobComplete(t *testing.T) {
	f := newPrecompileFixture(t, time.Now())
	if _, err := f.store.PutBytes(f.key, []byte("cwasm")); err != nil {
		t.Fatalf("put: %v", err)
	}
	f.r.NatsConn = nil

	if got := f.reconcile(t); len(got.Status.Precompiled) != 0 {
		t.Fatalf("expected no variant before Job Complete, got %+v", got.Status.Precompiled)
	}

	f.job.Status.Conditions = []batchv1.JobCondition{{Type: batchv1.JobComplete, Status: corev1.ConditionTrue}}
	if err := f.r.Status().Update(context.Background(), f.job); err != nil {
		if err := f.r.Update(context.Background(), f.job); err != nil {
			t.Fatalf("update job: %v", err)
		}
	}
	if got := f.reconcile(t); len(got.Status.Precompiled) != 1 {
		t.Fatalf("expected the variant after Job Complete, got %+v", got.Status.Precompiled)
	}
}

// If the object store cannot be reached, the reconcile waits for Job Complete
// instead of failing.
func TestReconcile_ClosedNatsConnWaitsForJobComplete(t *testing.T) {
	f := newPrecompileFixture(t, time.Now())
	if _, err := f.store.PutBytes(f.key, []byte("cwasm")); err != nil {
		t.Fatalf("put: %v", err)
	}
	f.nc.Close()

	if got := f.reconcile(t); len(got.Status.Precompiled) != 0 {
		t.Fatalf("expected no variant while NATS is gone, got %+v", got.Status.Precompiled)
	}
}

func TestOutputWatcher_SendsEventForPut(t *testing.T) {
	f := newPrecompileFixture(t, time.Now())
	other := &runtimev1alpha1.Artifact{ObjectMeta: metav1.ObjectMeta{Name: "other", Namespace: metav1.NamespaceDefault}}
	if err := f.r.Create(context.Background(), other); err != nil {
		t.Fatalf("create: %v", err)
	}

	events := make(chan event.GenericEvent, 8)
	w := &precompileOutputWatcher{Reader: f.r.Client, NatsConn: f.nc, Bucket: gcTestBucket, Events: events}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { _ = w.Start(ctx); close(done) }()
	t.Cleanup(func() { cancel(); <-done })

	// The watch only delivers puts made after it started; retry the put until
	// an event arrives, so the test does not race the watch setup.
	deadline := time.After(10 * time.Second)
	for {
		if _, err := f.store.PutBytes(f.key, []byte("cwasm")); err != nil {
			t.Fatalf("put: %v", err)
		}
		select {
		case e := <-events:
			if e.Object.GetName() != testArtifactName || e.Object.GetNamespace() != metav1.NamespaceDefault {
				t.Fatalf("event for the wrong Artifact: %s/%s", e.Object.GetNamespace(), e.Object.GetName())
			}
			if err := f.store.Delete(f.key); err != nil {
				t.Fatalf("delete: %v", err)
			}
			select {
			case e := <-events:
				if e.Object.GetName() != testArtifactName {
					t.Fatalf("unexpected event for %s", e.Object.GetName())
				}
				// A put still in flight from the retry loop; a delete sends none.
			case <-time.After(300 * time.Millisecond):
			}
			return
		case <-time.After(200 * time.Millisecond):
		case <-deadline:
			t.Fatal("no event for the put")
		}
	}
}

func TestOutputWatcher_RetriesUntilBucketExists(t *testing.T) {
	nc := startEmbeddedNats(t)
	a := &runtimev1alpha1.Artifact{ObjectMeta: metav1.ObjectMeta{Name: testArtifactName, Namespace: metav1.NamespaceDefault}}
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).WithObjects(a).
		WithIndex(&runtimev1alpha1.Artifact{}, artifactNameIndex,
			func(o client.Object) []string { return []string{o.GetName()} }).
		Build()
	events := make(chan event.GenericEvent, 8)
	w := &precompileOutputWatcher{Reader: c, NatsConn: nc, Bucket: gcTestBucket, Events: events}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { _ = w.Start(ctx); close(done) }()
	t.Cleanup(func() { cancel(); <-done })

	js, err := nc.JetStream()
	if err != nil {
		t.Fatalf("jetstream: %v", err)
	}
	store, err := js.CreateObjectStore(&nats.ObjectStoreConfig{Bucket: gcTestBucket})
	if err != nil {
		t.Fatalf("create object store: %v", err)
	}
	deadline := time.After(10 * time.Second)
	for {
		if _, err := store.PutBytes(testArtifactName+"/img/x.cwasm", []byte("cwasm")); err != nil {
			t.Fatalf("put: %v", err)
		}
		select {
		case <-events:
			return
		case <-time.After(200 * time.Millisecond):
		case <-deadline:
			t.Fatal("watcher did not pick up the bucket once it existed")
		}
	}
}
