package runtime

import (
	"context"
	"errors"
	"fmt"
	"net/url"
	"strings"
	"time"

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
	// BaseURL is the nats://<bucket> prefix precompiled bytes are written under
	// (PrecompileArtifactBaseURL). Object keys are the URL path under it.
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

	bucket, ok := bucketFromBaseURL(g.BaseURL)
	if !ok {
		log.V(1).Info("precompile artifact store is not nats://, GC disabled", "baseURL", g.BaseURL)
		return nil
	}

	log.V(1).Info("starting precompile GC",
		"interval", g.Interval,
		"gracePeriod", g.GracePeriod,
		"bucket", bucket,
	)

	wait.UntilWithContext(ctx, func(ctx context.Context) {
		if err := g.sweep(ctx, bucket); err != nil {
			log.Error(err, "precompile GC sweep failed")
		}
	}, g.Interval)
	return nil
}

// sweep performs one mark-and-sweep pass over the object store.
func (g *PrecompileGC) sweep(ctx context.Context, bucket string) error {
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

	// MaxWait raises the per-request timeout above nats.go's 5s default so
	// listing a large bucket doesn't fail with "context deadline exceeded"
	// while the list consumer is being built.
	js, err := g.NatsConn.JetStream(nats.MaxWait(natsListTimeout))
	if err != nil {
		return fmt.Errorf("acquiring JetStream context: %w", err)
	}
	store, err := js.ObjectStore(bucket)
	if err != nil {
		return fmt.Errorf("opening object store %q: %w", bucket, err)
	}

	// Every entry NATS currently has in the object store for this bucket.
	cwasmInfos, err := store.List()
	if err != nil {
		if errors.Is(err, nats.ErrNoObjectsFound) {
			return nil
		}
		return fmt.Errorf("listing cwasm objects in %q: %w", bucket, err)
	}

	now := time.Now()
	liveCwasm := make([]cwasmObject, 0, len(cwasmInfos))
	for _, info := range cwasmInfos {
		if info.Deleted {
			continue
		}
		liveCwasm = append(liveCwasm, cwasmObject{Key: info.Name, ModTime: info.ModTime})
	}

	// Sweep: diff the store's cwasm against the in-use set — what's left is
	// garbage, further filtered down to what's past the grace period.
	removable, withinGrace := removableKeys(liveCwasm, inUseCwasm, now, g.GracePeriod)

	deleted := 0
	for _, key := range removable {
		if err := store.Delete(key); err != nil {
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

// cwasmObject is the minimal object-store metadata the sweep reasons about
// for one precompiled .cwasm blob.
type cwasmObject struct {
	Key     string
	ModTime time.Time
}

// bucketFromBaseURL extracts the object-store bucket from a nats://<bucket>
// base URL. Returns ("", false) for non-nats schemes or a missing host,
// mirroring the Rust producer's parse_nats_url (host = bucket).
func bucketFromBaseURL(baseURL string) (string, bool) {
	u, err := url.Parse(baseURL)
	if err != nil || u.Scheme != "nats" || u.Host == "" {
		return "", false
	}
	return u.Host, true
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
