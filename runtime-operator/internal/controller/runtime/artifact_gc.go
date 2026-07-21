package runtime

import (
	"context"
	"errors"
	"fmt"
	"net/url"
	"strings"
	"time"

	"github.com/nats-io/nats.go"
	batchv1 "k8s.io/api/batch/v1"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// PrecompileGC garbage-collects precompiled .cwasm objects that no live
// Artifact references. The precompile pipeline writes AOT-compiled bytes to a
// NATS object store keyed by (artifact, image, target, wasmtime version) but
// never deletes them, so Artifact deletes, image re-tags and wasmtime-version
// bumps strand the old objects and grow the store without bound.
//
// GC is a deterministic mark-and-sweep, not a usage-based TTL: an object is
// reachable iff its key appears in some Artifact's Status.Precompiled, so an
// object referenced by no Artifact is provably dead. It never evicts a
// cold-but-valid object. The only hazard is the write-then-record race — the
// precompile Job puts the object before the operator patches status — which the
// GracePeriod (minimum object age) and the in-flight-Job guard cover.
//
// PrecompileGC is a manager.Runnable that runs only on the elected leader.
type PrecompileGC struct {
	// Reader lists Artifacts and Jobs to build the reachable set. It is the
	// manager's cached client (mgr.GetClient), so its namespace scope and RBAC
	// match the precompile controller that produces the objects: all namespaces
	// when the operator watches all namespaces, or the watched namespaces under
	// per-namespace Roles. The GC therefore only collects within the same scope
	// this operator writes to, and needs no RBAC beyond what precompile already
	// has.
	Reader client.Reader
	// NatsConn is the operator's existing NATS connection, reused to reach the
	// JetStream object store.
	NatsConn *nats.Conn
	// BaseURL is the nats://<bucket> prefix precompiled bytes are written under
	// (PrecompileArtifactBaseURL). Object keys are the URL path under it.
	BaseURL string
	// Interval is the sweep cadence.
	Interval time.Duration
	// GracePeriod is the minimum age (by object ModTime) before an unreachable
	// object may be collected. Guards the write-then-record race.
	GracePeriod time.Duration
}

// NeedLeaderElection makes the manager run the GC only on the elected leader,
// so multiple operator replicas never sweep concurrently.
func (g *PrecompileGC) NeedLeaderElection() bool { return true }

// Start implements manager.Runnable.
func (g *PrecompileGC) Start(ctx context.Context) error {
	log := ctrl.LoggerFrom(ctx).WithName("precompile-gc")
	log.Info("starting precompile GC",
		"interval", g.Interval,
		"gracePeriod", g.GracePeriod,
		"baseURL", g.BaseURL,
	)

	ticker := time.NewTicker(g.Interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil
		case <-ticker.C:
			if err := g.sweep(ctx); err != nil {
				log.Error(err, "precompile GC sweep failed")
			}
		}
	}
}

// sweep performs one mark-and-sweep pass over the object store.
func (g *PrecompileGC) sweep(ctx context.Context) error {
	log := ctrl.LoggerFrom(ctx).WithName("precompile-gc")

	bucket, ok := bucketFromBaseURL(g.BaseURL)
	if !ok {
		// file:// (dev) and any non-nats scheme have no object store to sweep.
		log.V(1).Info("precompile artifact store is not nats://, skipping GC", "baseURL", g.BaseURL)
		return nil
	}

	// Mark: reachable keys = union of every Artifact's recorded variants.
	var artifacts runtimev1alpha1.ArtifactList
	if err := g.Reader.List(ctx, &artifacts); err != nil {
		return fmt.Errorf("listing artifacts: %w", err)
	}
	// The cwasm keys still claimed by a live Artifact — the "keep" set.
	reachable := reachableKeys(g.BaseURL, artifacts.Items)

	// Safety belt: never sweep while a precompile Job is in flight — its object
	// may already be written but not yet recorded in any Artifact status.
	inFlight, err := g.precompileJobInFlight(ctx)
	if err != nil {
		return fmt.Errorf("checking in-flight precompile jobs: %w", err)
	}
	if inFlight {
		log.Info("precompile Job in flight, skipping GC sweep")
		return nil
	}

	js, err := g.NatsConn.JetStream()
	if err != nil {
		return fmt.Errorf("acquiring JetStream context: %w", err)
	}
	store, err := js.ObjectStore(bucket)
	if err != nil {
		if errors.Is(err, nats.ErrStreamNotFound) {
			// Bucket not created yet (no successful precompile has run).
			log.V(1).Info("object store does not exist yet, nothing to GC", "bucket", bucket)
			return nil
		}
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

	// Sweep: diff the store's cwasm against the reachable set — what's left is
	// garbage, further filtered down to what's past the grace period.
	removable, withinGrace := removableKeys(liveCwasm, reachable, now, g.GracePeriod)

	deleted := 0
	for _, key := range removable {
		if err := store.Delete(key); err != nil {
			log.Error(err, "failed to delete orphaned cwasm object", "bucket", bucket, "key", key)
			continue
		}
		deleted++
		log.Info("deleted orphaned cwasm object", "bucket", bucket, "key", key)
	}

	log.Info("precompile GC sweep complete",
		"bucket", bucket,
		"scanned", len(liveCwasm),
		"reachable", len(reachable),
		"orphaned", len(removable),
		"deleted", deleted,
		"withinGrace", withinGrace,
	)
	return nil
}

// precompileJobInFlight reports whether any precompile Job is neither complete
// nor failed. Reuses jobComplete/jobFailed from the precompile controller.
func (g *PrecompileGC) precompileJobInFlight(ctx context.Context) (bool, error) {
	var jobs batchv1.JobList
	if err := g.Reader.List(ctx, &jobs); err != nil {
		return false, err
	}
	for i := range jobs.Items {
		j := &jobs.Items[i]
		if !strings.HasPrefix(j.Name, "precompile-") {
			continue
		}
		failed, _ := jobFailed(j)
		if !jobComplete(j) && !failed {
			return true, nil
		}
	}
	return false, nil
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

// reachableKeys returns the set of object-store keys referenced by the recorded
// precompiled variants of the given artifacts, relative to baseURL. Variant URLs
// that do not live under baseURL (e.g. a different bucket) are ignored, so the
// sweep is scoped to its own bucket.
func reachableKeys(baseURL string, artifacts []runtimev1alpha1.Artifact) map[string]struct{} {
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

// removableKeys partitions stored .cwasm objects into those safe to delete —
// unreachable AND older than the grace period — and a count of objects still
// within the grace window (guarding the write-then-record race), which are
// left alone for now.
func removableKeys(
	cwasm []cwasmObject,
	reachable map[string]struct{},
	now time.Time,
	grace time.Duration,
) (removable []string, withinGrace int) {
	cutoff := now.Add(-grace)
	for _, o := range cwasm {
		if _, ok := reachable[o.Key]; ok {
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
