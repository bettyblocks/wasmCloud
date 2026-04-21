package runtime_operator

// This will be moved to go.wasmcloud.dev/runtime-operator/operator.go

import (
	"context"
	"fmt"
	"time"

	"github.com/nats-io/nats.go"
	runtime_controllers "go.wasmcloud.dev/runtime-operator/v2/internal/controller/runtime"
	"go.wasmcloud.dev/runtime-operator/v2/pkg/wasmbus"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/manager"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
	"go.wasmcloud.dev/runtime-operator/v2/internal/precompile"
	natsprecompile "go.wasmcloud.dev/runtime-operator/v2/internal/precompile/nats"
)

type EmbeddedOperatorConfig struct {
	// NATS connection string. Used to communicate with hosts.
	NatsURL string
	// NATS options. Used to configure the NATS connection.
	NatsOptions []nats.Option
	// Heartbeat TTL. Used to determine how long to wait before considering a host unreachable.
	HeartbeatTTL time.Duration
	// Host CPU threshold (percentage).
	// Used to calculate workload scheduling, avoiding hosts that are over this threshold.
	HostCPUThreshold float64
	// Host Memory threshold (percentage).
	// Used to calculate workload scheduling, avoiding hosts that are over this threshold.
	HostMemoryThreshold float64
	// Disable Artifact Controller. If set, Artifacts must be marked as 'Ready' elsewhere.
	// Useful when introducing a custom artifact management solution.
	DisableArtifactController bool
	// Namespace restricts the HostPodReconciler to watching Pods in this
	// namespace only. Should match the namespace the operator is deployed in.
	// If empty, all namespaces are watched.
	Namespace string

	// PrecompileEnabled turns on the out-of-process precompile pipeline:
	// the Artifact reconciler fans compile jobs out to a NATS JetStream
	// stream, and a goroutine subscribes to completion events and patches
	// Artifact.status.precompiled. Hosts then prefer precompiled variants
	// over inline compilation.
	PrecompileEnabled bool

	// PrecompileTargetMatrix is the set of target triples the operator
	// produces variants for. Defaults to a single target if empty.
	PrecompileTargetMatrix []string

	// PrecompileStream is the JetStream stream name the worker pool consumes.
	PrecompileStream string

	// DefaultHostPoolTargets is forwarded to the WorkloadDeployment reconciler
	// as the fallback HostPoolTargets when a WD does not set its own. Should
	// be a subset of PrecompileTargetMatrix.
	DefaultHostPoolTargets []string

	// AutoArtifactCleanupInterval is how often the cleanup loop scans for
	// orphaned auto-created Artifacts. Zero disables cleanup.
	AutoArtifactCleanupInterval time.Duration

	// AutoArtifactCleanupTTL is the minimum age an unreferenced auto-Artifact
	// must reach before the cleaner deletes it. Defaults to 24h when
	// AutoArtifactCleanupInterval is set and this is zero.
	AutoArtifactCleanupTTL time.Duration
}

// EmbeddedOperator is the main struct for the embedded operator.
// It allows embedding the Runtime Operator into other applications.
type EmbeddedOperator struct {
	Bus      wasmbus.Bus
	NatsConn *nats.Conn
}

// NewEmbeddedOperator creates a new EmbeddedOperator.
func NewEmbeddedOperator(
	ctx context.Context,
	mgr manager.Manager,
	cfg EmbeddedOperatorConfig,
) (*EmbeddedOperator, error) {
	nc, err := wasmbus.NatsConnect(cfg.NatsURL, cfg.NatsOptions...)
	if err != nil {
		return nil, err
	}
	bus := wasmbus.NewNatsBus(nc)

	var jobProducer precompile.JobProducer
	if cfg.PrecompileEnabled {
		streamName := cfg.PrecompileStream
		if streamName == "" {
			streamName = natsprecompile.DefaultStream
		}
		producer, err := natsprecompile.NewProducer(nc, streamName)
		if err != nil {
			return nil, fmt.Errorf("creating precompile job producer: %w", err)
		}
		jobProducer = producer

		if err := startPrecompileCompletionConsumer(ctx, nc, mgr.GetClient()); err != nil {
			return nil, fmt.Errorf("subscribing to precompile completions: %w", err)
		}
	}

	if !cfg.DisableArtifactController {
		targetMatrix := cfg.PrecompileTargetMatrix
		if cfg.PrecompileEnabled && len(targetMatrix) == 0 {
			targetMatrix = []string{"x86_64-unknown-linux-gnu"}
		}
		if err = (&runtime_controllers.ArtifactReconciler{
			Client:       mgr.GetClient(),
			Scheme:       mgr.GetScheme(),
			JobProducer:  jobProducer,
			TargetMatrix: targetMatrix,
		}).SetupWithManager(mgr); err != nil {
			return nil, err
		}
	}

	if err = (&runtime_controllers.HostReconciler{
		Client:             mgr.GetClient(),
		Scheme:             mgr.GetScheme(),
		Bus:                bus,
		UnreachableTimeout: cfg.HeartbeatTTL,
		CPUThreshold:       cfg.HostCPUThreshold,
		MemoryThreshold:    cfg.HostMemoryThreshold,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.HostPodReconciler{
		Client:    mgr.GetClient(),
		Scheme:    mgr.GetScheme(),
		Namespace: cfg.Namespace,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadReconciler{
		Client: mgr.GetClient(),
		Scheme: mgr.GetScheme(),
		Bus:    bus,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadReplicaSetReconciler{
		Client: mgr.GetClient(),
		Scheme: mgr.GetScheme(),
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadDeploymentReconciler{
		Client:                 mgr.GetClient(),
		Scheme:                 mgr.GetScheme(),
		DefaultHostPoolTargets: cfg.DefaultHostPoolTargets,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadRouteReconciler{
		Client: mgr.GetClient(),
		Scheme: mgr.GetScheme(),
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if cfg.AutoArtifactCleanupInterval > 0 {
		ttl := cfg.AutoArtifactCleanupTTL
		if ttl <= 0 {
			ttl = 24 * time.Hour
		}
		if err := mgr.Add(&runtime_controllers.AutoArtifactCleaner{
			Client:   mgr.GetClient(),
			Interval: cfg.AutoArtifactCleanupInterval,
			TTL:      ttl,
		}); err != nil {
			return nil, fmt.Errorf("add AutoArtifactCleaner: %w", err)
		}
	}

	return &EmbeddedOperator{
		Bus:      bus,
		NatsConn: nc,
	}, nil
}

// startPrecompileCompletionConsumer subscribes to completion events the
// precompile worker pool publishes and patches Artifact.status.precompiled
// (using a status merge patch) so the next reconcile sees the new variant.
//
// Failures to patch are logged but never block the consumer — the next
// reconcile will retry the compile job (idempotent) and produce another
// completion event.
func startPrecompileCompletionConsumer(ctx context.Context, nc *nats.Conn, c client.Client) error {
	consumer := natsprecompile.NewConsumer(nc)
	log := ctrl.Log.WithName("precompile-completion")

	return consumer.Subscribe(ctx, func(ev precompile.PrecompileCompletion) {
		if ev.Result.Err != nil {
			log.Info("precompile job failed", "namespace", ev.ArtifactNamespace, "name", ev.ArtifactName, "target", ev.Target, "err", *ev.Result.Err)
			return
		}
		if ev.Result.Ok == nil {
			log.Info("completion event has neither Ok nor Err set; ignoring",
				"namespace", ev.ArtifactNamespace, "name", ev.ArtifactName)
			return
		}
		variant := *ev.Result.Ok

		// Fetch current Artifact, append the variant if not already present, patch status.
		// Using a fresh context with a short timeout so a stuck patch doesn't pile up
		// goroutines waiting on the parent.
		patchCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
		defer cancel()

		artifact := &runtimev1alpha1.Artifact{}
		if err := c.Get(patchCtx, client.ObjectKey{Namespace: ev.ArtifactNamespace, Name: ev.ArtifactName}, artifact); err != nil {
			log.Info("artifact not found for completion event; dropping",
				"namespace", ev.ArtifactNamespace, "name", ev.ArtifactName, "err", err.Error())
			return
		}

		// Idempotent: skip if a variant with the same (target, wasmtime version, compat hash) is already there.
		for _, v := range artifact.Status.Precompiled {
			if v.Target == variant.Target &&
				v.WasmtimeVersion == variant.WasmtimeVersion &&
				v.CompatHash == variant.CompatHash {
				return
			}
		}

		patch := client.MergeFrom(artifact.DeepCopy())
		artifact.Status.Precompiled = append(artifact.Status.Precompiled, runtimev1alpha1.PrecompiledVariant{
			Target:          variant.Target,
			WasmtimeVersion: variant.WasmtimeVersion,
			CompatHash:      variant.CompatHash,
			ArtifactURL:     variant.ArtifactURL,
			Digest:          variant.Digest,
		})

		if err := c.Status().Patch(patchCtx, artifact, patch); err != nil {
			log.Info("failed to patch artifact status; will retry on next completion",
				"namespace", ev.ArtifactNamespace, "name", ev.ArtifactName, "err", err.Error())
			return
		}
		log.Info("recorded precompiled variant",
			"namespace", ev.ArtifactNamespace, "name", ev.ArtifactName, "target", variant.Target)
	})
}
