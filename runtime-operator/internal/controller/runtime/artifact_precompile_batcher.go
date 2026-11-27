package runtime

import (
	"sync"
	"time"

	"k8s.io/apimachinery/pkg/types"
)

// batchKey groups Artifacts that can share one precompile Job: the Job runs in
// one namespace and mounts at most one image pull secret. The worker image,
// target and wasmtime version are the same for the whole operator.
type batchKey struct {
	Namespace  string
	PullSecret string
}

// batchFlushFunc creates the Job(s) for a group of pending Artifacts.
type batchFlushFunc func(key batchKey, artifacts []types.NamespacedName)

// artifactBatcher collects pending Artifacts per batchKey and flushes a group
// when no new Artifact arrived for `window`, when it holds `maxItems`, or at
// the latest `maxDelay` after the first Artifact of the group arrived. It only
// holds names: the flush reads the Artifacts again, so a group never acts on
// stale specs.
//
// It also remembers, for a short time, which Artifact images were just put in
// a Job. The informer cache can lag the create call by some milliseconds, and
// without this a reconcile in that gap would queue the Artifact again.
type artifactBatcher struct {
	window   time.Duration
	maxDelay time.Duration
	maxItems int
	flush    batchFlushFunc
	now      func() time.Time

	mu       sync.Mutex
	groups   map[batchKey]*pendingGroup
	flushing map[types.NamespacedName]int
	assigned map[types.NamespacedName]assignment
}

type pendingGroup struct {
	first time.Time
	names map[types.NamespacedName]struct{}
	timer *time.Timer
}

type assignment struct {
	output  string
	expires time.Time
}

// assignmentTTL is how long a just-created batch item hides its Artifact from
// re-queueing. It only needs to cover the informer cache lag.
const assignmentTTL = 30 * time.Second

func newArtifactBatcher(window time.Duration, maxItems int, flush batchFlushFunc) *artifactBatcher {
	if maxItems < 1 {
		maxItems = 1
	}
	if window < 0 {
		window = 0
	}
	return &artifactBatcher{
		window:   window,
		maxDelay: 4 * window,
		maxItems: maxItems,
		flush:    flush,
		now:      time.Now,
		groups:   map[batchKey]*pendingGroup{},
		flushing: map[types.NamespacedName]int{},
		assigned: map[types.NamespacedName]assignment{},
	}
}

// Add queues an Artifact. Adding an Artifact that is already queued is a no-op
// (apart from extending the debounce window).
func (b *artifactBatcher) Add(key batchKey, name types.NamespacedName) {
	b.mu.Lock()
	defer b.mu.Unlock()

	g, ok := b.groups[key]
	if !ok {
		g = &pendingGroup{first: b.now(), names: map[types.NamespacedName]struct{}{}}
		b.groups[key] = g
	}
	g.names[name] = struct{}{}

	switch {
	case len(g.names) >= b.maxItems || b.window == 0:
		b.flushLocked(key, g)
	case g.timer == nil:
		g.timer = time.AfterFunc(b.window, func() { b.flushKey(key, g) })
	default:
		// Debounce: wait for a quiet window, but never longer than maxDelay
		// after the first Artifact, so a steady trickle cannot starve a group.
		wait := min(b.window, b.maxDelay-b.now().Sub(g.first))
		if wait < 0 {
			wait = 0
		}
		g.timer.Reset(wait)
	}
}

// Pending reports whether the Artifact is queued, or its group is being
// flushed right now.
func (b *artifactBatcher) Pending(key batchKey, name types.NamespacedName) bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.flushing[name] > 0 {
		return true
	}
	g, ok := b.groups[key]
	if !ok {
		return false
	}
	_, ok = g.names[name]
	return ok
}

// Assigned records that the Artifact was put in a Job that writes `output`.
func (b *artifactBatcher) Assigned(name types.NamespacedName, output string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	now := b.now()
	for n, a := range b.assigned {
		if now.After(a.expires) {
			delete(b.assigned, n)
		}
	}
	b.assigned[name] = assignment{output: output, expires: now.Add(assignmentTTL)}
}

// RecentlyAssigned reports whether the Artifact was put in a Job for `output`
// within the last assignmentTTL.
func (b *artifactBatcher) RecentlyAssigned(name types.NamespacedName, output string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	a, ok := b.assigned[name]
	return ok && a.output == output && !b.now().After(a.expires)
}

func (b *artifactBatcher) flushKey(key batchKey, g *pendingGroup) {
	b.mu.Lock()
	defer b.mu.Unlock()
	// The group may already have been flushed (max items) and replaced.
	if b.groups[key] != g {
		return
	}
	b.flushLocked(key, g)
}

// flushLocked removes the group and hands it to the flush func on its own
// goroutine, so API calls never run under the lock. Until the flush returns
// (and has called Assigned), the names still count as Pending.
func (b *artifactBatcher) flushLocked(key batchKey, g *pendingGroup) {
	if g.timer != nil {
		g.timer.Stop()
	}
	delete(b.groups, key)
	names := make([]types.NamespacedName, 0, len(g.names))
	for n := range g.names {
		names = append(names, n)
		b.flushing[n]++
	}
	go func() {
		defer b.flushDone(names)
		b.flush(key, names)
	}()
}

func (b *artifactBatcher) flushDone(names []types.NamespacedName) {
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, n := range names {
		if b.flushing[n]--; b.flushing[n] <= 0 {
			delete(b.flushing, n)
		}
	}
}
