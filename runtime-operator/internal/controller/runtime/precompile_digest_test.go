package runtime

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/opencontainers/go-digest"
)

const (
	layerA        digest.Digest = "sha256:97298013435ea4b12754673e7c8b53223d58c79b7fbb1107ce20d16f24452bd6"
	layerB        digest.Digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111"
	testProfile                 = "0123456789abcdef"
	testTokenUser               = "user"
	testTokenPass               = "pass"
)

func TestDigestKeyURL(t *testing.T) {
	got, err := DigestKeyURL("nats://precompiled-artifacts/", layerA, testTarget, testWasmtimeVersion, testProfile)
	if err != nil {
		t.Fatal(err)
	}
	want := "nats://precompiled-artifacts/sha256/" + layerA.Encoded() +
		"/" + testTarget + "-" + testWasmtimeVersion + "-" + testProfile + ".cwasm"
	if got != want {
		t.Fatalf("got %s want %s", got, want)
	}

	plain, _ := DigestKeyURL("nats://b", layerA, testTarget, testWasmtimeVersion, testProfile)
	for name, other := range map[string]func() (string, error){
		"wasmtime": func() (string, error) {
			return DigestKeyURL("nats://b", layerA, testTarget, "28.0.0", testProfile)
		},
		"target": func() (string, error) {
			return DigestKeyURL("nats://b", layerA, "aarch64-unknown-linux-gnu", testWasmtimeVersion, testProfile)
		},
		"profile": func() (string, error) {
			return DigestKeyURL("nats://b", layerA, testTarget, testWasmtimeVersion, "fedcba9876543210")
		},
		"layer": func() (string, error) {
			return DigestKeyURL("nats://b", layerB, testTarget, testWasmtimeVersion, testProfile)
		},
	} {
		u, err := other()
		if err != nil || u == plain {
			t.Errorf("%s must change the key: %s %v", name, u, err)
		}
	}

	for _, bad := range []digest.Digest{"", "sha256:abc", "sha512:" + digest.Digest(strings.Repeat("a", 128)),
		"sha256:" + digest.Digest(strings.Repeat("Z", 64))} {
		if _, err := DigestKeyURL("nats://b", bad, testTarget, testWasmtimeVersion, testProfile); err == nil {
			t.Errorf("digest %q: want error", bad)
		}
	}
	for _, bad := range []string{"", "epoch", "0123456789ABCDEF", "0123456789abcde"} {
		if _, err := DigestKeyURL("nats://b", layerA, testTarget, testWasmtimeVersion, bad); err == nil {
			t.Errorf("profile %q: want error", bad)
		}
	}
}

func TestIsCurrentContentURL(t *testing.T) {
	r := &PrecompileReconciler{
		ArtifactStore:    ArtifactStoreConfig{BaseURL: gcTestBaseURL},
		Target:           testTarget,
		WasmtimeVersion:  testWasmtimeVersion,
		ContentAddressed: &ContentAddressedConfig{EngineProfile: testProfile},
	}
	cur, _ := DigestKeyURL(gcTestBaseURL, layerA, testTarget, testWasmtimeVersion, testProfile)
	otherProfile, _ := DigestKeyURL(gcTestBaseURL, layerA, testTarget, testWasmtimeVersion, "fedcba9876543210")
	legacy := gcTestBaseURL + "/comp/ghcr.io_x_y_v1/" + testTarget + "-" + testWasmtimeVersion + ".cwasm"
	if !r.isCurrentContentURL(cur) {
		t.Errorf("%s must be current", cur)
	}
	for _, u := range []string{otherProfile, legacy, "nats://other/sha256/" + layerA.Encoded() + "/x.cwasm"} {
		if r.isCurrentContentURL(u) {
			t.Errorf("%s must not be current", u)
		}
	}
}

func TestPinnedImageRef(t *testing.T) {
	md := digest.Digest("sha256:" + strings.Repeat("a", 64))
	cases := map[string]string{
		"wasmcloud-registry:5000/job-overhead/n0001:g1": "wasmcloud-registry:5000/job-overhead/n0001@" + md.String(),
		"ghcr.io/x/y:v1":                    "ghcr.io/x/y@" + md.String(),
		"ghcr.io/x/y@" + layerB.String():    "ghcr.io/x/y@" + md.String(),
		"ghcr.io/x/y:v1@" + layerB.String(): "ghcr.io/x/y@" + md.String(),
		"registry.local/a/b/c":              "registry.local/a/b/c@" + md.String(),
		"library/ubuntu:latest":             "ubuntu@" + md.String(),
	}
	for in, want := range cases {
		got, err := PinnedImageRef(in, md)
		if err != nil || got != want {
			t.Errorf("PinnedImageRef(%q) = %q, %v; want %q", in, got, err, want)
		}
	}
	if _, err := PinnedImageRef("ghcr.io/x/Y:v1", md); err == nil {
		t.Error("want error for invalid reference")
	}
}

type fakeRegistryOpts struct {
	// auth is "", "basic" or "bearer".
	auth string
	// lieAboutDigest sends a wrong Docker-Content-Digest header.
	lieAboutDigest bool
	// layers overrides the layer list of every manifest.
	layers []map[string]any
}

// fakeRegistry serves Wasm manifests like zot. Every GET writes a fresh
// `created` annotation, like `wash oci push` does on each push, so the
// manifest digest changes while the layer digest does not.
func fakeRegistry(t *testing.T, tags map[string]digest.Digest, o fakeRegistryOpts) (string, *atomic.Int32) {
	t.Helper()
	var calls atomic.Int32
	var srv *httptest.Server
	srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/token" {
			if u, p, ok := r.BasicAuth(); !ok || u != testTokenUser || p != testTokenPass {
				w.WriteHeader(http.StatusUnauthorized)
				return
			}
			if r.URL.Query().Get("scope") != "repository:x:pull,push" || r.URL.Query().Get("service") != "reg" {
				w.WriteHeader(http.StatusBadRequest)
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]string{"access_token": "tok"})
			return
		}
		calls.Add(1)
		switch o.auth {
		case "bearer":
			if r.Header.Get("Authorization") != "Bearer tok" {
				w.Header().Set("WWW-Authenticate",
					fmt.Sprintf(`Bearer realm="%s/token",service="reg",scope="repository:x:pull,push"`, srv.URL))
				w.WriteHeader(http.StatusUnauthorized)
				return
			}
		case "basic":
			if u, p, ok := r.BasicAuth(); !ok || u != testTokenUser || p != testTokenPass {
				w.Header().Set("WWW-Authenticate", `Basic realm="zot"`)
				w.WriteHeader(http.StatusUnauthorized)
				return
			}
		}
		rest := strings.TrimPrefix(r.URL.Path, "/v2/")
		i := strings.LastIndex(rest, "/manifests/")
		if i < 0 {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		layer, ok := tags[rest[:i]+":"+rest[i+len("/manifests/"):]]
		if !ok {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		layers := o.layers
		if layers == nil {
			layers = []map[string]any{{"mediaType": wasmLayerMediaType, "digest": layer, "size": 54000}}
		}
		body, _ := json.Marshal(map[string]any{
			"schemaVersion": 2,
			"mediaType":     ociManifestMediaType,
			"annotations":   map[string]string{"org.opencontainers.image.created": time.Now().Format(time.RFC3339Nano)},
			"config":        map[string]any{"mediaType": "application/vnd.wasm.config.v0+json", "digest": layerB, "size": 1},
			"layers":        layers,
		})
		hdr := digest.FromBytes(body).String()
		if o.lieAboutDigest {
			hdr = layerB.String()
		}
		w.Header().Set("Content-Type", ociManifestMediaType)
		w.Header().Set("Docker-Content-Digest", hdr)
		_, _ = w.Write(body)
	}))
	t.Cleanup(srv.Close)
	return strings.TrimPrefix(srv.URL, "http://"), &calls
}

func TestHTTPDigestResolver_SameLayerSameKeyAcrossPushes(t *testing.T) {
	host, _ := fakeRegistry(t, map[string]digest.Digest{
		"app/n0001:g1": layerA,
		"app/n0001:g2": layerA, // rebuild, identical .wasm
		"app/n0002:g1": layerB,
	}, fakeRegistryOpts{})
	res := NewHTTPDigestResolver(" other:1, " + host + " ")
	ctx := context.Background()

	r1, err := res.Resolve(ctx, host+"/app/n0001:g1", nil)
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(time.Millisecond)
	r2, err := res.Resolve(ctx, host+"/app/n0001:g2", nil)
	if err != nil {
		t.Fatal(err)
	}
	r3, err := res.Resolve(ctx, host+"/app/n0002:g1", nil)
	if err != nil {
		t.Fatal(err)
	}
	if r1.LayerDigest != layerA || r2.LayerDigest != layerA || r3.LayerDigest != layerB {
		t.Fatalf("layers: %+v %+v %+v", r1, r2, r3)
	}
	if r1.ManifestDigest == r2.ManifestDigest {
		t.Fatal("test setup: manifest digests should differ (created annotation)")
	}
	k1, _ := DigestKeyURL(gcTestBaseURL, r1.LayerDigest, testTarget, testWasmtimeVersion, testProfile)
	k2, _ := DigestKeyURL(gcTestBaseURL, r2.LayerDigest, testTarget, testWasmtimeVersion, testProfile)
	if k1 != k2 {
		t.Fatalf("same bytes, different keys: %s %s", k1, k2)
	}
	if _, err := res.Resolve(ctx, host+"/app/missing:g1", nil); err == nil {
		t.Fatal("want error for missing tag")
	}
}

func TestHTTPDigestResolver_PinnedRefMustMatch(t *testing.T) {
	host, _ := fakeRegistry(t, map[string]digest.Digest{"x:" + layerB.String(): layerA}, fakeRegistryOpts{})
	res := NewHTTPDigestResolver(host)
	// The fake serves a new manifest each time, so it never hashes to the
	// digest in the reference.
	if _, err := res.Resolve(context.Background(), host+"/x@"+layerB.String(), nil); err == nil ||
		!strings.Contains(err.Error(), "hashes to") {
		t.Fatalf("want digest mismatch error, got %v", err)
	}
}

func TestHTTPDigestResolver_Auth(t *testing.T) {
	creds := &RegistryCredentials{Username: testTokenUser, Password: testTokenPass}
	for _, mode := range []string{"bearer", "basic"} {
		t.Run(mode, func(t *testing.T) {
			host, calls := fakeRegistry(t, map[string]digest.Digest{"x:v1": layerA}, fakeRegistryOpts{auth: mode})
			res := NewHTTPDigestResolver(host)
			got, err := res.Resolve(context.Background(), host+"/x:v1", creds)
			if err != nil {
				t.Fatal(err)
			}
			if got.LayerDigest != layerA {
				t.Fatalf("got %+v", got)
			}
			if calls.Load() != 2 {
				t.Fatalf("manifest GETs = %d, want 2 (challenge + authorized)", calls.Load())
			}
			if _, err := res.Resolve(context.Background(), host+"/x:v1",
				&RegistryCredentials{Username: testTokenUser, Password: "wrong"}); err == nil {
				t.Fatal("want error with wrong credentials")
			}
		})
	}
	host, _ := fakeRegistry(t, map[string]digest.Digest{"x:v1": layerA}, fakeRegistryOpts{auth: "basic"})
	if _, err := NewHTTPDigestResolver(host).Resolve(context.Background(), host+"/x:v1", nil); err == nil ||
		!strings.Contains(err.Error(), "imagePullSecret") {
		t.Fatalf("want a clear error without credentials, got %v", err)
	}
}

func TestHTTPDigestResolver_RejectsBadManifests(t *testing.T) {
	tags := map[string]digest.Digest{"x:v1": layerA}
	cases := map[string]fakeRegistryOpts{
		"digest header lies": {lieAboutDigest: true},
		"no wasm layer":      {layers: []map[string]any{{"mediaType": "application/octet-stream", "digest": layerA}}},
		"two wasm layers": {layers: []map[string]any{
			{"mediaType": wasmLayerMediaType, "digest": layerA},
			{"mediaType": legacyWasmLayerMediaType, "digest": layerB},
		}},
		"invalid layer digest": {layers: []map[string]any{{"mediaType": wasmLayerMediaType, "digest": "sha256:zz"}}},
	}
	for name, o := range cases {
		t.Run(name, func(t *testing.T) {
			host, _ := fakeRegistry(t, tags, o)
			if _, err := NewHTTPDigestResolver(host).Resolve(context.Background(), host+"/x:v1", nil); err == nil {
				t.Fatal("want error")
			}
		})
	}
	host, _ := fakeRegistry(t, tags, fakeRegistryOpts{
		layers: []map[string]any{{"mediaType": legacyWasmLayerMediaType, "digest": layerA}},
	})
	got, err := NewHTTPDigestResolver(host).Resolve(context.Background(), host+"/x:v1", nil)
	if err != nil || got.LayerDigest != layerA {
		t.Fatalf("legacy media type: %+v %v", got, err)
	}
}

func TestParseChallengeParams(t *testing.T) {
	got := parseChallengeParams(`realm="https://r/token",service="reg", scope="repository:a/b:pull,push",x=y`)
	want := map[string]string{
		"realm": "https://r/token", "service": "reg", "scope": "repository:a/b:pull,push", "x": "y",
	}
	if len(got) != len(want) {
		t.Fatalf("got %v", got)
	}
	for k, v := range want {
		if got[k] != v {
			t.Errorf("%s = %q, want %q", k, got[k], v)
		}
	}
}

func metaSubjectOfKey(key string) string {
	return metaSubject(gcTestBucket, base64.URLEncoding.EncodeToString([]byte(key)))
}

func TestNatsObjectStore_Claim(t *testing.T) {
	nc := startEmbeddedNats(t)
	st := &NatsObjectStore{Conn: nc}
	ctx := context.Background()
	const key = "sha256/abc/x.cwasm"
	url := gcTestBaseURL + "/" + key

	// Missing bucket: a miss, not an error.
	if ok, err := st.Claim(ctx, url); err != nil || ok {
		t.Fatalf("missing bucket: %v %v", ok, err)
	}
	js, _ := nc.JetStream()
	store, err := js.CreateObjectStore(&nats.ObjectStoreConfig{Bucket: gcTestBucket})
	if err != nil {
		t.Fatal(err)
	}
	if ok, err := st.Claim(ctx, url); err != nil || ok {
		t.Fatalf("missing key: %v %v", ok, err)
	}
	if _, err := store.PutBytes(key, []byte("cwasm")); err != nil {
		t.Fatal(err)
	}

	stream := objectStreamName(gcTestBucket)
	before, seqBefore, err := readObjectInfo(js, stream, metaSubjectOfKey(key))
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(20 * time.Millisecond)
	if ok, err := st.Claim(ctx, url); err != nil || !ok {
		t.Fatalf("present key: %v %v", ok, err)
	}
	after, seqAfter, _ := readObjectInfo(js, stream, metaSubjectOfKey(key))
	if !after.ModTime.After(before.ModTime) || seqAfter <= seqBefore {
		t.Fatalf("Claim did not touch the object: %v/%d -> %v/%d",
			before.ModTime, seqBefore, after.ModTime, seqAfter)
	}
	if b, err := store.GetBytes(key); err != nil || string(b) != "cwasm" {
		t.Fatalf("data changed after Claim: %q %v", b, err)
	}

	if err := store.Delete(key); err != nil {
		t.Fatal(err)
	}
	if ok, err := st.Claim(ctx, url); err != nil || ok {
		t.Fatalf("deleted object must be a miss: %v %v", ok, err)
	}

	for _, bad := range []string{"file:///x.cwasm", "nats://b/", "nats:///k"} {
		if _, err := st.Claim(ctx, bad); err == nil {
			t.Errorf("%s: want error", bad)
		}
	}
}

// The claim and the GC's delete are both conditional on the metadata
// sequence. Whichever lands second loses.
func TestClaimAndConditionalDeleteExcludeEachOther(t *testing.T) {
	nc := startEmbeddedNats(t)
	js, _ := nc.JetStream()
	store, err := js.CreateObjectStore(&nats.ObjectStoreConfig{Bucket: gcTestBucket})
	if err != nil {
		t.Fatal(err)
	}
	const key = "sha256/abc/x.cwasm"
	if _, err := store.PutBytes(key, []byte("cwasm")); err != nil {
		t.Fatal(err)
	}
	stream := objectStreamName(gcTestBucket)
	_, seq, err := readObjectInfo(js, stream, metaSubjectOfKey(key))
	if err != nil {
		t.Fatal(err)
	}

	// The GC read seq; then the operator claims the object.
	if ok, err := (&NatsObjectStore{Conn: nc}).Claim(context.Background(), gcTestBaseURL+"/"+key); err != nil || !ok {
		t.Fatalf("claim: %v %v", ok, err)
	}
	// The GC's delete for the old read must not land.
	deleted, err := deleteIfUnchanged(js, gcTestBucket, cwasmObject{Key: key, Seq: seq})
	if err != nil || deleted {
		t.Fatalf("stale delete landed: %v %v", deleted, err)
	}
	if b, err := store.GetBytes(key); err != nil || string(b) != "cwasm" {
		t.Fatalf("object lost: %q %v", b, err)
	}

	// The other order: the GC deletes first, then a claim from an older read
	// must not resurrect the metadata.
	info, seq, _ := readObjectInfo(js, stream, metaSubjectOfKey(key))
	if deleted, err := deleteIfUnchanged(js, gcTestBucket, cwasmObject{Key: key, Seq: seq}); err != nil || !deleted {
		t.Fatalf("delete: %v %v", deleted, err)
	}
	data, _ := json.Marshal(info)
	if err := publishMetaIfUnchanged(js, gcTestBucket, metaSubjectOfKey(key), data, seq); !isWrongLastSequence(err) {
		t.Fatalf("stale claim must fail with wrong last sequence, got %v", err)
	}
	if ok, _ := (&NatsObjectStore{Conn: nc}).Claim(context.Background(), gcTestBaseURL+"/"+key); ok {
		t.Fatal("deleted object claimed")
	}
	if _, err := store.GetBytes(key); err == nil {
		t.Fatal("deleted object still readable")
	}
}
