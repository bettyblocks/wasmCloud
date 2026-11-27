package runtime

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/opencontainers/go-digest"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

var testManifest = digest.Digest("sha256:" + strings.Repeat("e", 64))

type fakeResolver struct {
	layers map[string]digest.Digest
	err    error
	calls  int
	creds  []*RegistryCredentials
}

func (f *fakeResolver) Resolve(_ context.Context, image string, creds *RegistryCredentials) (ResolvedImage, error) {
	f.calls++
	f.creds = append(f.creds, creds)
	if f.err != nil {
		return ResolvedImage{}, f.err
	}
	l, ok := f.layers[image]
	if !ok {
		return ResolvedImage{}, errors.New("manifest unknown: " + image)
	}
	return ResolvedImage{ManifestDigest: testManifest, LayerDigest: l}, nil
}

type fakeStore struct {
	objects map[string]bool
	claims  []string
}

func (f *fakeStore) Claim(_ context.Context, url string) (bool, error) {
	f.claims = append(f.claims, url)
	return f.objects[url], nil
}

type contentFixture struct {
	t   *testing.T
	r   *PrecompileReconciler
	c   client.Client
	res *fakeResolver
	st  *fakeStore
}

func newContentFixture(t *testing.T, layers map[string]digest.Digest, objs ...client.Object) *contentFixture {
	t.Helper()
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).
		WithStatusSubresource(&runtimev1alpha1.Artifact{}, &batchv1.Job{}).
		WithObjects(objs...).Build()
	res := &fakeResolver{layers: layers}
	st := &fakeStore{objects: map[string]bool{}}
	return &contentFixture{t: t, c: c, res: res, st: st, r: &PrecompileReconciler{
		Client:          c,
		Scheme:          gcScheme(t),
		WorkerImage:     "worker:test",
		ArtifactStore:   ArtifactStoreConfig{BaseURL: gcTestBaseURL},
		Target:          testTarget,
		WasmtimeVersion: testWasmtimeVersion,
		ContentAddressed: &ContentAddressedConfig{
			Resolver: res, Store: st, EngineProfile: testProfile,
		},
	}}
}

func keyOf(layer digest.Digest) string {
	u, err := DigestKeyURL(gcTestBaseURL, layer, testTarget, testWasmtimeVersion, testProfile)
	if err != nil {
		panic(err)
	}
	return u
}

func artifactWith(name, image string) *runtimev1alpha1.Artifact {
	return &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: metav1.NamespaceDefault, UID: types.UID(name)},
		Spec:       runtimev1alpha1.ArtifactSpec{Image: image},
	}
}

func (f *contentFixture) reconcile(name string) error {
	f.t.Helper()
	_, err := f.r.Reconcile(context.Background(), ctrl.Request{
		NamespacedName: types.NamespacedName{Namespace: metav1.NamespaceDefault, Name: name},
	})
	return err
}

func (f *contentFixture) mustReconcile(name string) {
	f.t.Helper()
	if err := f.reconcile(name); err != nil {
		f.t.Fatalf("reconcile %s: %v", name, err)
	}
}

func (f *contentFixture) artifact(name string) *runtimev1alpha1.Artifact {
	f.t.Helper()
	var a runtimev1alpha1.Artifact
	if err := f.c.Get(context.Background(), types.NamespacedName{Namespace: metav1.NamespaceDefault, Name: name}, &a); err != nil {
		f.t.Fatal(err)
	}
	return &a
}

func (f *contentFixture) job(name string) *batchv1.Job {
	f.t.Helper()
	var j batchv1.Job
	err := f.c.Get(context.Background(), types.NamespacedName{Namespace: metav1.NamespaceDefault, Name: "precompile-" + name}, &j)
	if apierrors.IsNotFound(err) {
		return nil
	}
	if err != nil {
		f.t.Fatal(err)
	}
	return &j
}

func (f *contentFixture) jobs() []batchv1.Job {
	f.t.Helper()
	var jobs batchv1.JobList
	if err := f.c.List(context.Background(), &jobs); err != nil {
		f.t.Fatal(err)
	}
	return jobs.Items
}

func (f *contentFixture) setJobCondition(name string, t batchv1.JobConditionType) {
	f.t.Helper()
	j := f.job(name)
	j.Status.Conditions = []batchv1.JobCondition{{Type: t, Status: corev1.ConditionTrue, Message: "boom"}}
	if err := f.c.Status().Update(context.Background(), j); err != nil {
		f.t.Fatal(err)
	}
}

func expectVariant(t *testing.T, a *runtimev1alpha1.Artifact, url string) {
	t.Helper()
	if len(a.Status.Precompiled) != 1 || a.Status.Precompiled[0].ArtifactURL != url ||
		a.Status.Precompiled[0].ImageRef != a.Spec.Image ||
		a.Status.Precompiled[0].Target != testTarget ||
		a.Status.Precompiled[0].WasmtimeVersion != testWasmtimeVersion {
		t.Fatalf("status.precompiled = %+v, want one variant with %s", a.Status.Precompiled, url)
	}
	if got := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompiled).Status; got != condition.ConditionTrue {
		t.Fatalf("Precompiled condition = %s", got)
	}
}

func TestContentAddressed_CacheHitSkipsJob(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{"reg/app/n0001:g2": layerA}, artifactWith("n0001", "reg/app/n0001:g2"))
	f.st.objects[keyOf(layerA)] = true

	f.mustReconcile("n0001")

	if len(f.jobs()) != 0 {
		t.Fatalf("want no Job on a cache hit, got %d", len(f.jobs()))
	}
	a := f.artifact("n0001")
	expectVariant(t, a, keyOf(layerA))
	if r := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing).Reason; r != reasonCacheHit {
		t.Fatalf("reason = %s", r)
	}
	if len(f.st.claims) != 1 || f.st.claims[0] != keyOf(layerA) {
		t.Fatalf("claims = %v", f.st.claims)
	}

	// A resync asks neither the registry nor the store again.
	f.mustReconcile("n0001")
	if f.res.calls != 1 || len(f.st.claims) != 1 {
		t.Fatalf("resync: resolver calls = %d, claims = %d", f.res.calls, len(f.st.claims))
	}
}

func TestContentAddressed_CacheMissRunsPinnedJob(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{"reg/app/n0002:g1": layerB}, artifactWith("n0002", "reg/app/n0002:g1"))

	f.mustReconcile("n0002")

	j := f.job("n0002")
	if j == nil {
		t.Fatal("want a Job on a miss")
	}
	args := j.Spec.Template.Spec.Containers[0].Args
	if want := []string{"--image", "reg/app/n0002@" + testManifest.String(), "--output", keyOf(layerB)}; strings.Join(args, " ") != strings.Join(want, " ") {
		t.Fatalf("args = %v, want %v", args, want)
	}
	if j.Labels[contentKeyLabel] != contentKeyLabelValue(keyOf(layerB)) {
		t.Fatalf("labels = %v", j.Labels)
	}
	if r := f.artifact("n0002").Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing).Reason; r != reasonJobInFlight {
		t.Fatalf("reason = %s", r)
	}

	// The Job wrote the object and completed.
	f.st.objects[keyOf(layerB)] = true
	f.setJobCondition("n0002", batchv1.JobComplete)
	f.mustReconcile("n0002")
	a := f.artifact("n0002")
	expectVariant(t, a, keyOf(layerB))
	if r := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing).Reason; r != reasonJobSucceeded {
		t.Fatalf("reason = %s", r)
	}
}

// Two Artifacts with the same bytes (here: the same component pushed to two
// repos): one Job, one object, and both record the same key.
func TestContentAddressed_TwoArtifactsSameBytesShareOneJobAndObject(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{
		"reg/app1/data-api:3.2.0": layerA,
		"reg/app2/data-api:3.2.0": layerA,
	}, artifactWith("app1-data-api", "reg/app1/data-api:3.2.0"), artifactWith("app2-data-api", "reg/app2/data-api:3.2.0"))

	f.mustReconcile("app1-data-api")
	f.mustReconcile("app2-data-api")

	if n := len(f.jobs()); n != 1 {
		t.Fatalf("want 1 Job for the shared bytes, got %d", n)
	}
	waiting := f.artifact("app2-data-api").Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing)
	if waiting.Reason != reasonWaitingForSharedJob || !strings.Contains(waiting.Message, "precompile-app1-data-api") {
		t.Fatalf("second Artifact: %+v", waiting)
	}

	// The shared Job finishes; its event wakes the waiting Artifact.
	f.st.objects[keyOf(layerA)] = true
	f.setJobCondition("app1-data-api", batchv1.JobComplete)
	reqs := f.r.artifactsWaitingForJob(context.Background(), f.job("app1-data-api"))
	if len(reqs) != 1 || reqs[0].Name != "app2-data-api" {
		t.Fatalf("woken = %v", reqs)
	}
	f.mustReconcile("app1-data-api")
	f.mustReconcile("app2-data-api")

	if n := len(f.jobs()); n != 1 {
		t.Fatalf("want still 1 Job, got %d", n)
	}
	for _, n := range []string{"app1-data-api", "app2-data-api"} {
		expectVariant(t, f.artifact(n), keyOf(layerA))
	}
	var list runtimev1alpha1.ArtifactList
	if err := f.c.List(context.Background(), &list); err != nil {
		t.Fatal(err)
	}
	if inUse := inUseKeysFromArtifacts(gcTestBaseURL, list.Items); len(inUse) != 1 {
		t.Fatalf("in-use keys = %v, want the one shared key", inUse)
	}
}

// When the shared Job fails, the waiting Artifact runs its own Job.
func TestContentAddressed_SharedJobFailureFallsBackToOwnJob(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{"reg/a:1": layerA, "reg/b:1": layerA},
		artifactWith("a", "reg/a:1"), artifactWith("b", "reg/b:1"))
	f.mustReconcile("a")
	f.mustReconcile("b")
	f.setJobCondition("a", batchv1.JobFailed)

	f.mustReconcile("b")
	if f.job("b") == nil {
		t.Fatal("b must run its own Job when the shared one failed")
	}
}

// Re-tagging an Artifact to new bytes that are already compiled removes its
// stale Job and records the cached object.
func TestContentAddressed_ImageChangeToCachedBytes(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{"reg/n:g1": layerB, "reg/n:g2": layerA}, artifactWith("n", "reg/n:g1"))
	f.st.objects[keyOf(layerA)] = true
	f.mustReconcile("n") // miss -> Job for g1

	a := f.artifact("n")
	a.Spec.Image = "reg/n:g2"
	if err := f.c.Update(context.Background(), a); err != nil {
		t.Fatal(err)
	}
	f.mustReconcile("n") // stale Job deleted
	if f.job("n") != nil {
		t.Fatal("stale Job not removed")
	}
	f.mustReconcile("n") // hit
	expectVariant(t, f.artifact("n"), keyOf(layerA))
	if len(f.jobs()) != 0 {
		t.Fatalf("want no Job, got %d", len(f.jobs()))
	}
}

// The bug this mode fixes: the same tag re-pushed with other bytes must not
// keep the old object. A new Artifact with the moved tag gets the new key.
func TestContentAddressed_MovedTagGetsNewKey(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{"reg/n:latest": layerA}, artifactWith("n", "reg/n:latest"))
	f.st.objects[keyOf(layerA)] = true
	f.mustReconcile("n")
	expectVariant(t, f.artifact("n"), keyOf(layerA))

	// The tag moves, and the Artifact is re-created (the compiler re-applies).
	f.res.layers["reg/n:latest"] = layerB
	if err := f.c.Delete(context.Background(), f.artifact("n")); err != nil {
		t.Fatal(err)
	}
	if err := f.c.Create(context.Background(), artifactWith("n", "reg/n:latest")); err != nil {
		t.Fatal(err)
	}
	f.mustReconcile("n")
	j := f.job("n")
	if j == nil || j.Spec.Template.Spec.Containers[0].Args[3] != keyOf(layerB) {
		t.Fatalf("want a Job for the new bytes, got %+v", j)
	}
}

// A variant recorded by the legacy mode (tag-keyed URL) is replaced, and is
// kept until the content-addressed one is ready.
func TestContentAddressed_MigratesLegacyVariant(t *testing.T) {
	a := artifactWith("n", testImageV1)
	f := newContentFixture(t, map[string]digest.Digest{testImageV1: layerA}, a)
	legacy := f.r.outputURLOf(a)
	a = f.artifact("n")
	a.Status.Precompiled = []runtimev1alpha1.PrecompiledVariant{f.r.variantFor(a, legacy)}
	if err := f.c.Status().Update(context.Background(), a); err != nil {
		t.Fatal(err)
	}

	f.mustReconcile("n")
	if got := f.artifact("n").Status.Precompiled; len(got) != 1 || got[0].ArtifactURL != legacy {
		t.Fatalf("legacy variant must stay while the Job runs: %+v", got)
	}
	f.st.objects[keyOf(layerA)] = true
	f.setJobCondition("n", batchv1.JobComplete)
	f.mustReconcile("n")
	expectVariant(t, f.artifact("n"), keyOf(layerA))
}

func TestContentAddressed_CompletedJobWithMissingObjectRunsAgain(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{"reg/n:1": layerA}, artifactWith("n", "reg/n:1"))
	f.mustReconcile("n")
	f.setJobCondition("n", batchv1.JobComplete)

	f.mustReconcile("n") // complete, but no object
	if f.job("n") != nil {
		t.Fatal("the Job must be deleted so it runs again")
	}
	if len(f.artifact("n").Status.Precompiled) != 0 {
		t.Fatal("nothing may be recorded without an object")
	}
	f.mustReconcile("n")
	if f.job("n") == nil {
		t.Fatal("want a new Job")
	}
}

func TestContentAddressed_JobFailureIsReported(t *testing.T) {
	f := newContentFixture(t, map[string]digest.Digest{"reg/n:1": layerA}, artifactWith("n", "reg/n:1"))
	f.mustReconcile("n")
	f.setJobCondition("n", batchv1.JobFailed)
	f.mustReconcile("n")
	if c := f.artifact("n").Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed); c.Status != condition.ConditionTrue || c.Message != "boom" {
		t.Fatalf("PrecompileFailed = %+v", c)
	}
}

func TestContentAddressed_ResolveFailureIsReportedAndRetried(t *testing.T) {
	f := newContentFixture(t, nil, artifactWith("n", "reg/n:1"))
	f.res.err = errors.New("registry down")

	if err := f.reconcile("n"); err == nil {
		t.Fatal("want an error so the reconcile is retried")
	}
	c := f.artifact("n").Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed)
	if c.Status != condition.ConditionTrue || c.Reason != reasonResolveFailed || !strings.Contains(c.Message, "registry down") {
		t.Fatalf("PrecompileFailed = %+v", c)
	}
	if len(f.jobs()) != 0 {
		t.Fatal("no Job without a digest")
	}

	// The registry is back and the object exists: the failure is cleared.
	f.res.err = nil
	f.res.layers = map[string]digest.Digest{"reg/n:1": layerA}
	f.st.objects[keyOf(layerA)] = true
	f.mustReconcile("n")
	a := f.artifact("n")
	expectVariant(t, a, keyOf(layerA))
	if c := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed); c.Status == condition.ConditionTrue {
		t.Fatalf("PrecompileFailed not cleared: %+v", c)
	}
}

func TestContentAddressed_UsesImagePullSecret(t *testing.T) {
	sec := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: "pull", Namespace: metav1.NamespaceDefault},
		Type:       corev1.SecretTypeDockerConfigJson,
		Data: map[string][]byte{corev1.DockerConfigJsonKey: []byte(
			`{"auths":{"reg.example:5000":{"auth":"dXNlcjpwYXNz"}}}`)},
	}
	a := artifactWith("n", "reg.example:5000/n:1")
	a.Spec.ImagePullSecret = &corev1.LocalObjectReference{Name: "pull"}
	f := newContentFixture(t, map[string]digest.Digest{"reg.example:5000/n:1": layerA}, a, sec)

	f.mustReconcile("n")
	if len(f.res.creds) != 1 || f.res.creds[0] == nil ||
		f.res.creds[0].Username != testTokenUser || f.res.creds[0].Password != testTokenPass {
		t.Fatalf("creds = %+v", f.res.creds)
	}
	j := f.job("n")
	if j == nil || len(j.Spec.Template.Spec.Volumes) != 1 {
		t.Fatal("the Job must still mount the pull secret")
	}
}

func TestContentAddressed_OffKeepsLegacyJob(t *testing.T) {
	f := newContentFixture(t, nil, artifactWith("n0001", testImageV1))
	f.r.ContentAddressed = nil
	f.mustReconcile("n0001")
	j := f.job("n0001")
	if j == nil {
		t.Fatal("want a Job")
	}
	args := j.Spec.Template.Spec.Containers[0].Args
	if args[1] != testImageV1 || args[3] != f.r.outputURLOf(artifactWith("n0001", testImageV1)) {
		t.Fatalf("legacy args = %v", args)
	}
	if _, ok := j.Labels[contentKeyLabel]; ok {
		t.Fatal("legacy Job must not carry the content key label")
	}
	if f.res.calls != 0 {
		t.Fatal("legacy mode must not call the registry")
	}
}
