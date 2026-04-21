package runtime

import (
	"context"
	"time"

	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// AutoArtifactCleaner scans the cluster for auto-created Artifacts that no
// WorkloadDeployment references and deletes them once they're older than
// `TTL`. The existing finalizer ensures nothing is deleted while referenced;
// this runnable just prunes rows the WD-controller no longer needs.
//
// Zero value (Interval=0) disables the cleaner. Implements
// manager.Runnable so it shuts down cleanly with the manager context.
type AutoArtifactCleaner struct {
	Client client.Client
	// Interval between cleanup scans. Zero disables the cleaner.
	Interval time.Duration
	// TTL an unreferenced Artifact must exceed before deletion. Protects
	// against races where an Artifact has been created but the referencing
	// WorkloadDeployment hasn't been seen by the cache yet.
	TTL time.Duration
}

// Start runs the cleanup loop until ctx is cancelled. Errors are logged but
// do not abort the loop — the next tick retries.
func (c *AutoArtifactCleaner) Start(ctx context.Context) error {
	if c.Interval <= 0 {
		// Disabled; block until shutdown so the manager doesn't treat this as a crash.
		<-ctx.Done()
		return nil
	}
	log := ctrl.LoggerFrom(ctx).WithName("auto-artifact-cleanup")

	ticker := time.NewTicker(c.Interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil
		case <-ticker.C:
			if err := c.scan(ctx); err != nil {
				log.Error(err, "cleanup scan failed")
			}
		}
	}
}

func (c *AutoArtifactCleaner) scan(ctx context.Context) error {
	log := ctrl.LoggerFrom(ctx).WithName("auto-artifact-cleanup")

	artifacts := &runtimev1alpha1.ArtifactList{}
	if err := c.Client.List(ctx, artifacts, client.MatchingLabels{autoCreatedLabel: "true"}); err != nil {
		return err
	}

	for i := range artifacts.Items {
		artifact := &artifacts.Items[i]
		if artifact.DeletionTimestamp != nil {
			continue
		}
		age := time.Since(artifact.CreationTimestamp.Time)
		if age < c.TTL {
			continue
		}

		referenced, err := c.isReferenced(ctx, artifact)
		if err != nil {
			log.Error(err, "check references", "artifact", artifact.Name)
			continue
		}
		if referenced {
			continue
		}

		log.Info("deleting orphaned auto-Artifact",
			"namespace", artifact.Namespace, "name", artifact.Name, "age", age)
		if err := c.Client.Delete(ctx, artifact); err != nil && client.IgnoreNotFound(err) != nil {
			log.Error(err, "delete", "artifact", artifact.Name)
		}
	}
	return nil
}

func (c *AutoArtifactCleaner) isReferenced(ctx context.Context, artifact *runtimev1alpha1.Artifact) (bool, error) {
	deployments := &runtimev1alpha1.WorkloadDeploymentList{}
	if err := c.Client.List(ctx, deployments, client.InNamespace(artifact.Namespace)); err != nil {
		return false, err
	}
	for i := range deployments.Items {
		deployment := &deployments.Items[i]
		if deployment.DeletionTimestamp != nil {
			continue
		}
		for j := range deployment.Spec.Artifacts {
			configArtifact := &deployment.Spec.Artifacts[j]
			refName, err := resolveArtifactName(configArtifact)
			if err != nil {
				continue
			}
			if refName == artifact.Name {
				return true, nil
			}
		}
	}
	return false, nil
}
