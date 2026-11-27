package runtime

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/nats-io/nats.go"
	batchv1 "k8s.io/api/batch/v1"
	"k8s.io/apimachinery/pkg/util/wait"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/event"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/source"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

const artifactNameIndex = "precompile.artifactName"

const reopenWatchDelay = 5 * time.Second

func (r *PrecompileReconciler) startPrecompiledArtifactWatcher(mgr ctrl.Manager) (source.Source, error) {
	bucket, ok := bucketFromBaseURL(r.ArtifactStore.BaseURL)
	if !ok || r.NatsConn == nil {
		return nil, nil
	}
	if err := indexArtifactsByName(mgr); err != nil {
		return nil, err
	}
	events := make(chan event.GenericEvent)
	if err := mgr.Add(&precompiledArtifactWatcher{
		Reader:   mgr.GetClient(),
		NatsConn: r.NatsConn,
		Bucket:   bucket,
		Events:   events,
	}); err != nil {
		return nil, err
	}
	return source.Channel(events, &handler.EnqueueRequestForObject{}), nil
}

func indexArtifactsByName(mgr ctrl.Manager) error {
	return mgr.GetFieldIndexer().IndexField(context.Background(), &runtimev1alpha1.Artifact{}, artifactNameIndex,
		func(o client.Object) []string { return []string{o.GetName()} })
}

func (r *PrecompileReconciler) precompiledArtifactKey(a *runtimev1alpha1.Artifact) string {
	return fmt.Sprintf("%s/%s/%s-%s.cwasm", a.Name, sanitizeImageRef(a.Spec.Image), r.Target, r.WasmtimeVersion)
}

func artifactNameFromKey(key string) (string, bool) {
	name, rest, ok := strings.Cut(key, "/")
	return name, ok && name != "" && rest != ""
}

func (r *PrecompileReconciler) jobSucceeded(ctx context.Context, a *runtimev1alpha1.Artifact, job *batchv1.Job) bool {
	return jobComplete(job) || r.precompiledArtifactStoredSince(ctx, a, job.CreationTimestamp.Time)
}

func (r *PrecompileReconciler) precompiledArtifactStoredSince(ctx context.Context, a *runtimev1alpha1.Artifact, since time.Time) bool {
	if r.NatsConn == nil {
		return false
	}
	bucket, ok := bucketFromBaseURL(r.ArtifactStore.BaseURL)
	if !ok {
		return false
	}
	js, err := r.NatsConn.JetStream(nats.MaxWait(natsRequestTimeout))
	if err != nil {
		return false
	}
	store, err := js.ObjectStore(bucket)
	if err != nil {
		return false
	}
	key := r.precompiledArtifactKey(a)
	info, err := store.GetInfo(key, nats.Context(ctx))
	if err != nil {
		if !errors.Is(err, nats.ErrObjectNotFound) {
			ctrl.LoggerFrom(ctx).V(1).Info("cannot read precompiled artifact", "key", key, "error", err.Error())
		}
		return false
	}
	storedAt := info.ModTime
	return storedAt.After(since) || storedAt.Equal(since)
}

type precompiledArtifactWatcher struct {
	Reader   client.Reader
	NatsConn *nats.Conn
	Bucket   string
	Events   chan<- event.GenericEvent
}

func (w *precompiledArtifactWatcher) NeedLeaderElection() bool { return true }

func (w *precompiledArtifactWatcher) Start(ctx context.Context) error {
	wait.UntilWithContext(ctx, w.runWatch, reopenWatchDelay)
	return nil
}

func (w *precompiledArtifactWatcher) runWatch(ctx context.Context) {
	if err := w.streamPrecompiledArtifacts(ctx); err != nil && ctx.Err() == nil {
		ctrl.LoggerFrom(ctx).V(1).Info("precompiled artifact watch ended; reopening",
			"bucket", w.Bucket, "in", reopenWatchDelay, "error", err.Error())
	}
}

func (w *precompiledArtifactWatcher) streamPrecompiledArtifacts(ctx context.Context) error {
	js, err := w.NatsConn.JetStream(nats.MaxWait(natsRequestTimeout))
	if err != nil {
		return err
	}
	store, err := js.ObjectStore(w.Bucket)
	if err != nil {
		return err
	}
	watcher, err := store.Watch(nats.UpdatesOnly(), nats.IgnoreDeletes())
	if err != nil {
		return err
	}
	defer func() { _ = watcher.Stop() }()

	updates := watcher.Updates()
	for {
		select {
		case <-ctx.Done():
			return nil
		case info, ok := <-updates:
			if !ok {
				return errors.New("watch closed")
			}
			w.enqueueArtifactsFor(ctx, info)
		}
	}
}

func (w *precompiledArtifactWatcher) enqueueArtifactsFor(ctx context.Context, info *nats.ObjectInfo) {
	if info == nil {
		return
	}
	name, ok := artifactNameFromKey(info.Name)
	if !ok {
		return
	}
	var artifacts runtimev1alpha1.ArtifactList
	if err := w.Reader.List(ctx, &artifacts, client.MatchingFields{artifactNameIndex: name}); err != nil {
		ctrl.LoggerFrom(ctx).V(1).Info("cannot list artifacts for precompiled artifact", "key", info.Name, "error", err.Error())
		return
	}
	for i := range artifacts.Items {
		select {
		case w.Events <- event.GenericEvent{Object: &artifacts.Items[i]}:
		case <-ctx.Done():
			return
		}
	}
}
