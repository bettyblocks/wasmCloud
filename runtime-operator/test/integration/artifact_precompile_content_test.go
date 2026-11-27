package integration

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"time"

	"github.com/nats-io/nats.go"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/go-digest"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/utils/ptr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/cache"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/config"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
	runtimectrl "go.wasmcloud.dev/runtime-operator/v2/internal/controller/runtime"
	"go.wasmcloud.dev/runtime-operator/v2/pkg/wasmbus"
)

const (
	contentNamespace = "precompile-content"
	contentBucket    = "precompiled-content"
	contentProfile   = "0123456789abcdef"
	// sha256 of the .wasm that every repo below serves.
	sharedLayer digest.Digest = "sha256:97298013435ea4b12754673e7c8b53223d58c79b7fbb1107ce20d16f24452bd6"
)

// wasmRegistry serves a Wasm manifest with sharedLayer for every repo and
// tag. Like `wash oci push`, every manifest carries a fresh `created`
// annotation, so the manifest digests differ while the layer is the same.
func wasmRegistry() string {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.Contains(r.URL.Path, "/manifests/") {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		body, _ := json.Marshal(map[string]any{
			"schemaVersion": 2,
			"mediaType":     "application/vnd.oci.image.manifest.v1+json",
			"annotations":   map[string]string{"org.opencontainers.image.created": time.Now().Format(time.RFC3339Nano)},
			"config":        map[string]any{"mediaType": "application/vnd.wasm.config.v0+json", "digest": sharedLayer, "size": 1},
			"layers":        []map[string]any{{"mediaType": "application/wasm", "digest": sharedLayer, "size": 54000}},
		})
		w.Header().Set("Docker-Content-Digest", digest.FromBytes(body).String())
		_, _ = w.Write(body)
	}))
	DeferCleanup(srv.Close)
	return strings.TrimPrefix(srv.URL, "http://")
}

func embeddedNats() *nats.Conn {
	GinkgoHelper()
	opts := wasmbus.NatsDefaultServerOptions()
	opts.Port = -1
	opts.StoreDir = GinkgoT().TempDir()
	s, err := wasmbus.NatsEmbeddedServer(opts, 5*time.Second)
	if err != nil {
		Skip("embedded NATS unavailable: " + err.Error())
	}
	DeferCleanup(s.Shutdown)
	nc, err := nats.Connect(s.ClientURL())
	Expect(err).NotTo(HaveOccurred())
	DeferCleanup(nc.Close)
	return nc
}

var _ = Describe("content-addressed precompile", Ordered, func() {
	var (
		ctx      context.Context
		registry string
		store    nats.ObjectStore
		baseURL  = "nats://" + contentBucket
		wantKey  string
	)

	BeforeAll(func() {
		ctx = context.Background()
		registry = wasmRegistry()
		nc := embeddedNats()
		js, err := nc.JetStream()
		Expect(err).NotTo(HaveOccurred())
		store, err = js.CreateObjectStore(&nats.ObjectStoreConfig{Bucket: contentBucket})
		Expect(err).NotTo(HaveOccurred())

		wantKey, err = runtimectrl.DigestKeyURL(baseURL, sharedLayer, testTarget, testWasmtimeVersion, contentProfile)
		Expect(err).NotTo(HaveOccurred())

		Expect(k8sClient.Create(ctx, &corev1.Namespace{
			ObjectMeta: metav1.ObjectMeta{Name: contentNamespace},
		})).To(Succeed())

		// A second manager, scoped to its own namespace, runs the
		// precompile controller in content-addressed mode with the real
		// registry resolver and the real NATS object store.
		mgr, err := ctrl.NewManager(restConfig, ctrl.Options{
			Scheme:     k8sClient.Scheme(),
			Metrics:    metricsserver.Options{BindAddress: "0"},
			Cache:      cache.Options{DefaultNamespaces: map[string]cache.Config{contentNamespace: {}}},
			Controller: config.Controller{SkipNameValidation: ptr.To(true)},
		})
		Expect(err).NotTo(HaveOccurred())
		Expect((&runtimectrl.PrecompileReconciler{
			Client:          mgr.GetClient(),
			Scheme:          mgr.GetScheme(),
			WorkerImage:     testWorkerImage,
			ArtifactStore:   runtimectrl.ArtifactStoreConfig{BaseURL: baseURL},
			Target:          testTarget,
			WasmtimeVersion: testWasmtimeVersion,
			ContentAddressed: &runtimectrl.ContentAddressedConfig{
				Resolver:      runtimectrl.NewHTTPDigestResolver(registry),
				Store:         &runtimectrl.NatsObjectStore{Conn: nc},
				EngineProfile: contentProfile,
				JobReader:     mgr.GetAPIReader(),
			},
		}).SetupWithManager(mgr)).To(Succeed())

		mgrCtx, cancel := context.WithCancel(context.Background())
		DeferCleanup(cancel)
		go func() {
			defer GinkgoRecover()
			Expect(mgr.Start(mgrCtx)).To(Succeed())
		}()
	})

	newContentArtifact := func(name, image string) {
		GinkgoHelper()
		Expect(k8sClient.Create(ctx, &runtimev1alpha1.Artifact{
			ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: contentNamespace},
			Spec:       runtimev1alpha1.ArtifactSpec{Image: image},
		})).To(Succeed())
	}

	jobs := func(g Gomega) []batchv1.Job {
		var list batchv1.JobList
		g.Expect(k8sClient.List(ctx, &list, client.InNamespace(contentNamespace))).To(Succeed())
		return list.Items
	}

	expectRecorded := func(g Gomega, name string) *runtimev1alpha1.Artifact {
		var a runtimev1alpha1.Artifact
		g.Expect(k8sClient.Get(ctx, types.NamespacedName{Namespace: contentNamespace, Name: name}, &a)).To(Succeed())
		g.Expect(a.Status.Precompiled).To(HaveLen(1))
		g.Expect(a.Status.Precompiled[0].ArtifactURL).To(Equal(wantKey))
		g.Expect(a.Status.Precompiled[0].ImageRef).To(Equal(a.Spec.Image))
		g.Expect(a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompiled).Status).
			To(Equal(corev1.ConditionTrue))
		return &a
	}

	It("runs one Job for two Artifacts with the same bytes, and both record one object", func() {
		newContentArtifact("app1-data-api", registry+"/app1/data-api:3.2.0")
		newContentArtifact("app2-data-api", registry+"/app2/data-api:3.2.0")

		var job batchv1.Job
		Eventually(func(g Gomega) {
			items := jobs(g)
			g.Expect(items).To(HaveLen(1))
			job = items[0]
			// Both Artifacts were reconciled: one runs the Job, the other waits.
			for _, n := range []string{"app1-data-api", "app2-data-api"} {
				var a runtimev1alpha1.Artifact
				g.Expect(k8sClient.Get(ctx, types.NamespacedName{Namespace: contentNamespace, Name: n}, &a)).To(Succeed())
				g.Expect(a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing).Status).
					To(Equal(corev1.ConditionTrue))
			}
		}).Should(Succeed())
		Consistently(func(g Gomega) { g.Expect(jobs(g)).To(HaveLen(1)) }, "300ms").Should(Succeed())

		args := job.Spec.Template.Spec.Containers[0].Args
		Expect(args).To(HaveLen(4))
		Expect(args[1]).To(MatchRegexp(`^` + registry + `/app[12]/data-api@sha256:[0-9a-f]{64}$`))
		Expect(args[3]).To(Equal(wantKey))

		// Play the worker: write the object, then complete the Job.
		key := strings.TrimPrefix(wantKey, baseURL+"/")
		_, err := store.PutBytes(key, []byte("cwasm"))
		Expect(err).NotTo(HaveOccurred())
		job.Status.Succeeded = 1
		job.Status.Conditions = []batchv1.JobCondition{{Type: batchv1.JobComplete, Status: corev1.ConditionTrue}}
		Expect(k8sClient.Status().Update(ctx, &job)).To(Succeed())

		Eventually(func(g Gomega) {
			expectRecorded(g, "app1-data-api")
			expectRecorded(g, "app2-data-api")
		}).Should(Succeed())
		Expect(objectKeys(store)).To(ConsistOf(key))
	})

	It("skips the Job for a new Artifact whose bytes are already compiled", func() {
		newContentArtifact("app1-data-api-retag", registry+"/app1/data-api:3.2.1")

		Eventually(func(g Gomega) {
			a := expectRecorded(g, "app1-data-api-retag")
			g.Expect(a.Status.GetCondition(runtimev1alpha1.ArtifactConditionPrecompileProgressing).Reason).
				To(BeEquivalentTo("CacheHit"))
		}).Should(Succeed())
		Consistently(func(g Gomega) {
			for _, j := range jobs(g) {
				g.Expect(j.Name).NotTo(Equal("precompile-app1-data-api-retag"))
			}
		}, "300ms").Should(Succeed())
	})
})

func objectKeys(store nats.ObjectStore) []string {
	GinkgoHelper()
	infos, err := store.List()
	Expect(err).NotTo(HaveOccurred())
	var keys []string
	for _, i := range infos {
		if !i.Deleted {
			keys = append(keys, i.Name)
		}
	}
	return keys
}
