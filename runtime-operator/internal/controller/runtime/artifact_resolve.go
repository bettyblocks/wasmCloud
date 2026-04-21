package runtime

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"sigs.k8s.io/controller-runtime/pkg/client"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

const autoCreatedLabel = "runtime.wasmcloud.dev/auto-created"

// resolveArtifactName returns the Artifact CR name that a
// WorkloadDeploymentArtifact refers to, without performing any API calls.
//
// For `artifactFrom`-style entries it's the name the user provided. For
// `image`-style entries it's a deterministic `auto-<hash>` name so two
// WorkloadDeployments that inline the same image resolve to the same Artifact
// and share its precompile cache.
func resolveArtifactName(a *runtimev1alpha1.WorkloadDeploymentArtifact) (string, error) {
	switch {
	case a.ArtifactFrom != nil && a.Image == "":
		return a.ArtifactFrom.Name, nil
	case a.ArtifactFrom == nil && a.Image != "":
		return autoArtifactName(a.Image, a.ImagePullSecret), nil
	default:
		return "", fmt.Errorf(
			"artifact %q must have exactly one of artifactFrom or image set", a.Name)
	}
}

// autoArtifactName derives a stable name from an image ref and optional pull
// secret. Two WorkloadDeployments inlining the same image converge on the
// same Artifact, so the precompile pipeline runs once per unique (image,
// pullSecret) pair.
func autoArtifactName(image string, pullSecret *corev1.LocalObjectReference) string {
	h := sha256.New()
	h.Write([]byte(image))
	if pullSecret != nil {
		h.Write([]byte{0})
		h.Write([]byte(pullSecret.Name))
	}
	return "auto-" + hex.EncodeToString(h.Sum(nil))[:12]
}

// upsertInlineArtifact creates the auto-Artifact for a
// WorkloadDeploymentArtifact that inlines an image. No-op when `artifactFrom`
// is used. Already-exists is swallowed so reconciles are idempotent.
func upsertInlineArtifact(
	ctx context.Context,
	c client.Client,
	namespace string,
	a *runtimev1alpha1.WorkloadDeploymentArtifact,
) error {
	if a.Image == "" {
		return nil
	}
	name, err := resolveArtifactName(a)
	if err != nil {
		return err
	}
	artifact := &runtimev1alpha1.Artifact{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: namespace,
			Labels:    map[string]string{autoCreatedLabel: "true"},
		},
		Spec: runtimev1alpha1.ArtifactSpec{
			Image:           a.Image,
			ImagePullSecret: a.ImagePullSecret,
		},
	}
	if err := c.Create(ctx, artifact); err != nil && !apierrors.IsAlreadyExists(err) {
		return fmt.Errorf("auto-upsert Artifact %q: %w", name, err)
	}
	return nil
}
