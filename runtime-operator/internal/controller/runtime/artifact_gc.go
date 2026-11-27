package runtime

import (
	"context"
	"encoding/base64"
	"encoding/json"
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

const natsRequestTimeout = 30 * time.Second

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
// With content-addressed keys an orphan can become in use again without a
// new write: the operator finds the object and records it. Before it records
// it, the operator re-publishes the object's metadata (NatsObjectStore.Claim),
// which moves its ModTime. The delete here is conditional on the metadata
// sequence the sweep read: if anything wrote the metadata since (a claim, a
// new put of the same key), the delete does not land and the object is
// kept. Both sides are conditional on the same per-subject sequence, so
// exactly one of them wins.
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

	// beforeDelete, if set, runs between the mark phase and the deletes.
	// Tests use it to change the store in that window.
	beforeDelete func()
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

	js, err := g.NatsConn.JetStream(nats.MaxWait(natsRequestTimeout))
	if err != nil {
		return fmt.Errorf("acquiring JetStream context: %w", err)
	}
	if _, err := js.ObjectStore(bucket); err != nil {
		return fmt.Errorf("opening object store %q: %w", bucket, err)
	}
	stream := objectStreamName(bucket)

	metaSubjects, err := listMetaSubjects(js, bucket)
	if err != nil {
		return fmt.Errorf("listing object keys in %q: %w", bucket, err)
	}

	now := time.Now()
	var liveCwasm []cwasmObject
	tombstones := 0
	for subj := range metaSubjects {
		key, ok := keyFromMetaSubject(bucket, subj)
		if !ok {
			log.Info("skipping object metadata subject with undecodable key", "bucket", bucket, "subject", subj)
			continue
		}
		if _, used := inUseCwasm[key]; used {
			continue
		}
		info, seq, err := readObjectInfo(js, stream, subj)
		if err != nil {
			log.Error(err, "failed to read object metadata", "bucket", bucket, "key", key)
			continue
		}
		if !info.Deleted {
			liveCwasm = append(liveCwasm, cwasmObject{Key: key, ModTime: info.ModTime, Seq: seq})
			continue
		}
		if err := purgeTombstone(js, bucket, subj, seq, info.NUID); err != nil {
			log.Error(err, "failed to purge object tombstone", "bucket", bucket, "key", key)
			continue
		}
		tombstones++
	}

	// Sweep: diff the store's cwasm against the in-use set — what's left is
	// garbage, further filtered down to what's past the grace period.
	removable, withinGrace := removableKeys(liveCwasm, inUseCwasm, now, g.GracePeriod)

	if g.beforeDelete != nil {
		g.beforeDelete()
	}

	deleted, touched := 0, 0
	for _, obj := range removable {
		ok, err := deleteIfUnchanged(js, bucket, obj)
		if err != nil {
			log.Error(err, "failed to delete orphaned cwasm object", "bucket", bucket, "key", obj.Key)
			continue
		}
		if !ok {
			// Claimed or re-written since the sweep read it: in use again.
			touched++
			log.V(1).Info("kept cwasm object that changed during the sweep", "bucket", bucket, "key", obj.Key)
			continue
		}
		deleted++
		// Deletions stay at Info: they are irreversible, so the audit trail of
		// what GC removed must be visible at default verbosity.
		log.Info("deleted orphaned cwasm object", "bucket", bucket, "key", obj.Key)
	}

	log.V(1).Info("precompile GC sweep complete",
		"bucket", bucket,
		"keys", len(metaSubjects),
		"inUse", len(inUseCwasm),
		"orphaned", len(removable),
		"deleted", deleted,
		"changedDuringSweep", touched,
		"withinGrace", withinGrace,
		"tombstonesPurged", tombstones,
	)
	return nil
}

func objectStreamName(bucket string) string { return "OBJ_" + bucket }

func metaSubject(bucket, token string) string { return "$O." + bucket + ".M." + token }

func chunkSubject(bucket, nuid string) string { return "$O." + bucket + ".C." + nuid }

// listMetaSubjects returns one metadata subject per key, live or deleted.
func listMetaSubjects(js nats.JetStreamContext, bucket string) (map[string]uint64, error) {
	si, err := js.StreamInfo(objectStreamName(bucket),
		&nats.StreamInfoRequest{SubjectsFilter: metaSubject(bucket, ">")})
	if err != nil {
		return nil, err
	}
	return si.State.Subjects, nil
}

func keyFromMetaSubject(bucket, subj string) (string, bool) {
	enc, ok := strings.CutPrefix(subj, metaSubject(bucket, ""))
	if !ok {
		return "", false
	}
	key, err := base64.URLEncoding.DecodeString(enc)
	if err != nil || len(key) == 0 {
		return "", false
	}
	return string(key), true
}

func readObjectInfo(js nats.JetStreamContext, stream, subj string) (nats.ObjectInfo, uint64, error) {
	var info nats.ObjectInfo
	msg, err := js.GetLastMsg(stream, subj)
	if err != nil {
		return info, 0, err
	}
	if err := json.Unmarshal(msg.Data, &info); err != nil {
		return info, 0, err
	}
	info.ModTime = msg.Time
	return info, msg.Sequence, nil
}

// purgeTombstone stops at upToSeq so a re-upload of the same key since then survives.
func purgeTombstone(js nats.JetStreamContext, bucket, subj string, upToSeq uint64, nuid string) error {
	stream := objectStreamName(bucket)
	if nuid != "" {
		if err := js.PurgeStream(stream, &nats.StreamPurgeRequest{Subject: chunkSubject(bucket, nuid)}); err != nil {
			return fmt.Errorf("purging chunks: %w", err)
		}
	}
	return js.PurgeStream(stream, &nats.StreamPurgeRequest{Subject: subj, Sequence: upToSeq + 1})
}

// deleteIfUnchanged deletes obj like ObjectStore.Delete does (a tombstone on
// the metadata subject, then a purge of the chunks), but only if the
// metadata is still the message the sweep read (obj.Seq). It returns false,
// and deletes nothing, when the object was claimed or re-written since.
func deleteIfUnchanged(js nats.JetStreamContext, bucket string, obj cwasmObject) (bool, error) {
	stream := objectStreamName(bucket)
	subj := metaSubject(bucket, base64.URLEncoding.EncodeToString([]byte(obj.Key)))
	// Re-read right before the delete. The conditional publish below is what
	// makes it safe; this read gives a clear answer without a failed publish
	// in the common case, and the chunk subject of the current object.
	info, seq, err := readObjectInfo(js, stream, subj)
	switch {
	case errors.Is(err, nats.ErrMsgNotFound):
		return false, nil
	case err != nil:
		return false, fmt.Errorf("re-reading metadata: %w", err)
	case seq != obj.Seq || info.Deleted:
		return false, nil
	}
	if info.NUID == "" {
		return false, nats.ErrBadObjectMeta
	}
	info.Deleted = true
	info.Size, info.Chunks, info.Digest = 0, 0, ""
	info.ModTime = time.Time{}
	data, err := json.Marshal(info)
	if err != nil {
		return false, err
	}
	if err := publishMetaIfUnchanged(js, bucket, subj, data, seq); err != nil {
		if isWrongLastSequence(err) {
			return false, nil
		}
		return false, fmt.Errorf("publishing delete marker: %w", err)
	}
	if err := js.PurgeStream(stream, &nats.StreamPurgeRequest{Subject: chunkSubject(bucket, info.NUID)}); err != nil {
		return true, fmt.Errorf("purging chunks: %w", err)
	}
	return true, nil
}

// publishMetaIfUnchanged replaces the metadata message on subj with data, if
// the last message on subj still has sequence lastSeq.
func publishMetaIfUnchanged(
	js nats.JetStreamContext, bucket, subj string, data []byte, lastSeq uint64, opts ...nats.PubOpt,
) error {
	m := nats.NewMsg(subj)
	m.Header.Set(nats.MsgRollup, nats.MsgRollupSubject)
	m.Data = data
	opts = append(opts, nats.ExpectStream(objectStreamName(bucket)), nats.ExpectLastSequencePerSubject(lastSeq))
	_, err := js.PublishMsg(m, opts...)
	return err
}

func isWrongLastSequence(err error) bool {
	var apiErr *nats.APIError
	return errors.As(err, &apiErr) && apiErr.ErrorCode == nats.JSErrCodeStreamWrongLastSequence
}

// cwasmObject is the minimal object-store metadata the sweep reasons about
// for one precompiled .cwasm blob.
type cwasmObject struct {
	Key     string
	ModTime time.Time
	// Seq is the stream sequence of the metadata message the sweep read.
	Seq uint64
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
) (removable []cwasmObject, withinGrace int) {
	cutoff := now.Add(-grace)
	for _, o := range cwasm {
		if _, ok := inUse[o.Key]; ok {
			continue
		}
		if o.ModTime.After(cutoff) {
			withinGrace++
			continue
		}
		removable = append(removable, o)
	}
	return removable, withinGrace
}
