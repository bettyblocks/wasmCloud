package v1alpha1

import (
	"go.wasmcloud.dev/runtime-operator/api/condition"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

const ArtifactConditionSync condition.ConditionType = "Sync"
const ArtifactConditionPublished condition.ConditionType = "Published"
const ArtifactConditionCompiled condition.ConditionType = "Compiled"

// ArtifactSpec defines the desired state of Artifact.
type ArtifactSpec struct {
	// +kubebuilder:validation:Required
	Image string `json:"image"`
	// +kubebuilder:validation:Optional
	ImagePullSecret *corev1.LocalObjectReference `json:"imagePullSecret,omitempty"`

	// CompileImage is the container image used to run the Wasmtime AOT compilation job.
	// Must have wasmtime and oras available. If empty, compilation is skipped.
	// +kubebuilder:validation:Optional
	CompileImage string `json:"compileImage,omitempty"`

	// TargetArchitectures to AOT-compile for. Supported: "amd64", "arm64". Defaults to both.
	// +kubebuilder:validation:Optional
	TargetArchitectures []string `json:"targetArchitectures,omitempty"`

	// CompiledImageRepository is the OCI repository to push compiled artifacts to.
	// Tags are generated as "{arch}-{generation}" (e.g. "amd64-3").
	// Required when CompileImage is set.
	// +kubebuilder:validation:Optional
	CompiledImageRepository string `json:"compiledImageRepository,omitempty"`

	// CompiledImagePushSecret contains push credentials for CompiledImageRepository.
	// +kubebuilder:validation:Optional
	CompiledImagePushSecret *corev1.LocalObjectReference `json:"compiledImagePushSecret,omitempty"`
}

// ArtifactStatus defines the observed state of Artifact.
type ArtifactStatus struct {
	condition.ConditionedStatus `json:",inline"`
	// +kubebuilder:validation:Optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
	// +kubebuilder:validation:Optional
	ArtifactURL string `json:"artifactUrl,omitempty"`
	// CompiledArtifacts maps architecture (e.g. "amd64") to the compiled OCI artifact URL.
	// +kubebuilder:validation:Optional
	CompiledArtifacts map[string]string `json:"compiledArtifacts,omitempty"`
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
