package runtime

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"

	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	utilrand "k8s.io/apimachinery/pkg/util/rand"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/event"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// PrecompileBatchConfig turns on batching: pending Artifacts are collected for
// a short window and precompiled by one Indexed Job per group. Each index (pod)
// runs `wash-precompile --batch-file` over its slice of the items, so at most
// MaxParallelPods pods run per Job, and the pod start is paid once per pod
// instead of once per Artifact. The worker image must support batch mode.
type PrecompileBatchConfig struct {
	// Window is the debounce window: a group is flushed when no new Artifact
	// arrived for this long (and at the latest 4x Window after the first).
	Window time.Duration
	// MaxParallelPods is the parallelism (and completions) of one batch Job.
	MaxParallelPods int
	// MaxItemsPerJob flushes a group early when it reaches this size.
	MaxItemsPerJob int
	// APIReader reads the worker Pods of a failed index (their termination
	// message has the per-item result). Pods in Artifact namespaces are not
	// in the operator's cache. When nil, a failed index fails all its items.
	APIReader client.Reader
}

// MaxPrecompileBatchItems bounds the Artifacts in one batch Job. The item
// manifest is a Job annotation (all annotations together must stay under
// 256 KiB), at about 300 bytes per item.
const MaxPrecompileBatchItems = 500

const (
	// PrecompileBatchLabel marks batch Jobs, their Pods and their ConfigMaps.
	PrecompileBatchLabel = "runtime.wasmcloud.dev/precompile-batch"
	// precompileBatchItemsAnnotation holds the JSON item manifest of a batch
	// Job, so the operator can map results back without reading the ConfigMap.
	precompileBatchItemsAnnotation = "runtime.wasmcloud.dev/precompile-batch-items"

	precompileBatchJobPrefix  = "precompile-batch-"
	precompileBatchMountPath  = "/etc/precompile-batch"
	precompileContainerName   = "precompile"
	precompileBatchVolumeName = "batch"

	// workerExitPermanent is the exit code of wash-precompile when every failed
	// item failed permanently (invalid wasm, image not found).
	workerExitPermanent = 2
	// batchBackoffLimitPerIndex retries a failed index (pod) twice, unless it
	// exited with workerExitPermanent. A retry runs all items of the index again.
	batchBackoffLimitPerIndex = 2
	// batchJobTTLSeconds deletes a finished batch Job (and its ConfigMap and
	// Pods) after 10 minutes. Results are recorded on the Artifacts long before.
	batchJobTTLSeconds = 600
	// batchFlushRetryDelay is how long a group waits before it is queued again
	// after the Job could not be created.
	batchFlushRetryDelay = 5 * time.Second

	reasonBatchQueued       = "Queued"
	reasonBatchJobInFlight  = "JobInFlight"
	reasonBatchSucceeded    = "JobSucceeded"
	reasonBatchFailed       = "PrecompileFailed"
	reasonBatchPermanentErr = "PermanentError"
)

// batchItem is one Artifact in a batch Job.
type batchItem struct {
	Artifact string    `json:"artifact"`
	UID      types.UID `json:"uid"`
	Image    string    `json:"image"`
	Output   string    `json:"output"`
	// Index is the Job completion index (the pod) that runs this item.
	Index int `json:"index"`
	// Item is the 0-based line of this item in the batch file of its index;
	// the worker reports results by this number.
	Item int `json:"item"`
}

// workerSummary is the JSON the worker writes to its termination log.
type workerSummary struct {
	Total  int   `json:"total"`
	OK     []int `json:"ok"`
	Failed []struct {
		Index     int    `json:"index"`
		Permanent bool   `json:"permanent"`
		Error     string `json:"error"`
	} `json:"failed"`
	// Error is set when the batch could not start at all.
	Error     string `json:"error"`
	Permanent bool   `json:"permanent"`
}

func (r *PrecompileReconciler) batchKeyOf(a *runtimev1alpha1.Artifact) batchKey {
	k := batchKey{Namespace: a.Namespace}
	if a.Spec.ImagePullSecret != nil {
		k.PullSecret = a.Spec.ImagePullSecret.Name
	}
	return k
}

// reconcileBatched is the Reconcile of batch mode.
func (r *PrecompileReconciler) reconcileBatched(ctx context.Context, a *runtimev1alpha1.Artifact) (ctrl.Result, error) {
	current := runtimev1alpha1.PrecompiledVariant{
		Target:          r.Target,
		WasmtimeVersion: r.WasmtimeVersion,
		ArtifactURL:     r.outputURLOf(a),
		ImageRef:        a.Spec.Image,
	}
	if variantRecorded(a.Status.Precompiled, current) {
		return ctrl.Result{}, r.dropSupersededVariants(ctx, a, current)
	}
	if failedForGeneration(a) {
		// Already failed for this spec; wait for a spec change (a new image),
		// as the one-Job-per-Artifact path does.
		return ctrl.Result{}, nil
	}

	job, item, err := r.findBatchItem(ctx, a, current.ArtifactURL)
	if err != nil {
		return ctrl.Result{}, err
	}
	if job != nil {
		return r.applyBatchItemState(ctx, a, job, item)
	}

	// No Job holds the current image of this Artifact (none yet, the image
	// changed during a batch, or the finished Job was deleted by its TTL).
	name := types.NamespacedName{Namespace: a.Namespace, Name: a.Name}
	if a.DeletionTimestamp != nil {
		return ctrl.Result{}, nil
	}
	key := r.batchKeyOf(a)
	if !r.batcher.Pending(key, name) && !r.batcher.RecentlyAssigned(name, current.ArtifactURL) {
		r.batcher.Add(key, name)
	}
	return ctrl.Result{}, r.setProgressing(ctx, a, reasonBatchQueued)
}

// findBatchItem returns the newest batch Job that holds the current image and
// output of the Artifact, and its item.
func (r *PrecompileReconciler) findBatchItem(
	ctx context.Context, a *runtimev1alpha1.Artifact, output string,
) (*batchv1.Job, batchItem, error) {
	var jobs batchv1.JobList
	if err := r.List(ctx, &jobs,
		client.InNamespace(a.Namespace),
		client.MatchingLabels{PrecompileBatchLabel: "true"},
	); err != nil {
		return nil, batchItem{}, err
	}
	sort.Slice(jobs.Items, func(i, j int) bool {
		return jobs.Items[j].CreationTimestamp.Before(&jobs.Items[i].CreationTimestamp)
	})
	for i := range jobs.Items {
		job := &jobs.Items[i]
		if job.DeletionTimestamp != nil || !ownedBy(job, a.UID) {
			continue
		}
		items, err := batchItemsOf(job)
		if err != nil {
			ctrl.LoggerFrom(ctx).Error(err, "ignoring batch Job with an unreadable item manifest", "job", job.Name)
			continue
		}
		for _, it := range items {
			if it.UID == a.UID && it.Image == a.Spec.Image && it.Output == output {
				return job, it, nil
			}
		}
	}
	return nil, batchItem{}, nil
}

func ownedBy(o metav1.Object, uid types.UID) bool {
	for _, ref := range o.GetOwnerReferences() {
		if ref.UID == uid {
			return true
		}
	}
	return false
}

func batchItemsOf(job *batchv1.Job) ([]batchItem, error) {
	var items []batchItem
	if err := json.Unmarshal([]byte(job.Annotations[precompileBatchItemsAnnotation]), &items); err != nil {
		return nil, fmt.Errorf("parse %s: %w", precompileBatchItemsAnnotation, err)
	}
	return items, nil
}

// batchItemState is the result of one item as far as it is known.
type batchItemState struct {
	done      bool
	succeeded bool
	permanent bool
	message   string
}

func (r *PrecompileReconciler) applyBatchItemState(
	ctx context.Context, a *runtimev1alpha1.Artifact, job *batchv1.Job, item batchItem,
) (ctrl.Result, error) {
	state, err := r.batchItemState(ctx, job, item)
	if err != nil {
		return ctrl.Result{}, err
	}
	switch {
	case !state.done:
		return ctrl.Result{}, r.setProgressing(ctx, a, reasonBatchJobInFlight)
	case state.succeeded:
		return ctrl.Result{}, r.recordBatchSuccess(ctx, a)
	default:
		return ctrl.Result{}, r.recordBatchFailure(ctx, a, state)
	}
}

// batchItemState reads the result of an item from the Job status. A completed
// index means every item of that pod succeeded. For a failed index the
// termination message of its pods tells which items failed.
func (r *PrecompileReconciler) batchItemState(
	ctx context.Context, job *batchv1.Job, item batchItem,
) (batchItemState, error) {
	completed, err := parseIndexes(job.Status.CompletedIndexes)
	if err != nil {
		return batchItemState{}, err
	}
	if completed[item.Index] {
		return batchItemState{done: true, succeeded: true}, nil
	}

	failedIndexes := ""
	if job.Status.FailedIndexes != nil {
		failedIndexes = *job.Status.FailedIndexes
	}
	failed, err := parseIndexes(failedIndexes)
	if err != nil {
		return batchItemState{}, err
	}
	finished, jobMsg := jobFinished(job)
	if !failed[item.Index] && !finished {
		return batchItemState{}, nil
	}

	if state, ok := r.itemResultFromPods(ctx, job, item); ok {
		return state, nil
	}
	if failed[item.Index] {
		return batchItemState{done: true, message: fmt.Sprintf(
			"precompile Job %s: index %d failed", job.Name, item.Index)}, nil
	}
	// The Job ended without this index (for example a deadline).
	return batchItemState{done: true, message: fmt.Sprintf(
		"precompile Job %s ended before index %d completed: %s", job.Name, item.Index, jobMsg)}, nil
}

func jobFinished(j *batchv1.Job) (bool, string) {
	if failed, msg := jobFailed(j); failed {
		return true, msg
	}
	return jobComplete(j), ""
}

// itemResultFromPods looks for the item in the termination messages of the
// pods of its index. The newest result wins: a retry can succeed after a
// failed attempt. ok is false when no pod reported on the item.
func (r *PrecompileReconciler) itemResultFromPods(
	ctx context.Context, job *batchv1.Job, item batchItem,
) (batchItemState, bool) {
	if r.Batch == nil || r.Batch.APIReader == nil {
		return batchItemState{}, false
	}
	var pods corev1.PodList
	if err := r.Batch.APIReader.List(ctx, &pods,
		client.InNamespace(job.Namespace),
		client.MatchingLabels{batchv1.JobNameLabel: job.Name},
	); err != nil {
		ctrl.LoggerFrom(ctx).V(1).Info("cannot list batch worker pods", "job", job.Name, "error", err.Error())
		return batchItemState{}, false
	}

	type result struct {
		at    time.Time
		state batchItemState
	}
	var results []result
	index := strconv.Itoa(item.Index)
	for i := range pods.Items {
		p := &pods.Items[i]
		if !ownedBy(p, job.UID) || p.Annotations[batchv1.JobCompletionIndexAnnotation] != index {
			continue
		}
		for _, cs := range p.Status.ContainerStatuses {
			t := cs.State.Terminated
			if cs.Name != precompileContainerName || t == nil {
				continue
			}
			if state, ok := itemResultFromMessage(t.Message, item.Item, t.ExitCode); ok {
				results = append(results, result{at: t.FinishedAt.Time, state: state})
			}
		}
	}
	if len(results) == 0 {
		return batchItemState{}, false
	}
	newest := slices.MaxFunc(results, func(x, y result) int { return x.at.Compare(y.at) })
	return newest.state, true
}

// itemResultFromMessage reads one item from a worker termination message.
func itemResultFromMessage(msg string, item int, exitCode int32) (batchItemState, bool) {
	if msg == "" {
		return batchItemState{}, false
	}
	var s workerSummary
	if err := json.Unmarshal([]byte(msg), &s); err != nil {
		return batchItemState{}, false
	}
	if s.Error != "" {
		return batchItemState{done: true, permanent: s.Permanent, message: s.Error}, true
	}
	if slices.Contains(s.OK, item) {
		return batchItemState{done: true, succeeded: true}, true
	}
	for _, f := range s.Failed {
		if f.Index == item {
			msg := f.Error
			if msg == "" {
				msg = fmt.Sprintf("precompile failed (worker exit code %d)", exitCode)
			}
			return batchItemState{done: true, permanent: f.Permanent, message: msg}, true
		}
	}
	return batchItemState{}, false
}

// parseIndexes parses a Job index list like "0,2-4,7".
func parseIndexes(s string) (map[int]bool, error) {
	out := map[int]bool{}
	if s == "" {
		return out, nil
	}
	for _, part := range strings.Split(s, ",") {
		lo, hi, isRange := strings.Cut(part, "-")
		from, err := strconv.Atoi(lo)
		if err != nil {
			return nil, fmt.Errorf("invalid index list %q: %w", s, err)
		}
		to := from
		if isRange {
			if to, err = strconv.Atoi(hi); err != nil {
				return nil, fmt.Errorf("invalid index list %q: %w", s, err)
			}
		}
		if to < from {
			return nil, fmt.Errorf("invalid index list %q: range %s", s, part)
		}
		for i := from; i <= to; i++ {
			out[i] = true
		}
	}
	return out, nil
}

// failedForGeneration reports whether the Artifact already failed for its
// current spec.
func failedForGeneration(a *runtimev1alpha1.Artifact) bool {
	c := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed)
	return c.Status == condition.ConditionTrue && c.ObservedGeneration == a.Generation
}

func (r *PrecompileReconciler) setProgressing(ctx context.Context, a *runtimev1alpha1.Artifact, reason condition.ConditionReason) error {
	c := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing)
	if c.Status == condition.ConditionTrue && c.Reason == reason {
		return nil
	}
	base := a.DeepCopy()
	a.Status.SetConditions(condition.Condition{
		Type:               runtimev1alpha1.ArtifactConditionPrecompileProgressing,
		Status:             condition.ConditionTrue,
		Reason:             reason,
		LastTransitionTime: metav1.Now(),
	})
	return r.patchStatus(ctx, a, base)
}

func (r *PrecompileReconciler) recordBatchSuccess(ctx context.Context, a *runtimev1alpha1.Artifact) error {
	base := a.DeepCopy()
	// Replace rather than append: see dropSupersededVariants.
	a.Status.Precompiled = []runtimev1alpha1.PrecompiledVariant{{
		Target:          r.Target,
		WasmtimeVersion: r.WasmtimeVersion,
		ArtifactURL:     r.outputURLOf(a),
		ImageRef:        a.Spec.Image,
	}}
	now := metav1.Now()
	a.Status.SetConditions(
		condition.ReadyCondition(runtimev1alpha1.ArtifactConditionPrecompiled),
		condition.Condition{
			Type:               runtimev1alpha1.ArtifactConditionPrecompileProgressing,
			Status:             condition.ConditionFalse,
			Reason:             reasonBatchSucceeded,
			LastTransitionTime: now,
		},
	)
	if a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed).Status == condition.ConditionTrue {
		a.Status.SetConditions(condition.Condition{
			Type:               runtimev1alpha1.ArtifactConditionPrecompileFailed,
			Status:             condition.ConditionFalse,
			Reason:             reasonBatchSucceeded,
			LastTransitionTime: now,
		})
	}
	return r.patchStatus(ctx, a, base)
}

func (r *PrecompileReconciler) recordBatchFailure(ctx context.Context, a *runtimev1alpha1.Artifact, state batchItemState) error {
	reason := condition.ConditionReason(reasonBatchFailed)
	if state.permanent {
		reason = reasonBatchPermanentErr
	}
	existing := a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileFailed)
	if existing.Status == condition.ConditionTrue && existing.Message == state.message &&
		existing.ObservedGeneration == a.Generation {
		return nil
	}
	base := a.DeepCopy()
	now := metav1.Now()
	a.Status.SetConditions(
		condition.Condition{
			Type:               runtimev1alpha1.ArtifactConditionPrecompileFailed,
			Status:             condition.ConditionTrue,
			Reason:             reason,
			Message:            state.message,
			ObservedGeneration: a.Generation,
			LastTransitionTime: now,
		},
		condition.Condition{
			Type:               runtimev1alpha1.ArtifactConditionPrecompileProgressing,
			Status:             condition.ConditionFalse,
			Reason:             reason,
			LastTransitionTime: now,
		},
	)
	return r.patchStatus(ctx, a, base)
}

// flushBatch creates one batch Job for a group of Artifacts. It runs on its own
// goroutine (see artifactBatcher), outside any reconcile.
func (r *PrecompileReconciler) flushBatch(key batchKey, names []types.NamespacedName) {
	ctx := context.Background()
	log := ctrl.Log.WithName("precompile-batch").WithValues("namespace", key.Namespace)

	artifacts := make([]*runtimev1alpha1.Artifact, 0, len(names))
	for _, n := range names {
		var a runtimev1alpha1.Artifact
		if err := r.Get(ctx, n, &a); err != nil {
			if !apierrors.IsNotFound(err) {
				log.Error(err, "cannot read Artifact for batch; retrying", "artifact", n.Name)
				r.requeueLater(n)
			}
			continue
		}
		if a.DeletionTimestamp != nil || r.batchKeyOf(&a) != key || failedForGeneration(&a) {
			// A changed pull secret puts it in another group on its next reconcile.
			r.requeueLater(n)
			continue
		}
		if variantRecorded(a.Status.Precompiled, runtimev1alpha1.PrecompiledVariant{
			Target: r.Target, WasmtimeVersion: r.WasmtimeVersion, ImageRef: a.Spec.Image,
		}) {
			continue
		}
		if strings.ContainsAny(a.Spec.Image, " \t\r\n") {
			// The batch file is whitespace separated; fail just this Artifact.
			if err := r.recordBatchFailure(ctx, &a, batchItemState{
				done: true, permanent: true, message: "image reference contains whitespace",
			}); err != nil {
				log.Error(err, "cannot record failure", "artifact", a.Name)
			}
			continue
		}
		artifacts = append(artifacts, &a)
	}
	if len(artifacts) == 0 {
		return
	}
	sort.Slice(artifacts, func(i, j int) bool { return artifacts[i].Name < artifacts[j].Name })

	job, cm, items := r.buildBatch(key, artifacts)
	if err := r.createBatch(ctx, job, cm); err != nil {
		log.Error(err, "cannot create precompile batch Job; retrying", "artifacts", len(artifacts))
		for _, a := range artifacts {
			r.requeueLater(types.NamespacedName{Namespace: a.Namespace, Name: a.Name})
		}
		return
	}
	for _, it := range items {
		r.batcher.Assigned(types.NamespacedName{Namespace: key.Namespace, Name: it.Artifact}, it.Output)
	}
	log.Info("created precompile batch Job", "job", job.Name, "artifacts", len(items),
		"pods", *job.Spec.Parallelism)
}

// requeueLater reconciles the Artifact again after a short delay.
func (r *PrecompileReconciler) requeueLater(n types.NamespacedName) {
	time.AfterFunc(batchFlushRetryDelay, func() {
		a := &runtimev1alpha1.Artifact{}
		a.SetNamespace(n.Namespace)
		a.SetName(n.Name)
		r.requeue <- event.GenericEvent{Object: a}
	})
}

// createBatch creates the ConfigMap, then the Job, then makes the Job the
// owner of the ConfigMap, so the ConfigMap goes away with the Job (TTL). The
// pods never wait for their batch file. If the operator stops between the
// calls, an unowned ConfigMap with the batch label is left behind.
func (r *PrecompileReconciler) createBatch(ctx context.Context, job *batchv1.Job, cm *corev1.ConfigMap) error {
	if err := r.Create(ctx, cm); err != nil {
		return fmt.Errorf("create ConfigMap: %w", err)
	}
	if err := r.Create(ctx, job); err != nil {
		if delErr := r.Delete(ctx, cm); delErr != nil && !apierrors.IsNotFound(delErr) {
			ctrl.Log.Error(delErr, "cannot delete batch ConfigMap", "configmap", cm.Name)
		}
		return fmt.Errorf("create Job: %w", err)
	}
	base := cm.DeepCopy()
	cm.OwnerReferences = []metav1.OwnerReference{{
		APIVersion: batchv1.SchemeGroupVersion.String(),
		Kind:       "Job",
		Name:       job.Name,
		UID:        job.UID,
	}}
	if err := r.Patch(ctx, cm, client.MergeFrom(base)); err != nil {
		// The Job runs anyway; only the cleanup of the ConfigMap is lost.
		ctrl.Log.Error(err, "cannot make the batch Job own its ConfigMap", "configmap", cm.Name)
	}
	return nil
}

// buildBatch builds the Job, its ConfigMap and the item manifest. Items are
// spread round-robin over min(MaxParallelPods, len) indexes; index i reads the
// ConfigMap key "i".
func (r *PrecompileReconciler) buildBatch(
	key batchKey, artifacts []*runtimev1alpha1.Artifact,
) (*batchv1.Job, *corev1.ConfigMap, []batchItem) {
	pods := min(max(r.Batch.MaxParallelPods, 1), len(artifacts))

	items := make([]batchItem, 0, len(artifacts))
	files := make([]strings.Builder, pods)
	perPod := make([]int, pods)
	owners := make([]metav1.OwnerReference, 0, len(artifacts))
	for n, a := range artifacts {
		index := n % pods
		it := batchItem{
			Artifact: a.Name,
			UID:      a.UID,
			Image:    a.Spec.Image,
			Output:   r.outputURLOf(a),
			Index:    index,
			Item:     perPod[index],
		}
		perPod[index]++
		fmt.Fprintf(&files[index], "%s %s\n", it.Image, it.Output)
		items = append(items, it)
		// Every Artifact owns the Job (none as controller): events map back to
		// all of them, and the Job is garbage collected once all are deleted.
		owners = append(owners, metav1.OwnerReference{
			APIVersion: runtimev1alpha1.GroupVersion.String(),
			Kind:       "Artifact",
			Name:       a.Name,
			UID:        a.UID,
		})
	}

	data := make(map[string]string, pods)
	keyItems := make([]corev1.KeyToPath, 0, pods)
	for i := range files {
		k := strconv.Itoa(i)
		data[k] = files[i].String()
		keyItems = append(keyItems, corev1.KeyToPath{Key: k, Path: k})
	}

	manifest, _ := json.Marshal(items) // plain structs; cannot fail
	labels := map[string]string{PrecompileBatchLabel: "true"}
	// A random name, not generateName: the pod template (with the ConfigMap
	// volume) cannot change after the create, so the name must be known first.
	name := precompileBatchJobPrefix + utilrand.String(10)

	container := corev1.Container{
		Name:  precompileContainerName,
		Image: r.WorkerImage,
		Args: []string{
			"--batch-file", precompileBatchMountPath + "/$(JOB_COMPLETION_INDEX)",
		},
		Env: slices.Clone(r.ArtifactStore.Env),
		VolumeMounts: []corev1.VolumeMount{{
			Name: precompileBatchVolumeName, MountPath: precompileBatchMountPath, ReadOnly: true,
		}},
		TerminationMessagePolicy: corev1.TerminationMessageReadFile,
	}
	if r.InsecureRegistries != "" {
		container.Env = append(container.Env, corev1.EnvVar{Name: "INSECURE_REGISTRIES", Value: r.InsecureRegistries})
	}
	volumes := []corev1.Volume{{
		Name: precompileBatchVolumeName,
		VolumeSource: corev1.VolumeSource{ConfigMap: &corev1.ConfigMapVolumeSource{
			LocalObjectReference: corev1.LocalObjectReference{Name: name},
			Items:                keyItems,
		}},
	}}
	if key.PullSecret != "" {
		container.Env = append(container.Env, corev1.EnvVar{Name: "DOCKER_CONFIG", Value: "/etc/docker-creds"})
		container.VolumeMounts = append(container.VolumeMounts, corev1.VolumeMount{
			Name: "docker-creds", MountPath: "/etc/docker-creds", ReadOnly: true,
		})
		volumes = append(volumes, corev1.Volume{
			Name: "docker-creds",
			VolumeSource: corev1.VolumeSource{Secret: &corev1.SecretVolumeSource{
				SecretName: key.PullSecret,
				Items:      []corev1.KeyToPath{{Key: ".dockerconfigjson", Path: "config.json"}},
			}},
		})
	}

	completions := int32(pods) //nolint:gosec // bounded by MaxParallelPods
	job := &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{
			Name:            name,
			Namespace:       key.Namespace,
			Labels:          labels,
			Annotations:     map[string]string{precompileBatchItemsAnnotation: string(manifest)},
			OwnerReferences: owners,
		},
		Spec: batchv1.JobSpec{
			CompletionMode:          ptrTo(batchv1.IndexedCompletion),
			Completions:             ptrTo(completions),
			Parallelism:             ptrTo(completions),
			BackoffLimitPerIndex:    ptrTo(int32(batchBackoffLimitPerIndex)),
			MaxFailedIndexes:        ptrTo(completions),
			TTLSecondsAfterFinished: ptrTo(int32(batchJobTTLSeconds)),
			PodFailurePolicy: &batchv1.PodFailurePolicy{Rules: []batchv1.PodFailurePolicyRule{
				{
					// Invalid wasm or a missing image: a retry gives the same result.
					Action: batchv1.PodFailurePolicyActionFailIndex,
					OnExitCodes: &batchv1.PodFailurePolicyOnExitCodesRequirement{
						ContainerName: ptrTo(precompileContainerName),
						Operator:      batchv1.PodFailurePolicyOnExitCodesOpIn,
						Values:        []int32{workerExitPermanent},
					},
				},
				{
					// A node drain or preemption is not the worker's fault.
					Action: batchv1.PodFailurePolicyActionIgnore,
					OnPodConditions: []batchv1.PodFailurePolicyOnPodConditionsPattern{{
						Type: corev1.DisruptionTarget, Status: corev1.ConditionTrue,
					}},
				},
			}},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labels},
				Spec: corev1.PodSpec{
					RestartPolicy: corev1.RestartPolicyNever,
					Containers:    []corev1.Container{container},
					Volumes:       volumes,
				},
			},
		},
	}
	cm := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: key.Namespace, Labels: labels},
		Data:       data,
	}
	return job, cm, items
}

func ptrTo[T any](v T) *T { return &v }
