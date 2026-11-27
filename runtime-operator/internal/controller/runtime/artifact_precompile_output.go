package runtime

import (
	"context"
	"errors"
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

const (
	outputEventBuffer = 64
	reopenWatchDelay  = 5 * time.Second
)

func (r *PrecompileReconciler) startOutputWatcher(mgr ctrl.Manager) (source.Source, error) {
	bucket, ok := bucketFromBaseURL(r.ArtifactStore.BaseURL)
	if !ok || r.NatsConn == nil {
		return nil, nil
	}
	if err := indexArtifactsByName(mgr); err != nil {
		return nil, err
	}
	outputEvents := make(chan event.GenericEvent, outputEventBuffer)
	if err := mgr.Add(&precompileOutputWatcher{
		Reader:   mgr.GetClient(),
		NatsConn: r.NatsConn,
		Bucket:   bucket,
		Events:   outputEvents,
	}); err != nil {
		return nil, err
	}
	return source.Channel(outputEvents, &handler.EnqueueRequestForObject{}), nil
}

func indexArtifactsByName(mgr ctrl.Manager) error {
	return mgr.GetFieldIndexer().IndexField(context.Background(), &runtimev1alpha1.Artifact{}, artifactNameIndex,
		func(o client.Object) []string { return []string{o.GetName()} })
}

func outputKeyOf(baseURL, outputURL string) (string, bool) {
	if _, ok := bucketFromBaseURL(baseURL); !ok {
		return "", false
	}
	key, ok := strings.CutPrefix(outputURL, strings.TrimSuffix(baseURL, "/")+"/")
	if !ok || key == "" {
		return "", false
	}
	return key, true
}

func artifactNameFromKey(key string) (string, bool) {
	name, rest, ok := strings.Cut(key, "/")
	return name, ok && name != "" && rest != ""
}

func (r *PrecompileReconciler) jobSucceeded(ctx context.Context, a *runtimev1alpha1.Artifact, job *batchv1.Job) bool {
	return jobComplete(job) || r.outputStoredSince(ctx, a, job.CreationTimestamp.Time)
}

func (r *PrecompileReconciler) outputStoredSince(ctx context.Context, a *runtimev1alpha1.Artifact, since time.Time) bool {
	if r.NatsConn == nil {
		return false
	}
	bucket, ok := bucketFromBaseURL(r.ArtifactStore.BaseURL)
	if !ok {
		return false
	}
	key, ok := outputKeyOf(r.ArtifactStore.BaseURL, r.outputURLOf(a))
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
	info, err := store.GetInfo(key, nats.Context(ctx))
	if err != nil {
		if !errors.Is(err, nats.ErrObjectNotFound) {
			ctrl.LoggerFrom(ctx).V(1).Info("cannot read precompile output", "key", key, "error", err.Error())
		}
		return false
	}
	return !info.ModTime.Before(since)
}

type precompileOutputWatcher struct {
	Reader   client.Reader
	NatsConn *nats.Conn
	Bucket   string
	Events   chan<- event.GenericEvent
}

func (w *precompileOutputWatcher) NeedLeaderElection() bool { return true }

func (w *precompileOutputWatcher) Start(ctx context.Context) error {
	wait.UntilWithContext(ctx, w.runWatch, reopenWatchDelay)
	return nil
}

func (w *precompileOutputWatcher) runWatch(ctx context.Context) {
	if err := w.streamOutputs(ctx); err != nil && ctx.Err() == nil {
		ctrl.LoggerFrom(ctx).V(1).Info("precompile output watch ended; reopening",
			"bucket", w.Bucket, "in", reopenWatchDelay, "error", err.Error())
	}
}

func (w *precompileOutputWatcher) streamOutputs(ctx context.Context) error {
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
			if info == nil {
				continue
			}
			if name, ok := artifactNameFromKey(info.Name); ok {
				w.enqueueArtifactsNamed(ctx, name)
			}
		}
	}
}

func (w *precompileOutputWatcher) enqueueArtifactsNamed(ctx context.Context, name string) {
	var artifacts runtimev1alpha1.ArtifactList
	if err := w.Reader.List(ctx, &artifacts, client.MatchingFields{artifactNameIndex: name}); err != nil {
		ctrl.LoggerFrom(ctx).V(1).Info("cannot list artifacts for precompile output", "name", name, "error", err.Error())
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
