package runtime

import (
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// Shared by the precompile, artifact GC, resolve and deployment-watch tests.
const (
	testPrecompileTarget      = "x86_64-unknown-linux-gnu"
	testWasmtimeVersion       = "27.0.0"
	testArtifactName          = "comp"
	testArtifactImageV1       = "ghcr.io/x/y:v1"
	testArtifactImageV2       = "ghcr.io/x/y:v2"
	testCwasmKey              = "a/img/x86_64-27.0.0.cwasm"
	testWatchedDeploymentName = "echo"
)

var _ = Describe("PrecompileReconciler.outputURLOf", func() {
	r := &PrecompileReconciler{
		ArtifactStore:   ArtifactStoreConfig{BaseURL: "nats://store"},
		Target:          testPrecompileTarget,
		WasmtimeVersion: testWasmtimeVersion,
	}

	It("produces different URLs for the same artifact name when the image changes", func() {
		v1 := &runtimev1alpha1.Artifact{
			ObjectMeta: metav1.ObjectMeta{Name: testArtifactName, Namespace: metav1.NamespaceDefault},
			Spec:       runtimev1alpha1.ArtifactSpec{Image: testArtifactImageV1},
		}
		v2 := &runtimev1alpha1.Artifact{
			ObjectMeta: metav1.ObjectMeta{Name: testArtifactName, Namespace: metav1.NamespaceDefault},
			Spec:       runtimev1alpha1.ArtifactSpec{Image: testArtifactImageV2},
		}

		Expect(r.outputURLOf(v1)).NotTo(Equal(r.outputURLOf(v2)))
	})

	It("is deterministic for the same image", func() {
		a := &runtimev1alpha1.Artifact{
			ObjectMeta: metav1.ObjectMeta{Name: testArtifactName, Namespace: metav1.NamespaceDefault},
			Spec:       runtimev1alpha1.ArtifactSpec{Image: testArtifactImageV1},
		}
		Expect(r.outputURLOf(a)).To(Equal(r.outputURLOf(a)))
	})
})
