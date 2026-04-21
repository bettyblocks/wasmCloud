package v1alpha1

import (
	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

const ArtifactConditionSync condition.ConditionType = "Sync"
const ArtifactConditionPublished condition.ConditionType = "Published"
const ArtifactConditionPrecompiled condition.ConditionType = "Precompiled"

// ArtifactSpec defines the desired state of Artifact.
type ArtifactSpec struct {
	// +kubebuilder:validation:Required
	Image string `json:"image"`
	// +kubebuilder:validation:Optional
	ImagePullSecret *corev1.LocalObjectReference `json:"imagePullSecret,omitempty"`
}

// PrecompiledVariant describes a single precompiled .cwasm artifact published
// for a specific host configuration.
//
// A host consumes a variant only when all three of Target, WasmtimeVersion, and
// CompatHash match its own values. Any drift forces a cache miss — either a
// fallback compile (WorkloadDeployment PrecompileMode=Fallback) or a blocked
// rollout (PrecompileMode=Strict).
type PrecompiledVariant struct {
	// Target triple this variant was compiled for, e.g. "x86_64-unknown-linux-gnu".
	// +kubebuilder:validation:Required
	Target string `json:"target"`
	// Exact wasmtime crate version the worker was built against.
	// +kubebuilder:validation:Required
	WasmtimeVersion string `json:"wasmtimeVersion"`
	// Stable hex string derived from Engine::precompile_compatibility_hash.
	// +kubebuilder:validation:Required
	CompatHash string `json:"compatHash"`
	// URL an ArtifactStore can fetch. Scheme selects the backend (e.g. nats://).
	// +kubebuilder:validation:Required
	ArtifactURL string `json:"artifactUrl"`
	// sha256 digest of the precompiled bytes, for integrity checks.
	// +kubebuilder:validation:Required
	Digest string `json:"digest"`
}

// ArtifactStatus defines the observed state of Artifact.
type ArtifactStatus struct {
	condition.ConditionedStatus `json:",inline"`
	// +kubebuilder:validation:Optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
	// +kubebuilder:validation:Optional
	ArtifactURL string `json:"artifactUrl,omitempty"`
	// Precompiled variants published for this artifact, one per host config
	// the target matrix produced. Populated by the precompile worker pipeline
	// via the operator.
	// +kubebuilder:validation:Optional
	Precompiled []PrecompiledVariant `json:"precompiled,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status

// Artifact is the Schema for the artifacts API.
type Artifact struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   ArtifactSpec   `json:"spec,omitempty"`
	Status ArtifactStatus `json:"status,omitempty"`
}

// fulfill the ConditionedStatus interface
func (a *Artifact) ConditionedStatus() *condition.ConditionedStatus {
	return &a.Status.ConditionedStatus
}

func (a *Artifact) InitializeConditionedStatus() {
}

// +kubebuilder:object:root=true

// ArtifactList contains a list of Artifact.
type ArtifactList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []Artifact `json:"items"`
}

func init() {
	SchemeBuilder.Register(&Artifact{}, &ArtifactList{})
}
