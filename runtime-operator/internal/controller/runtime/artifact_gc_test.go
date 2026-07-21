package runtime

import (
	"context"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
	"go.wasmcloud.dev/runtime-operator/v2/pkg/wasmbus"
)

const gcTestBaseURL = "nats://precompiled-artifacts"

func TestBucketFromBaseURL(t *testing.T) {
	cases := []struct {
		name       string
		baseURL    string
		wantBucket string
		wantOK     bool
	}{
		{"nats scheme", "nats://precompiled-artifacts", "precompiled-artifacts", true},
		{"nats with trailing slash", "nats://bucket/", "bucket", true},
		{"file scheme is skipped", "file:///var/lib/cwasm", "", false},
		{"empty is skipped", "", "", false},
		{"missing host", "nats://", "", false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			bucket, ok := bucketFromBaseURL(tc.baseURL)
			if ok != tc.wantOK || bucket != tc.wantBucket {
				t.Fatalf("bucketFromBaseURL(%q) = (%q, %v), want (%q, %v)",
					tc.baseURL, bucket, ok, tc.wantBucket, tc.wantOK)
			}
		})
	}
}

func TestReachableKeys(t *testing.T) {
	artifact := func(name string, urls ...string) runtimev1alpha1.Artifact {
		a := runtimev1alpha1.Artifact{ObjectMeta: metav1.ObjectMeta{Name: name}}
		for _, u := range urls {
			a.Status.Precompiled = append(a.Status.Precompiled,
				runtimev1alpha1.PrecompiledVariant{ArtifactURL: u})
		}
		return a
	}

	cases := []struct {
		name      string
		artifacts []runtimev1alpha1.Artifact
		want      map[string]struct{}
	}{
		{
			name: "unions keys across artifacts and variants",
			artifacts: []runtimev1alpha1.Artifact{
				artifact("a", gcTestBaseURL+"/a/img/x86_64-27.0.0.cwasm"),
				artifact("b",
					gcTestBaseURL+"/b/img/x86_64-27.0.0.cwasm",
					gcTestBaseURL+"/b/img/aarch64-27.0.0.cwasm"),
			},
			want: map[string]struct{}{
				"a/img/x86_64-27.0.0.cwasm":  {},
				"b/img/x86_64-27.0.0.cwasm":  {},
				"b/img/aarch64-27.0.0.cwasm": {},
			},
		},
		{
			name: "ignores variants stored under a different bucket",
			artifacts: []runtimev1alpha1.Artifact{
				artifact("a",
					gcTestBaseURL+"/a/img/x86_64-27.0.0.cwasm",
					"nats://other-bucket/a/img/x86_64-27.0.0.cwasm"),
			},
			want: map[string]struct{}{"a/img/x86_64-27.0.0.cwasm": {}},
		},
		{
			name:      "empty status yields empty set",
			artifacts: []runtimev1alpha1.Artifact{artifact("a")},
			want:      map[string]struct{}{},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := reachableKeys(gcTestBaseURL, tc.artifacts)
			if len(got) != len(tc.want) {
				t.Fatalf("reachableKeys = %v, want %v", got, tc.want)
			}
			for k := range tc.want {
				if _, ok := got[k]; !ok {
					t.Fatalf("reachableKeys missing key %q; got %v", k, got)
				}
			}
		})
	}
}

func TestRemovableKeys(t *testing.T) {
	now := time.Unix(1_000_000, 0)
	grace := time.Hour
	reachable := map[string]struct{}{"live.cwasm": {}}

	cwasm := []cwasmObject{
		{Key: "live.cwasm", ModTime: now.Add(-2 * time.Hour)},           // reachable -> keep
		{Key: "old-orphan.cwasm", ModTime: now.Add(-2 * time.Hour)},     // orphan, past grace -> removable
		{Key: "fresh-orphan.cwasm", ModTime: now.Add(-1 * time.Minute)}, // orphan, within grace -> skip
	}

	removable, withinGrace := removableKeys(cwasm, reachable, now, grace)

	if len(removable) != 1 || removable[0] != "old-orphan.cwasm" {
		t.Fatalf("removable = %v, want [old-orphan.cwasm]", removable)
	}
	if withinGrace != 1 {
		t.Fatalf("withinGrace = %d, want 1", withinGrace)
	}
}

func TestRemovableKeys_GraceBoundaryIsInclusiveOfEqual(t *testing.T) {
	now := time.Unix(1_000_000, 0)
	grace := time.Hour
	// An orphan whose ModTime is exactly at the cutoff is old enough to be
	// removable (cutoff = now-grace; ModTime == cutoff is not After(cutoff)).
	cwasm := []cwasmObject{{Key: "edge.cwasm", ModTime: now.Add(-grace)}}

	removable, withinGrace := removableKeys(cwasm, map[string]struct{}{}, now, grace)
	if len(removable) != 1 || withinGrace != 0 {
		t.Fatalf("removable=%v withinGrace=%d, want [edge.cwasm] withinGrace=0", removable, withinGrace)
	}
}

func gcScheme(t *testing.T) *runtime.Scheme {
	t.Helper()
	s := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(s); err != nil {
		t.Fatalf("adding clientgo scheme: %v", err)
	}
	if err := runtimev1alpha1.AddToScheme(s); err != nil {
		t.Fatalf("adding runtime scheme: %v", err)
	}
	return s
}

func job(name string, conds ...batchv1.JobCondition) *batchv1.Job {
	return &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: "default"},
		Status:     batchv1.JobStatus{Conditions: conds},
	}
}

func TestPrecompileJobInFlight(t *testing.T) {
	complete := batchv1.JobCondition{Type: batchv1.JobComplete, Status: corev1.ConditionTrue}
	failed := batchv1.JobCondition{Type: batchv1.JobFailed, Status: corev1.ConditionTrue}

	cases := []struct {
		name string
		jobs []client.Object
		want bool
	}{
		{"running precompile job", []client.Object{job("precompile-a")}, true},
		{"completed precompile job", []client.Object{job("precompile-a", complete)}, false},
		{"failed precompile job", []client.Object{job("precompile-a", failed)}, false},
		{"non-precompile job ignored", []client.Object{job("build-a")}, false},
		{"mixed: one in flight", []client.Object{job("precompile-a", complete), job("precompile-b")}, true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			c := fake.NewClientBuilder().WithScheme(gcScheme(t)).WithObjects(tc.jobs...).Build()
			g := &PrecompileGC{Reader: c}
			got, err := g.precompileJobInFlight(context.Background())
			if err != nil {
				t.Fatalf("precompileJobInFlight: %v", err)
			}
			if got != tc.want {
				t.Fatalf("precompileJobInFlight = %v, want %v", got, tc.want)
			}
		})
	}
}

// startEmbeddedNats boots an in-process JetStream-enabled NATS server for the
// sweep integration test. Reuses the operator's own server helper.
func startEmbeddedNats(t *testing.T) *nats.Conn {
	t.Helper()
	opts := wasmbus.NatsDefaultServerOptions()
	opts.Port = -1 // random free port
	opts.StoreDir = t.TempDir()
	s, err := wasmbus.NatsEmbeddedServer(opts, 5*time.Second)
	if err != nil {
		t.Skipf("embedded NATS unavailable: %v", err)
	}
	t.Cleanup(s.Shutdown)

	nc, err := nats.Connect(s.ClientURL())
	if err != nil {
		t.Fatalf("connecting to embedded NATS: %v", err)
	}
	t.Cleanup(nc.Close)
	return nc
}

func objectStoreKeys(t *testing.T, store nats.ObjectStore) map[string]struct{} {
	t.Helper()
	objectInfos, err := store.List()
	if err != nil {
		if err == nats.ErrNoObjectsFound {
			return map[string]struct{}{}
		}
		t.Fatalf("listing objects: %v", err)
	}
	keys := make(map[string]struct{})
	for _, info := range objectInfos {
		if info.Deleted {
			continue
		}
		keys[info.Name] = struct{}{}
	}
	return keys
}

func TestSweep_DeletesOrphansPastGrace(t *testing.T) {
	nc := startEmbeddedNats(t)
	js, err := nc.JetStream()
	if err != nil {
		t.Fatalf("jetstream: %v", err)
	}
	store, err := js.CreateObjectStore(&nats.ObjectStoreConfig{Bucket: "precompiled-artifacts"})
	if err != nil {
		t.Fatalf("create object store: %v", err)
	}

	const liveKey = "live/img/x86_64-27.0.0.cwasm"
	const orphanKey = "orphan/img/x86_64-27.0.0.cwasm"
	if _, err := store.PutBytes(liveKey, []byte("live")); err != nil {
		t.Fatalf("put live: %v", err)
	}
	if _, err := store.PutBytes(orphanKey, []byte("orphan")); err != nil {
		t.Fatalf("put orphan: %v", err)
	}

	// One live Artifact references liveKey; nothing references orphanKey.
	liveArtifact := &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{Name: "live", Namespace: "default"},
		Status: runtimev1alpha1.ArtifactStatus{
			Precompiled: []runtimev1alpha1.PrecompiledVariant{
				{ArtifactURL: gcTestBaseURL + "/" + liveKey},
			},
		},
	}
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).WithObjects(liveArtifact).Build()

	newGC := func(grace time.Duration) *PrecompileGC {
		return &PrecompileGC{
			Reader:      c,
			NatsConn:    nc,
			BaseURL:     gcTestBaseURL,
			GracePeriod: grace,
		}
	}
	ctx := context.Background()

	// Long grace period protects the fresh orphan.
	if err := newGC(time.Hour).sweep(ctx); err != nil {
		t.Fatalf("grace sweep: %v", err)
	}
	if keys := objectStoreKeys(t, store); len(keys) != 2 {
		t.Fatalf("within-grace orphan must be kept; store has %v", keys)
	}

	// Active deletion past grace removes only the orphan.
	if err := newGC(0).sweep(ctx); err != nil {
		t.Fatalf("delete sweep: %v", err)
	}
	keys := objectStoreKeys(t, store)
	if _, ok := keys[liveKey]; !ok {
		t.Fatalf("live object was wrongly deleted; store has %v", keys)
	}
	if _, ok := keys[orphanKey]; ok {
		t.Fatalf("orphan object should have been deleted; store has %v", keys)
	}
}

// Missing bucket is a clean no-op (no successful precompile has run yet).
func TestSweep_MissingBucketIsNoOp(t *testing.T) {
	nc := startEmbeddedNats(t)
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).Build()
	g := &PrecompileGC{Reader: c, NatsConn: nc, BaseURL: gcTestBaseURL, GracePeriod: 0}
	if err := g.sweep(context.Background()); err != nil {
		t.Fatalf("sweep against missing bucket should be a no-op, got: %v", err)
	}
}

// Non-nats base URL (e.g. file:// dev store) is skipped without touching NATS.
func TestSweep_NonNatsSchemeSkipped(t *testing.T) {
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).Build()
	g := &PrecompileGC{Reader: c, BaseURL: "file:///var/lib/cwasm", GracePeriod: 0}
	if err := g.sweep(context.Background()); err != nil {
		t.Fatalf("non-nats sweep should be a no-op, got: %v", err)
	}
}
