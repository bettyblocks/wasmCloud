package runtime

import (
	"context"
	"errors"
	"fmt"
	"net/url"
	"os"
	"strings"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/azidentity"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	awsconfig "github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/nats-io/nats.go"
	"k8s.io/apimachinery/pkg/util/wait"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// natsListTimeout bounds each JetStream API request the sweep makes (notably
// building the object-store list consumer). nats.go defaults to 5s, which a
// bucket with a large orphan backlog can exceed while the server computes
// DeliverLastPerSubject across every object's metadata subject — exactly the
// backlog this GC exists to clear. A sweep runs at most once per interval on
// the leader, so a generous bound here is cheap.
const natsListTimeout = 3 * time.Minute

// PrecompileGC garbage-collects precompiled .cwasm objects that are not
// currently in use. The precompile pipeline writes AOT-compiled bytes to a
// NATS object store keyed by (artifact, image, target, wasmtime version) but
// never deletes them, so Artifact deletes, image re-tags and wasmtime-version
// bumps strand the old objects and grow the store without bound.
//
// GC is a deterministic mark-and-sweep, not a usage-based TTL: an object is
// in use iff its key appears in some Artifact's Status.Precompiled, or is
// actively resolved onto a running WorkloadReplicaSet's components — so an
// object matching neither is provably dead. It never evicts a cold-but-valid
// object. The only hazard is the write-then-record race — the precompile Job
// puts the object before the operator patches Artifact status — which
// GracePeriod (minimum object age) covers with a wide safety margin: the race
// window is a single reconcile, GracePeriod defaults to much longer.
//
// PrecompileGC runs automatically whenever the precompile controller is
// enabled, as a manager.Runnable that only executes on the elected leader.
//
// The artifact store backend is selected by BaseURL's scheme: nats://
// (JetStream object store, via NatsConn), s3:// (AWS SDK v2, credentials
// resolved from the environment/config file/instance role via the SDK's
// default credential chain) and azblob:// (Azure SDK, account name +
// shared key from AZURE_STORAGE_ACCOUNT_NAME / AZURE_STORAGE_ACCOUNT_KEY,
// falling back to azidentity's DefaultAzureCredential). This mirrors how the
// Rust producer/consumer's object_store::from_env() resolves credentials for
// the same schemes, though each SDK has its own exact env var names. Any
// other scheme (e.g. file://, used for local dev) disables GC entirely —
// Start returns immediately without launching the sweep loop.
type PrecompileGC struct {
	// Reader lists Artifacts and WorkloadReplicaSets to build the in-use set.
	// It is the manager's cached client (mgr.GetClient), so its namespace
	// scope and RBAC match the precompile controller that produces the
	// objects: all namespaces when the operator watches all namespaces, or
	// the watched namespaces under per-namespace Roles. The GC therefore only
	// collects within the same scope this operator writes to, and needs no
	// RBAC beyond what precompile and the WorkloadReplicaSet controller
	// already have.
	Reader client.Reader
	// NatsConn is the operator's existing NATS connection, reused to reach the
	// JetStream object store.
	NatsConn *nats.Conn
	// BaseURL is the scheme://<bucket> prefix precompiled bytes are written
	// under (PrecompileArtifactBaseURL), e.g. nats://precompiled-artifacts,
	// s3://my-bucket, or azblob://my-container. Object keys are the URL path
	// under it.
	BaseURL string
	// Interval is the sweep cadence.
	Interval time.Duration
	// GracePeriod is the minimum age (by object ModTime) before a not-in-use
	// object may be collected. Guards the write-then-record race.
	GracePeriod time.Duration
}

// NeedLeaderElection makes the manager run the GC only on the elected leader,
// so multiple operator replicas never sweep concurrently.
func (g *PrecompileGC) NeedLeaderElection() bool { return true }

// Start implements manager.Runnable.
func (g *PrecompileGC) Start(ctx context.Context) error {
	log := ctrl.LoggerFrom(ctx).WithName("precompile-gc")

	scheme, bucket, ok := schemeAndBucketFromBaseURL(g.BaseURL)
	if !ok {
		log.V(1).Info("precompile artifact store scheme is not GC-able, GC disabled", "baseURL", g.BaseURL)
		return nil
	}

	log.V(1).Info("starting precompile GC",
		"interval", g.Interval,
		"gracePeriod", g.GracePeriod,
		"scheme", scheme,
		"bucket", bucket,
	)

	wait.UntilWithContext(ctx, func(ctx context.Context) {
		if err := g.sweep(ctx, scheme, bucket); err != nil {
			log.Error(err, "precompile GC sweep failed")
		}
	}, g.Interval)
	return nil
}

// sweep performs one mark-and-sweep pass over the object store.
func (g *PrecompileGC) sweep(ctx context.Context, scheme, bucket string) error {
	log := ctrl.LoggerFrom(ctx).WithName("precompile-gc")

	// Mark: in-use keys = union of every Artifact's recorded variants and
	// every WorkloadReplicaSet's actively-resolved component URLs.
	var artifacts runtimev1alpha1.ArtifactList
	if err := g.Reader.List(ctx, &artifacts); err != nil {
		return fmt.Errorf("listing artifacts: %w", err)
	}
	var replicaSets runtimev1alpha1.WorkloadReplicaSetList
	if err := g.Reader.List(ctx, &replicaSets); err != nil {
		return fmt.Errorf("listing workload replica sets: %w", err)
	}
	// The cwasm keys still claimed by a live Artifact or actively running in a
	// WorkloadReplicaSet — the "keep" set. Never delete either.
	inUseCwasm := unionKeys(
		inUseKeysFromArtifacts(g.BaseURL, artifacts.Items),
		inUseKeysFromReplicaSets(g.BaseURL, replicaSets.Items),
	)

	store, err := g.openStore(ctx, scheme, bucket)
	if err != nil {
		return fmt.Errorf("opening %s object store %q: %w", scheme, bucket, err)
	}
	defer func() {
		if err := store.Close(); err != nil {
			log.Error(err, "failed to close object store", "scheme", scheme, "bucket", bucket)
		}
	}()

	// Every entry the store currently has for this bucket/container.
	liveCwasm, err := store.List(ctx)
	if err != nil {
		return fmt.Errorf("listing cwasm objects in %q: %w", bucket, err)
	}

	now := time.Now()

	// Sweep: diff the store's cwasm against the in-use set — what's left is
	// garbage, further filtered down to what's past the grace period.
	removable, withinGrace := removableKeys(liveCwasm, inUseCwasm, now, g.GracePeriod)

	deleted := 0
	for _, key := range removable {
		if err := store.Delete(ctx, key); err != nil {
			log.Error(err, "failed to delete orphaned cwasm object", "bucket", bucket, "key", key)
			continue
		}
		deleted++
		// Deletions stay at Info: they are irreversible, so the audit trail of
		// what GC removed must be visible at default verbosity.
		log.Info("deleted orphaned cwasm object", "bucket", bucket, "key", key)
	}

	log.V(1).Info("precompile GC sweep complete",
		"bucket", bucket,
		"scanned", len(liveCwasm),
		"inUse", len(inUseCwasm),
		"orphaned", len(removable),
		"deleted", deleted,
		"withinGrace", withinGrace,
	)
	return nil
}

// openStore opens the cwasmStore backing scheme://bucket. NatsConn is only
// required for the nats scheme; s3/azblob authenticate via the environment
// (AWS_* / AZURE_STORAGE_* vars), matching how the Rust producer/consumer's
// object_store::from_env() resolves credentials.
func (g *PrecompileGC) openStore(ctx context.Context, scheme, bucket string) (cwasmStore, error) {
	switch scheme {
	case "nats":
		// MaxWait raises the per-request timeout above nats.go's 5s default so
		// listing a large bucket doesn't fail with "context deadline exceeded"
		// while the list consumer is being built.
		js, err := g.NatsConn.JetStream(nats.MaxWait(natsListTimeout))
		if err != nil {
			return nil, fmt.Errorf("acquiring JetStream context: %w", err)
		}
		store, err := js.ObjectStore(bucket)
		if err != nil {
			return nil, err
		}
		return &natsCwasmStore{store: store}, nil
	case "s3":
		return newS3CwasmStore(ctx, bucket)
	case "azblob":
		return newAzureCwasmStore(bucket)
	default:
		return nil, fmt.Errorf("unsupported scheme %q", scheme)
	}
}

// cwasmStore abstracts the backend holding precompiled .cwasm objects, so the
// mark-and-sweep logic in sweep is agnostic to whether the artifacts live in
// a NATS JetStream object store, S3, or Azure Blob Storage.
type cwasmStore interface {
	// List returns every live (non-deleted) object currently in the store.
	List(ctx context.Context) ([]cwasmObject, error)
	// Delete removes the object with the given key.
	Delete(ctx context.Context, key string) error
	// Close releases any resources (connections, clients) the store holds.
	Close() error
}

// natsCwasmStore adapts a NATS JetStream object store to cwasmStore.
type natsCwasmStore struct {
	store nats.ObjectStore
}

func (s *natsCwasmStore) List(_ context.Context) ([]cwasmObject, error) {
	infos, err := s.store.List()
	if err != nil {
		if errors.Is(err, nats.ErrNoObjectsFound) {
			return nil, nil
		}
		return nil, err
	}
	out := make([]cwasmObject, 0, len(infos))
	for _, info := range infos {
		if info.Deleted {
			continue
		}
		out = append(out, cwasmObject{Key: info.Name, ModTime: info.ModTime})
	}
	return out, nil
}

func (s *natsCwasmStore) Delete(_ context.Context, key string) error {
	return s.store.Delete(key)
}

func (s *natsCwasmStore) Close() error { return nil }

// s3CwasmStore adapts an AWS S3 bucket to cwasmStore.
type s3CwasmStore struct {
	client *s3.Client
	bucket string
}

// newS3CwasmStore builds an S3-backed cwasmStore. Credentials, region and
// endpoint come from the AWS SDK's default chain (env vars, shared config
// file, container/instance role), same as any other AWS SDK v2 client.
func newS3CwasmStore(ctx context.Context, bucket string) (*s3CwasmStore, error) {
	cfg, err := awsconfig.LoadDefaultConfig(ctx)
	if err != nil {
		return nil, fmt.Errorf("loading AWS config: %w", err)
	}
	return &s3CwasmStore{client: s3.NewFromConfig(cfg), bucket: bucket}, nil
}

func (s *s3CwasmStore) List(ctx context.Context) ([]cwasmObject, error) {
	var out []cwasmObject
	paginator := s3.NewListObjectsV2Paginator(s.client, &s3.ListObjectsV2Input{Bucket: &s.bucket})
	for paginator.HasMorePages() {
		page, err := paginator.NextPage(ctx)
		if err != nil {
			return nil, err
		}
		for _, obj := range page.Contents {
			if obj.Key == nil {
				continue
			}
			var modTime time.Time
			if obj.LastModified != nil {
				modTime = *obj.LastModified
			}
			out = append(out, cwasmObject{Key: *obj.Key, ModTime: modTime})
		}
	}
	return out, nil
}

func (s *s3CwasmStore) Delete(ctx context.Context, key string) error {
	_, err := s.client.DeleteObject(ctx, &s3.DeleteObjectInput{Bucket: &s.bucket, Key: &key})
	return err
}

func (s *s3CwasmStore) Close() error { return nil }

// azureCwasmStore adapts an Azure Blob Storage container to cwasmStore.
type azureCwasmStore struct {
	client    *azblob.Client
	container string
}

// newAzureCwasmStore builds an Azure Blob Storage-backed cwasmStore.
// AZURE_STORAGE_ACCOUNT_NAME selects the storage account; if
// AZURE_STORAGE_ACCOUNT_KEY (or its alias AZURE_STORAGE_ACCESS_KEY) is set,
// auth uses that shared key, otherwise it falls back to
// azidentity.DefaultAzureCredential (managed identity, Azure CLI,
// AZURE_CLIENT_ID/AZURE_CLIENT_SECRET/AZURE_TENANT_ID env vars, ...).
func newAzureCwasmStore(container string) (*azureCwasmStore, error) {
	account := os.Getenv("AZURE_STORAGE_ACCOUNT_NAME")
	if account == "" {
		return nil, errors.New("AZURE_STORAGE_ACCOUNT_NAME not set")
	}
	serviceURL := fmt.Sprintf("https://%s.blob.core.windows.net/", account)

	key := os.Getenv("AZURE_STORAGE_ACCOUNT_KEY")
	if key == "" {
		key = os.Getenv("AZURE_STORAGE_ACCESS_KEY")
	}

	var (
		client *azblob.Client
		err    error
	)
	if key != "" {
		cred, credErr := azblob.NewSharedKeyCredential(account, key)
		if credErr != nil {
			return nil, fmt.Errorf("building shared key credential: %w", credErr)
		}
		client, err = azblob.NewClientWithSharedKeyCredential(serviceURL, cred, nil)
	} else {
		cred, credErr := azidentity.NewDefaultAzureCredential(nil)
		if credErr != nil {
			return nil, fmt.Errorf("building default Azure credential: %w", credErr)
		}
		client, err = azblob.NewClient(serviceURL, cred, nil)
	}
	if err != nil {
		return nil, fmt.Errorf("building Azure Blob client: %w", err)
	}
	return &azureCwasmStore{client: client, container: container}, nil
}

func (s *azureCwasmStore) List(ctx context.Context) ([]cwasmObject, error) {
	var out []cwasmObject
	pager := s.client.NewListBlobsFlatPager(s.container, nil)
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			return nil, err
		}
		for _, item := range page.Segment.BlobItems {
			if item.Name == nil || (item.Deleted != nil && *item.Deleted) {
				continue
			}
			var modTime time.Time
			if item.Properties != nil && item.Properties.LastModified != nil {
				modTime = *item.Properties.LastModified
			}
			out = append(out, cwasmObject{Key: *item.Name, ModTime: modTime})
		}
	}
	return out, nil
}

func (s *azureCwasmStore) Delete(ctx context.Context, key string) error {
	_, err := s.client.DeleteBlob(ctx, s.container, key, nil)
	return err
}

func (s *azureCwasmStore) Close() error { return nil }

// cwasmObject is the minimal object-store metadata the sweep reasons about
// for one precompiled .cwasm blob.
type cwasmObject struct {
	Key     string
	ModTime time.Time
}

// schemeAndBucketFromBaseURL extracts the scheme and object-store
// bucket/container from a scheme://<bucket> base URL. Returns ("", "",
// false) for a scheme this GC doesn't know how to sweep (e.g. file://, used
// for local dev) or a missing host, mirroring the Rust producer's
// parse_nats_url / parse_container_url (host = bucket/container).
func schemeAndBucketFromBaseURL(baseURL string) (scheme, bucket string, ok bool) {
	u, err := url.Parse(baseURL)
	if err != nil || u.Host == "" {
		return "", "", false
	}
	switch u.Scheme {
	case "nats", "s3", "azblob":
		return u.Scheme, u.Host, true
	default:
		return "", "", false
	}
}

// bucketFromBaseURL extracts just the bucket/container from a
// scheme://<bucket> base URL. See schemeAndBucketFromBaseURL.
func bucketFromBaseURL(baseURL string) (string, bool) {
	_, bucket, ok := schemeAndBucketFromBaseURL(baseURL)
	return bucket, ok
}

// inUseKeysFromArtifacts returns the set of object-store keys referenced by
// the recorded precompiled variants of the given artifacts, relative to
// baseURL. Variant URLs that do not live under baseURL (e.g. a different
// bucket) are ignored, so the sweep is scoped to its own bucket.
func inUseKeysFromArtifacts(baseURL string, artifacts []runtimev1alpha1.Artifact) map[string]struct{} {
	prefix := strings.TrimSuffix(baseURL, "/") + "/"
	keys := make(map[string]struct{})
	for i := range artifacts {
		for _, v := range artifacts[i].Status.Precompiled {
			if key, ok := strings.CutPrefix(v.ArtifactURL, prefix); ok && key != "" {
				keys[key] = struct{}{}
			}
		}
	}
	return keys
}

// inUseKeysFromReplicaSets returns the set of object-store keys actively
// resolved onto the components of the given WorkloadReplicaSets, relative to
// baseURL. This is what's actually running — a component whose Artifact
// reference resolved to a precompiled variant carries that variant's URL on
// PrecompiledURL. Unresolved components (empty PrecompiledURL) and URLs
// outside baseURL are ignored, same as inUseKeysFromArtifacts.
func inUseKeysFromReplicaSets(baseURL string, replicaSets []runtimev1alpha1.WorkloadReplicaSet) map[string]struct{} {
	prefix := strings.TrimSuffix(baseURL, "/") + "/"
	keys := make(map[string]struct{})
	for i := range replicaSets {
		for _, c := range replicaSets[i].Spec.Template.Spec.Components {
			if key, ok := strings.CutPrefix(c.PrecompiledURL, prefix); ok && key != "" {
				keys[key] = struct{}{}
			}
		}
	}
	return keys
}

// unionKeys merges any number of key sets into one.
func unionKeys(sets ...map[string]struct{}) map[string]struct{} {
	out := make(map[string]struct{})
	for _, s := range sets {
		for key := range s {
			out[key] = struct{}{}
		}
	}
	return out
}

// removableKeys partitions stored .cwasm objects into those safe to delete —
// not in use AND older than the grace period — and a count of objects still
// within the grace window (guarding the write-then-record race), which are
// left alone for now.
func removableKeys(
	cwasm []cwasmObject,
	inUse map[string]struct{},
	now time.Time,
	grace time.Duration,
) (removable []string, withinGrace int) {
	cutoff := now.Add(-grace)
	for _, o := range cwasm {
		if _, ok := inUse[o.Key]; ok {
			continue
		}
		if o.ModTime.After(cutoff) {
			withinGrace++
			continue
		}
		removable = append(removable, o.Key)
	}
	return removable, withinGrace
}
