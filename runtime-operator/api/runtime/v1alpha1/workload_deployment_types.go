package v1alpha1

import (
	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

const (
	WorkloadDeploymentConditionArtifact condition.ConditionType = "Artifact"
	WorkloadDeploymentConditionSync     condition.ConditionType = "Sync"
	WorkloadDeploymentConditionDeploy   condition.ConditionType = "Deploy"
	WorkloadDeploymentConditionScale    condition.ConditionType = "Scale"
)

type WorkloadDeployPolicy string

const (
	WorkloadDeployPolicyRollingUpdate WorkloadDeployPolicy = "RollingUpdate"
	WorkloadDeployPolicyRecreate      WorkloadDeployPolicy = "Recreate"
)

// PrecompileMode controls how a WorkloadDeployment reacts to missing precompiled
// variants for the components it references.
//
// Fallback (default): the host compiles missing components inline via Cranelift
// on first use. Keeps deploys fast; trades off the resource-isolation guarantee.
//
// Strict: the WorkloadDeployment reconciler blocks on every referenced
// Artifact having a Precompiled variant for every target in HostPoolTargets
// before dispatching. Guarantees no Cranelift on workload hosts.
// +kubebuilder:validation:Enum=Fallback;Strict
type PrecompileMode string

const (
	PrecompileModeFallback PrecompileMode = "Fallback"
	PrecompileModeStrict   PrecompileMode = "Strict"
)

// WorkloadDeploymentArtifact references a component artifact to include in
// the deployment. Exactly one of ArtifactFrom or Image must be set:
//
//   - ArtifactFrom references an existing Artifact CR by name.
//   - Image inlines an OCI ref; the controller upserts an Artifact named
//     "auto-<sha256(image+pullSecret)[:12]>" in the same namespace.
//
// +kubebuilder:validation:XValidation:rule="(has(self.artifactFrom) ? 1 : 0) + (has(self.image) ? 1 : 0) == 1",message="exactly one of artifactFrom or image must be set"
type WorkloadDeploymentArtifact struct {
	// Local name used to refer to this artifact elsewhere in the workload.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Reference an existing Artifact CR in the same namespace.
	// +kubebuilder:validation:Optional
	ArtifactFrom *corev1.LocalObjectReference `json:"artifactFrom,omitempty"`

	// Inline OCI image reference. The controller will upsert an Artifact for this image.
	// +kubebuilder:validation:Optional
	Image string `json:"image,omitempty"`

	// Pull secret for the inline image form. Ignored when ArtifactFrom is set.
	// +kubebuilder:validation:Optional
	ImagePullSecret *corev1.LocalObjectReference `json:"imagePullSecret,omitempty"`
}

// WorkloadDeploymentSpec defines the desired state of WorkloadDeployment.
type WorkloadDeploymentSpec struct {
	WorkloadReplicaSetSpec `json:",inline"`

	// +kubebuilder:validation:Optional
	// +kubebuilder:default=RollingUpdate
	DeployPolicy WorkloadDeployPolicy `json:"deployPolicy,omitempty"`

	// +kubebuilder:validation:Optional
	Artifacts []WorkloadDeploymentArtifact `json:"artifacts,omitempty"`

	// PrecompileMode controls rollout behavior when precompiled variants are
	// missing. Defaults to Fallback.
	// +kubebuilder:validation:Optional
	// +kubebuilder:default=Fallback
	PrecompileMode PrecompileMode `json:"precompileMode,omitempty"`

	// HostPoolTargets is the set of target triples this deployment may run on.
	// Used in Strict mode to check that every referenced Artifact has a
	// precompiled variant for each target before dispatching. When empty, the
	// operator's default target matrix is used.
	// +kubebuilder:validation:Optional
	HostPoolTargets []string `json:"hostPoolTargets,omitempty"`

	// Kubernetes groups Kubernetes-specific configuration such as Service
	// references and endpoint management.
	// +kubebuilder:validation:Optional
	Kubernetes *KubernetesSpec `json:"kubernetes,omitempty"`
}

// WorkloadDeploymentStatus defines the observed state of WorkloadDeployment.
type WorkloadDeploymentStatus struct {
	condition.ConditionedStatus `json:",inline"`
	// +kubebuilder:validation:Optional
	CurrentReplicaSet *corev1.LocalObjectReference `json:"currentReplicaSet,omitempty"`
	// +kubebuilder:validation:Optional
	PreviousReplicaSet *corev1.LocalObjectReference `json:"previousReplicaSet,omitempty"`
	// +kubebuilder:validation:Optional
	Replicas *ReplicaSetStatus `json:"replicas,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:printcolumn:name="REPLICAS",type=integer,JSONPath=`.spec.replicas`
// +kubebuilder:printcolumn:name="READY",type=string,JSONPath=`.status.conditions[?(@.type=="Ready")].status`

// WorkloadDeployment is the Schema for the artifacts API.
type WorkloadDeployment struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   WorkloadDeploymentSpec   `json:"spec,omitempty"`
	Status WorkloadDeploymentStatus `json:"status,omitempty"`
}

// fulfill the ConditionedStatus interface
func (a *WorkloadDeployment) ConditionedStatus() *condition.ConditionedStatus {
	return &a.Status.ConditionedStatus
}

func (a *WorkloadDeployment) InitializeConditionedStatus() {
}

// +kubebuilder:object:root=true

// WorkloadDeploymentList contains a list of HttpTrigger.
type WorkloadDeploymentList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []WorkloadDeployment `json:"items"`
}

func init() {
	SchemeBuilder.Register(&WorkloadDeployment{}, &WorkloadDeploymentList{})
}
