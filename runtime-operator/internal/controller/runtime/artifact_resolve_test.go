package runtime

import (
	"testing"

	corev1 "k8s.io/api/core/v1"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

func TestResolveArtifactName_ArtifactFrom(t *testing.T) {
	a := &runtimev1alpha1.WorkloadDeploymentArtifact{
		Name: "local",
		ArtifactFrom: &corev1.LocalObjectReference{
			Name: "my-artifact",
		},
	}
	got, err := resolveArtifactName(a)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != "my-artifact" {
		t.Errorf("got %q, want %q", got, "my-artifact")
	}
}

func TestResolveArtifactName_InlineImage(t *testing.T) {
	a := &runtimev1alpha1.WorkloadDeploymentArtifact{
		Name:  "local",
		Image: "ghcr.io/acme/comp:v1",
	}
	got, err := resolveArtifactName(a)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got[:5] != "auto-" {
		t.Errorf("expected auto- prefix, got %q", got)
	}
	if len(got) != len("auto-")+12 {
		t.Errorf("expected 17-char name, got %q (len %d)", got, len(got))
	}
}

func TestResolveArtifactName_InlineImageDeterministic(t *testing.T) {
	a := &runtimev1alpha1.WorkloadDeploymentArtifact{Name: "a", Image: "ghcr.io/acme/comp:v1"}
	b := &runtimev1alpha1.WorkloadDeploymentArtifact{Name: "b", Image: "ghcr.io/acme/comp:v1"}
	na, _ := resolveArtifactName(a)
	nb, _ := resolveArtifactName(b)
	if na != nb {
		t.Errorf("two WDs with same image produced different names: %q vs %q", na, nb)
	}
}

func TestResolveArtifactName_PullSecretChangesHash(t *testing.T) {
	a := &runtimev1alpha1.WorkloadDeploymentArtifact{Name: "a", Image: "ghcr.io/acme/comp:v1"}
	b := &runtimev1alpha1.WorkloadDeploymentArtifact{
		Name:            "b",
		Image:           "ghcr.io/acme/comp:v1",
		ImagePullSecret: &corev1.LocalObjectReference{Name: "secret"},
	}
	na, _ := resolveArtifactName(a)
	nb, _ := resolveArtifactName(b)
	if na == nb {
		t.Errorf("pull secret should change hash, got same: %q", na)
	}
}

func TestResolveArtifactName_NeitherSet(t *testing.T) {
	a := &runtimev1alpha1.WorkloadDeploymentArtifact{Name: "local"}
	_, err := resolveArtifactName(a)
	if err == nil {
		t.Error("expected error when neither artifactFrom nor image is set")
	}
}

func TestResolveArtifactName_BothSet(t *testing.T) {
	a := &runtimev1alpha1.WorkloadDeploymentArtifact{
		Name:         "local",
		ArtifactFrom: &corev1.LocalObjectReference{Name: "my-artifact"},
		Image:        "ghcr.io/acme/comp:v1",
	}
	_, err := resolveArtifactName(a)
	if err == nil {
		t.Error("expected error when both artifactFrom and image are set")
	}
}
