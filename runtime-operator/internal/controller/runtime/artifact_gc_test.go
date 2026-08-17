package runtime

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	awsconfig "github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/nats-io/nats.go"
	testcontainers "github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/modules/azure/azurite"
	"github.com/testcontainers/testcontainers-go/modules/minio"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
	"go.wasmcloud.dev/runtime-operator/v2/pkg/wasmbus"
)

const (
	gcTestBaseURL = "nats://precompiled-artifacts"
	gcTestBucket  = "precompiled-artifacts"
)

func TestBucketFromBaseURL(t *testing.T) {
	cases := []struct {
		name       string
		baseURL    string
		wantBucket string
		wantOK     bool
	}{
		{"nats scheme", "nats://precompiled-artifacts", "precompiled-artifacts", true},
		{"nats with trailing slash", "nats://bucket/", "bucket", true},
		{"s3 scheme", "s3://my-bucket", "my-bucket", true},
		{"azblob scheme", "azblob://my-container", "my-container", true},
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

func TestSchemeAndBucketFromBaseURL(t *testing.T) {
	cases := []struct {
		name       string
		baseURL    string
		wantScheme string
		wantBucket string
		wantOK     bool
	}{
		{"nats scheme", "nats://precompiled-artifacts", "nats", "precompiled-artifacts", true},
		{"s3 scheme", "s3://my-bucket", "s3", "my-bucket", true},
		{"azblob scheme", "azblob://my-container", "azblob", "my-container", true},
		{"file scheme is skipped", "file:///var/lib/cwasm", "", "", false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			scheme, bucket, ok := schemeAndBucketFromBaseURL(tc.baseURL)
			if ok != tc.wantOK || scheme != tc.wantScheme || bucket != tc.wantBucket {
				t.Fatalf("schemeAndBucketFromBaseURL(%q) = (%q, %q, %v), want (%q, %q, %v)",
					tc.baseURL, scheme, bucket, ok, tc.wantScheme, tc.wantBucket, tc.wantOK)
			}
		})
	}
}

func TestInUseKeysFromArtifacts(t *testing.T) {
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
			got := inUseKeysFromArtifacts(gcTestBaseURL, tc.artifacts)
			if len(got) != len(tc.want) {
				t.Fatalf("inUseKeysFromArtifacts = %v, want %v", got, tc.want)
			}
			for k := range tc.want {
				if _, ok := got[k]; !ok {
					t.Fatalf("inUseKeysFromArtifacts missing key %q; got %v", k, got)
				}
			}
		})
	}
}

func TestRemovableKeys(t *testing.T) {
	now := time.Unix(1_000_000, 0)
	grace := time.Hour
	inUse := map[string]struct{}{"live.cwasm": {}}

	cwasm := []cwasmObject{
		{Key: "live.cwasm", ModTime: now.Add(-2 * time.Hour)},           // in use -> keep
		{Key: "old-orphan.cwasm", ModTime: now.Add(-2 * time.Hour)},     // orphan, past grace -> removable
		{Key: "fresh-orphan.cwasm", ModTime: now.Add(-1 * time.Minute)}, // orphan, within grace -> skip
	}

	removable, withinGrace := removableKeys(cwasm, inUse, now, grace)

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
	store, err := js.CreateObjectStore(&nats.ObjectStoreConfig{Bucket: gcTestBucket})
	if err != nil {
		t.Fatalf("create object store: %v", err)
	}

	const liveKey = "live/img/x86_64-27.0.0.cwasm"
	const runningKey = "running/img/x86_64-27.0.0.cwasm"
	const orphanKey = "orphan/img/x86_64-27.0.0.cwasm"
	for key, bytes := range map[string][]byte{
		liveKey:    []byte("live"),
		runningKey: []byte("running"),
		orphanKey:  []byte("orphan"),
	} {
		if _, err := store.PutBytes(key, bytes); err != nil {
			t.Fatalf("put %s: %v", key, err)
		}
	}

	// liveKey is claimed by an Artifact's status; runningKey is resolved onto a
	// WorkloadReplicaSet's component but absent from any Artifact status (e.g.
	// the Artifact was later deleted while the replica set is still running).
	// Neither may ever be collected. Nothing references orphanKey.
	liveArtifact := &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{Name: "live", Namespace: "default"},
		Status: runtimev1alpha1.ArtifactStatus{
			Precompiled: []runtimev1alpha1.PrecompiledVariant{
				{ArtifactURL: gcTestBaseURL + "/" + liveKey},
			},
		},
	}
	runningReplicaSet := &runtimev1alpha1.WorkloadReplicaSet{
		ObjectMeta: metav1.ObjectMeta{Name: "running", Namespace: "default"},
	}
	runningReplicaSet.Spec.Template.Spec.Components = []runtimev1alpha1.WorkloadComponent{
		{PrecompiledURL: gcTestBaseURL + "/" + runningKey},
	}
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).
		WithObjects(liveArtifact, runningReplicaSet).Build()

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
	if err := newGC(time.Hour).sweep(ctx, "nats", gcTestBucket); err != nil {
		t.Fatalf("grace sweep: %v", err)
	}
	if keys := objectStoreKeys(t, store); len(keys) != 3 {
		t.Fatalf("within-grace orphan must be kept; store has %v", keys)
	}

	// Active deletion past grace removes only the orphan.
	if err := newGC(0).sweep(ctx, "nats", gcTestBucket); err != nil {
		t.Fatalf("delete sweep: %v", err)
	}
	keys := objectStoreKeys(t, store)
	if _, ok := keys[liveKey]; !ok {
		t.Fatalf("Artifact-referenced object was wrongly deleted; store has %v", keys)
	}
	if _, ok := keys[runningKey]; !ok {
		t.Fatalf("replica-set-referenced object was wrongly deleted; store has %v", keys)
	}
	if _, ok := keys[orphanKey]; ok {
		t.Fatalf("orphan object should have been deleted; store has %v", keys)
	}
}

// startMinIO boots a MinIO container and returns an S3 client pointed at it
// plus the name of a freshly created bucket. Sets the env vars newS3CwasmStore
// reads (AWS_ENDPOINT_URL_S3, AWS_S3_FORCE_PATH_STYLE, credentials) so that
// g.sweep's own internally-built client resolves to the same container.
func startMinIO(t *testing.T) (*s3.Client, string) {
	t.Helper()
	ctx := context.Background()

	ctr, err := minio.Run(ctx, "minio/minio:RELEASE.2024-01-16T16-07-38Z")
	if err != nil {
		t.Skipf("minio container unavailable: %v", err)
	}
	t.Cleanup(func() {
		if err := ctr.Terminate(context.Background()); err != nil {
			t.Logf("terminating minio container: %v", err)
		}
	})

	endpoint, err := ctr.ConnectionString(ctx)
	if err != nil {
		t.Fatalf("minio connection string: %v", err)
	}

	t.Setenv("AWS_ACCESS_KEY_ID", ctr.Username)
	t.Setenv("AWS_SECRET_ACCESS_KEY", ctr.Password)
	t.Setenv("AWS_REGION", "us-east-1")
	t.Setenv("AWS_ENDPOINT_URL_S3", "http://"+endpoint)
	t.Setenv("AWS_S3_FORCE_PATH_STYLE", "true")

	cfg, err := awsconfig.LoadDefaultConfig(ctx)
	if err != nil {
		t.Fatalf("loading AWS config: %v", err)
	}
	client := s3.NewFromConfig(cfg, func(o *s3.Options) { o.UsePathStyle = true })

	bucket := "precompiled-artifacts"
	if _, err := client.CreateBucket(ctx, &s3.CreateBucketInput{Bucket: &bucket}); err != nil {
		t.Fatalf("creating bucket: %v", err)
	}
	return client, bucket
}

// startAzurite boots an Azurite container and returns an Azure Blob client
// pointed at it plus the name of a freshly created container. Sets
// AZURE_STORAGE_CONNECTION_STRING so g.sweep's own internally-built client
// resolves to the same container.
func startAzurite(t *testing.T) (*azblob.Client, string) {
	t.Helper()
	ctx := context.Background()

	// --skipApiVersionCheck: the SDK sends whatever x-ms-version it was built
	// with, which can be newer than what this Azurite image recognizes;
	// Azurite's own error message for that mismatch names this flag as the fix.
	ctr, err := azurite.Run(ctx, "mcr.microsoft.com/azure-storage/azurite:latest",
		testcontainers.WithCmdArgs("--skipApiVersionCheck"))
	if err != nil {
		t.Skipf("azurite container unavailable: %v", err)
	}
	t.Cleanup(func() {
		if err := ctr.Terminate(context.Background()); err != nil {
			t.Logf("terminating azurite container: %v", err)
		}
	})

	blobURL, err := ctr.BlobServiceURL(ctx)
	if err != nil {
		t.Fatalf("azurite blob service URL: %v", err)
	}
	connStr := fmt.Sprintf(
		"DefaultEndpointsProtocol=http;AccountName=%s;AccountKey=%s;BlobEndpoint=%s/%s;",
		azurite.AccountName, azurite.AccountKey, blobURL, azurite.AccountName,
	)
	t.Setenv("AZURE_STORAGE_CONNECTION_STRING", connStr)

	client, err := azblob.NewClientFromConnectionString(connStr, nil)
	if err != nil {
		t.Fatalf("azure blob client: %v", err)
	}

	container := "precompiled-artifacts"
	if _, err := client.CreateContainer(ctx, container, nil); err != nil {
		t.Fatalf("creating container: %v", err)
	}
	return client, container
}

// s3ObjectKeys lists every key currently in bucket.
func s3ObjectKeys(t *testing.T, client *s3.Client, bucket string) map[string]struct{} {
	t.Helper()
	ctx := context.Background()
	keys := make(map[string]struct{})
	paginator := s3.NewListObjectsV2Paginator(client, &s3.ListObjectsV2Input{Bucket: &bucket})
	for paginator.HasMorePages() {
		page, err := paginator.NextPage(ctx)
		if err != nil {
			t.Fatalf("listing objects: %v", err)
		}
		for _, obj := range page.Contents {
			if obj.Key != nil {
				keys[*obj.Key] = struct{}{}
			}
		}
	}
	return keys
}

// azureBlobKeys lists every non-deleted blob name currently in container.
func azureBlobKeys(t *testing.T, client *azblob.Client, container string) map[string]struct{} {
	t.Helper()
	ctx := context.Background()
	keys := make(map[string]struct{})
	pager := client.NewListBlobsFlatPager(container, nil)
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatalf("listing blobs: %v", err)
		}
		for _, item := range page.Segment.BlobItems {
			if item.Name != nil && (item.Deleted == nil || !*item.Deleted) {
				keys[*item.Name] = struct{}{}
			}
		}
	}
	return keys
}

func TestSweep_DeletesOrphansPastGrace_S3(t *testing.T) {
	client, bucket := startMinIO(t)
	ctx := context.Background()

	const liveKey = "live/img/x86_64-27.0.0.cwasm"
	const runningKey = "running/img/x86_64-27.0.0.cwasm"
	const orphanKey = "orphan/img/x86_64-27.0.0.cwasm"
	for _, key := range []string{liveKey, runningKey, orphanKey} {
		key := key
		if _, err := client.PutObject(ctx, &s3.PutObjectInput{
			Bucket: &bucket,
			Key:    &key,
			Body:   strings.NewReader(key),
		}); err != nil {
			t.Fatalf("put %s: %v", key, err)
		}
	}

	baseURL := "s3://" + bucket
	liveArtifact := &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{Name: "live", Namespace: "default"},
		Status: runtimev1alpha1.ArtifactStatus{
			Precompiled: []runtimev1alpha1.PrecompiledVariant{
				{ArtifactURL: baseURL + "/" + liveKey},
			},
		},
	}
	runningReplicaSet := &runtimev1alpha1.WorkloadReplicaSet{
		ObjectMeta: metav1.ObjectMeta{Name: "running", Namespace: "default"},
	}
	runningReplicaSet.Spec.Template.Spec.Components = []runtimev1alpha1.WorkloadComponent{
		{PrecompiledURL: baseURL + "/" + runningKey},
	}
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).
		WithObjects(liveArtifact, runningReplicaSet).Build()

	newGC := func(grace time.Duration) *PrecompileGC {
		return &PrecompileGC{Reader: c, BaseURL: baseURL, GracePeriod: grace}
	}

	// Long grace period protects the fresh orphan.
	if err := newGC(time.Hour).sweep(ctx, "s3", bucket); err != nil {
		t.Fatalf("grace sweep: %v", err)
	}
	if keys := s3ObjectKeys(t, client, bucket); len(keys) != 3 {
		t.Fatalf("within-grace orphan must be kept; bucket has %v", keys)
	}

	// Active deletion past grace removes only the orphan.
	if err := newGC(0).sweep(ctx, "s3", bucket); err != nil {
		t.Fatalf("delete sweep: %v", err)
	}
	keys := s3ObjectKeys(t, client, bucket)
	if _, ok := keys[liveKey]; !ok {
		t.Fatalf("Artifact-referenced object was wrongly deleted; bucket has %v", keys)
	}
	if _, ok := keys[runningKey]; !ok {
		t.Fatalf("replica-set-referenced object was wrongly deleted; bucket has %v", keys)
	}
	if _, ok := keys[orphanKey]; ok {
		t.Fatalf("orphan object should have been deleted; bucket has %v", keys)
	}
}

func TestSweep_DeletesOrphansPastGrace_Azure(t *testing.T) {
	client, container := startAzurite(t)
	ctx := context.Background()

	const liveKey = "live/img/x86_64-27.0.0.cwasm"
	const runningKey = "running/img/x86_64-27.0.0.cwasm"
	const orphanKey = "orphan/img/x86_64-27.0.0.cwasm"
	for _, key := range []string{liveKey, runningKey, orphanKey} {
		if _, err := client.UploadBuffer(ctx, container, key, []byte(key), nil); err != nil {
			t.Fatalf("upload %s: %v", key, err)
		}
	}

	baseURL := "azblob://" + container
	liveArtifact := &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{Name: "live", Namespace: "default"},
		Status: runtimev1alpha1.ArtifactStatus{
			Precompiled: []runtimev1alpha1.PrecompiledVariant{
				{ArtifactURL: baseURL + "/" + liveKey},
			},
		},
	}
	runningReplicaSet := &runtimev1alpha1.WorkloadReplicaSet{
		ObjectMeta: metav1.ObjectMeta{Name: "running", Namespace: "default"},
	}
	runningReplicaSet.Spec.Template.Spec.Components = []runtimev1alpha1.WorkloadComponent{
		{PrecompiledURL: baseURL + "/" + runningKey},
	}
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).
		WithObjects(liveArtifact, runningReplicaSet).Build()

	newGC := func(grace time.Duration) *PrecompileGC {
		return &PrecompileGC{Reader: c, BaseURL: baseURL, GracePeriod: grace}
	}

	// Long grace period protects the fresh orphan.
	if err := newGC(time.Hour).sweep(ctx, "azblob", container); err != nil {
		t.Fatalf("grace sweep: %v", err)
	}
	if keys := azureBlobKeys(t, client, container); len(keys) != 3 {
		t.Fatalf("within-grace orphan must be kept; container has %v", keys)
	}

	// Active deletion past grace removes only the orphan.
	if err := newGC(0).sweep(ctx, "azblob", container); err != nil {
		t.Fatalf("delete sweep: %v", err)
	}
	keys := azureBlobKeys(t, client, container)
	if _, ok := keys[liveKey]; !ok {
		t.Fatalf("Artifact-referenced object was wrongly deleted; container has %v", keys)
	}
	if _, ok := keys[runningKey]; !ok {
		t.Fatalf("replica-set-referenced object was wrongly deleted; container has %v", keys)
	}
	if _, ok := keys[orphanKey]; ok {
		t.Fatalf("orphan object should have been deleted; container has %v", keys)
	}
}

// A missing bucket (no successful precompile has run yet) is a real error —
// there is no special no-op case, so the caller sees it and retries on the
// next tick like any other transient failure.
func TestSweep_MissingBucketErrors(t *testing.T) {
	nc := startEmbeddedNats(t)
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).Build()
	g := &PrecompileGC{Reader: c, NatsConn: nc, BaseURL: gcTestBaseURL, GracePeriod: 0}
	if err := g.sweep(context.Background(), "nats", gcTestBucket); err == nil {
		t.Fatal("sweep against a missing bucket should return an error")
	}
}

// An unsupported base URL scheme (e.g. file:// dev store) disables GC
// entirely: Start returns immediately without launching the sweep loop and
// never touches NATS (note NatsConn is nil here — a sweep attempt would
// panic). nats://, s3:// and azblob:// are all supported schemes today.
func TestStart_UnsupportedSchemeDisablesGC(t *testing.T) {
	c := fake.NewClientBuilder().WithScheme(gcScheme(t)).Build()
	g := &PrecompileGC{Reader: c, BaseURL: "file:///var/lib/cwasm", Interval: time.Hour}
	if err := g.Start(context.Background()); err != nil {
		t.Fatalf("Start with a non-nats store should return nil, got: %v", err)
	}
}

func TestInUseKeysFromReplicaSets(t *testing.T) {
	replicaSet := func(name string, urls ...string) runtimev1alpha1.WorkloadReplicaSet {
		rs := runtimev1alpha1.WorkloadReplicaSet{ObjectMeta: metav1.ObjectMeta{Name: name}}
		for _, u := range urls {
			rs.Spec.Template.Spec.Components = append(rs.Spec.Template.Spec.Components,
				runtimev1alpha1.WorkloadComponent{PrecompiledURL: u})
		}
		return rs
	}

	cases := []struct {
		name        string
		replicaSets []runtimev1alpha1.WorkloadReplicaSet
		want        map[string]struct{}
	}{
		{
			name: "extracts key from resolved component",
			replicaSets: []runtimev1alpha1.WorkloadReplicaSet{
				replicaSet("a", gcTestBaseURL+"/a/img/x86_64-27.0.0.cwasm"),
			},
			want: map[string]struct{}{"a/img/x86_64-27.0.0.cwasm": {}},
		},
		{
			name: "skips unresolved component (empty PrecompiledURL)",
			replicaSets: []runtimev1alpha1.WorkloadReplicaSet{
				replicaSet("a", ""),
			},
			want: map[string]struct{}{},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := inUseKeysFromReplicaSets(gcTestBaseURL, tc.replicaSets)
			if len(got) != len(tc.want) {
				t.Fatalf("inUseKeysFromReplicaSets = %v, want %v", got, tc.want)
			}
			for k := range tc.want {
				if _, ok := got[k]; !ok {
					t.Fatalf("inUseKeysFromReplicaSets missing key %q; got %v", k, got)
				}
			}
		})
	}
}
