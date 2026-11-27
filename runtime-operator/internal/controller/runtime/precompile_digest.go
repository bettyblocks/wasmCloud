package runtime

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"
	"strings"
	"time"

	"github.com/distribution/reference"
	"github.com/nats-io/nats.go"
	"github.com/opencontainers/go-digest"
)

// Content-addressed precompile keys.
//
// A .cwasm depends only on the bytes of the .wasm and on the compiler: the
// target, the wasmtime version and the engine settings of the worker. With
// content-addressed keys the operator stores it at
//
//	<base>/sha256/<layer hex>/<target>-<wasmtime>-<engine profile>.cwasm
//
// The layer digest is the sha256 of the .wasm itself: a Wasm OCI artifact has
// one layer that holds the raw component. The manifest digest is not used:
// `wash oci push` writes a `created` annotation into the manifest, so two
// pushes of the same .wasm get different manifest digests.
//
// The engine profile is what `wash-precompile --print-engine-profile` prints.
// The worker refuses an output key with another profile, or whose digest does
// not match the bytes it pulled, so an object under such a key always holds
// the right code for that key.

// ResolvedImage is an image reference resolved to digests.
type ResolvedImage struct {
	// ManifestDigest is the digest of the manifest the operator read. The Job
	// pulls repo@ManifestDigest, so a tag that moves cannot change the bytes
	// between the resolve and the pull.
	ManifestDigest digest.Digest
	// LayerDigest is the digest of the Wasm layer, i.e. of the .wasm.
	LayerDigest digest.Digest
}

// RegistryCredentials are the username and password for one registry.
type RegistryCredentials struct {
	Username string
	Password string
}

// DigestResolver resolves an image reference to digests without pulling the
// layer.
type DigestResolver interface {
	Resolve(ctx context.Context, image string, creds *RegistryCredentials) (ResolvedImage, error)
}

// PrecompiledObjectStore is the operator's view of the store the worker writes
// to.
type PrecompiledObjectStore interface {
	// Claim reports whether a complete, live object exists at url. When it
	// does, Claim also restarts the object's GC grace period. Claim and the
	// GC's delete are both conditional on the object's last metadata
	// sequence, so exactly one of them wins: either Claim returns true and
	// the GC keeps the object, or the GC deletes it and Claim returns false.
	Claim(ctx context.Context, url string) (bool, error)
}

const (
	wasmLayerMediaType       = "application/wasm"
	legacyWasmLayerMediaType = "application/vnd.module.wasm.content.layer.v1+wasm"
	ociManifestMediaType     = "application/vnd.oci.image.manifest.v1+json"
	dockerManifestMediaType  = "application/vnd.docker.distribution.manifest.v2+json"

	// maxManifestBytes bounds the manifest read. Wasm manifests are ~1 KiB.
	maxManifestBytes = 4 << 20
	// defaultResolveTimeout bounds one Resolve, including the token request.
	defaultResolveTimeout = 10 * time.Second
)

// engineProfilePattern is the shape `wash-precompile --print-engine-profile`
// prints: 16 lower-case hex characters.
var engineProfilePattern = regexp.MustCompile(`^[0-9a-f]{16}$`)

// ValidateEngineProfile checks that profile has the shape the worker prints.
func ValidateEngineProfile(profile string) error {
	if !engineProfilePattern.MatchString(profile) {
		return fmt.Errorf("engine profile %q must be 16 lower-case hex characters "+
			"(the output of `wash-precompile --print-engine-profile`)", profile)
	}
	return nil
}

// contentKeySuffix is the file name of every content-addressed key for one
// operator configuration.
func contentKeySuffix(target, wasmtimeVersion, engineProfile string) string {
	return "/" + target + "-" + wasmtimeVersion + "-" + engineProfile + ".cwasm"
}

// contentKeyPrefix is the part of every content-addressed URL before the hex.
func contentKeyPrefix(baseURL string) string {
	return strings.TrimSuffix(baseURL, "/") + "/sha256/"
}

// DigestKeyURL returns the content-addressed output URL of a .wasm.
func DigestKeyURL(baseURL string, layer digest.Digest, target, wasmtimeVersion, engineProfile string) (string, error) {
	if err := layer.Validate(); err != nil {
		return "", fmt.Errorf("invalid layer digest %q: %w", layer, err)
	}
	if layer.Algorithm() != digest.SHA256 {
		return "", fmt.Errorf("layer digest %q: only sha256 is supported", layer)
	}
	if err := ValidateEngineProfile(engineProfile); err != nil {
		return "", err
	}
	return contentKeyPrefix(baseURL) + layer.Encoded() + contentKeySuffix(target, wasmtimeVersion, engineProfile), nil
}

// PinnedImageRef returns image with its tag (or digest) replaced by
// manifestDigest.
func PinnedImageRef(image string, manifestDigest digest.Digest) (string, error) {
	named, err := reference.ParseNormalizedNamed(image)
	if err != nil {
		return "", fmt.Errorf("invalid image reference %q: %w", image, err)
	}
	pinned, err := reference.WithDigest(reference.TrimNamed(named), manifestDigest)
	if err != nil {
		return "", err
	}
	return reference.FamiliarString(pinned), nil
}

// HTTPDigestResolver resolves digests with one GET of the manifest on the OCI
// distribution API. A registry that answers 401 gets one more round trip:
// Basic auth, or a token request for Bearer auth.
type HTTPDigestResolver struct {
	// Client is reused across calls, so connections are kept alive.
	Client *http.Client
	// InsecureRegistries are registries (host[:port], as in the image
	// reference) that are reached over plain HTTP. This mirrors the
	// worker's INSECURE_REGISTRIES.
	InsecureRegistries map[string]bool
}

// NewHTTPDigestResolver returns a resolver that reaches the comma-separated
// insecureRegistries over plain HTTP.
func NewHTTPDigestResolver(insecureRegistries string) *HTTPDigestResolver {
	insecure := map[string]bool{}
	for _, r := range strings.Split(insecureRegistries, ",") {
		if r = strings.TrimSpace(r); r != "" {
			insecure[r] = true
		}
	}
	return &HTTPDigestResolver{
		Client:             &http.Client{Timeout: defaultResolveTimeout},
		InsecureRegistries: insecure,
	}
}

func (h *HTTPDigestResolver) Resolve(ctx context.Context, image string, creds *RegistryCredentials) (ResolvedImage, error) {
	named, err := reference.ParseNormalizedNamed(image)
	if err != nil {
		return ResolvedImage{}, fmt.Errorf("invalid image reference %q: %w", image, err)
	}
	host := reference.Domain(named)
	apiHost := host
	if host == "docker.io" {
		apiHost = "registry-1.docker.io"
	}
	ref := "latest"
	var wantManifest digest.Digest
	if d, ok := named.(reference.Digested); ok {
		wantManifest = d.Digest()
		ref = wantManifest.String()
	} else if t, ok := named.(reference.Tagged); ok {
		ref = t.Tag()
	}
	scheme := "https"
	if h.InsecureRegistries[host] {
		scheme = "http"
	}
	repo := reference.Path(named)
	manifestURL := fmt.Sprintf("%s://%s/v2/%s/manifests/%s", scheme, apiHost, repo, ref)

	client := h.Client
	if client == nil {
		client = &http.Client{Timeout: defaultResolveTimeout}
	}

	resp, err := getManifest(ctx, client, manifestURL, "")
	if err != nil {
		return ResolvedImage{}, err
	}
	if resp.StatusCode == http.StatusUnauthorized {
		challenge := resp.Header.Get("WWW-Authenticate")
		drainAndClose(resp)
		authz, err := authorizationFor(ctx, client, challenge, creds, repo)
		if err != nil {
			return ResolvedImage{}, fmt.Errorf("authenticating to %s: %w", host, err)
		}
		if resp, err = getManifest(ctx, client, manifestURL, authz); err != nil {
			return ResolvedImage{}, err
		}
	}
	defer drainAndClose(resp)
	if resp.StatusCode != http.StatusOK {
		return ResolvedImage{}, fmt.Errorf("GET %s: %s", manifestURL, resp.Status)
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, maxManifestBytes+1))
	if err != nil {
		return ResolvedImage{}, fmt.Errorf("reading manifest of %s: %w", image, err)
	}
	if len(body) > maxManifestBytes {
		return ResolvedImage{}, fmt.Errorf("manifest of %s is larger than %d bytes", image, maxManifestBytes)
	}

	// Pin the bytes we parsed, not what a header claims.
	manifestDigest := digest.FromBytes(body)
	if hdr := resp.Header.Get("Docker-Content-Digest"); hdr != "" && hdr != manifestDigest.String() {
		return ResolvedImage{}, fmt.Errorf("manifest of %s: registry says digest %s, body hashes to %s",
			image, hdr, manifestDigest)
	}
	if wantManifest != "" && wantManifest != manifestDigest {
		return ResolvedImage{}, fmt.Errorf("manifest of %s hashes to %s", image, manifestDigest)
	}

	layer, err := wasmLayerDigest(body)
	if err != nil {
		return ResolvedImage{}, fmt.Errorf("manifest of %s: %w", image, err)
	}
	return ResolvedImage{ManifestDigest: manifestDigest, LayerDigest: layer}, nil
}

// wasmLayerDigest returns the digest of the single Wasm layer of a manifest.
func wasmLayerDigest(manifest []byte) (digest.Digest, error) {
	var m struct {
		Layers []struct {
			MediaType string        `json:"mediaType"`
			Digest    digest.Digest `json:"digest"`
		} `json:"layers"`
	}
	if err := json.Unmarshal(manifest, &m); err != nil {
		return "", fmt.Errorf("decoding manifest: %w", err)
	}
	var found []digest.Digest
	for _, l := range m.Layers {
		if l.MediaType == wasmLayerMediaType || l.MediaType == legacyWasmLayerMediaType {
			found = append(found, l.Digest)
		}
	}
	switch len(found) {
	case 0:
		return "", errors.New("no Wasm layer")
	case 1:
		if err := found[0].Validate(); err != nil {
			return "", fmt.Errorf("invalid Wasm layer digest: %w", err)
		}
		return found[0], nil
	default:
		return "", fmt.Errorf("%d Wasm layers, want 1", len(found))
	}
}

func getManifest(ctx context.Context, client *http.Client, u, authz string) (*http.Response, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, u, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", ociManifestMediaType+", "+dockerManifestMediaType)
	if authz != "" {
		req.Header.Set("Authorization", authz)
	}
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("GET %s: %w", u, err)
	}
	return resp, nil
}

func drainAndClose(resp *http.Response) {
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 64<<10))
	_ = resp.Body.Close()
}

// authorizationFor answers a WWW-Authenticate challenge with an Authorization
// header value.
func authorizationFor(
	ctx context.Context, client *http.Client, challenge string, creds *RegistryCredentials, repo string,
) (string, error) {
	scheme, params, _ := strings.Cut(strings.TrimSpace(challenge), " ")
	switch strings.ToLower(scheme) {
	case "basic":
		if creds == nil {
			return "", errors.New("registry wants Basic auth, and the Artifact has no imagePullSecret for it")
		}
		return "Basic " + base64.StdEncoding.EncodeToString([]byte(creds.Username+":"+creds.Password)), nil
	case "bearer":
		return bearerToken(ctx, client, parseChallengeParams(params), creds, repo)
	default:
		return "", fmt.Errorf("unsupported auth challenge %q", challenge)
	}
}

func bearerToken(
	ctx context.Context, client *http.Client, p map[string]string, creds *RegistryCredentials, repo string,
) (string, error) {
	realm, err := url.Parse(p["realm"])
	if err != nil || realm.Host == "" || (realm.Scheme != "https" && realm.Scheme != "http") {
		return "", fmt.Errorf("invalid token realm %q", p["realm"])
	}
	q := realm.Query()
	if p["service"] != "" {
		q.Set("service", p["service"])
	}
	scope := p["scope"]
	if scope == "" {
		scope = "repository:" + repo + ":pull"
	}
	q.Set("scope", scope)
	realm.RawQuery = q.Encode()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, realm.String(), nil)
	if err != nil {
		return "", err
	}
	if creds != nil {
		req.SetBasicAuth(creds.Username, creds.Password)
	}
	resp, err := client.Do(req)
	if err != nil {
		return "", fmt.Errorf("token request: %w", err)
	}
	defer drainAndClose(resp)
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("token request: %s", resp.Status)
	}
	var tok struct {
		Token       string `json:"token"`
		AccessToken string `json:"access_token"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, maxManifestBytes)).Decode(&tok); err != nil {
		return "", fmt.Errorf("decoding token response: %w", err)
	}
	if tok.Token == "" {
		tok.Token = tok.AccessToken
	}
	if tok.Token == "" {
		return "", errors.New("token response has no token")
	}
	return "Bearer " + tok.Token, nil
}

// parseChallengeParams parses `k="v",k2="v2"`. Values may hold commas inside
// quotes (scope="repository:a:pull,push").
func parseChallengeParams(s string) map[string]string {
	out := map[string]string{}
	for s = strings.TrimSpace(s); s != ""; s = strings.TrimSpace(s) {
		k, rest, ok := strings.Cut(s, "=")
		if !ok {
			break
		}
		k = strings.ToLower(strings.TrimSpace(k))
		var v string
		if strings.HasPrefix(rest, `"`) {
			end := strings.Index(rest[1:], `"`)
			if end < 0 {
				v, rest = rest[1:], ""
			} else {
				v, rest = rest[1:1+end], rest[2+end:]
			}
		} else {
			v, rest, _ = strings.Cut(rest, ",")
			v = strings.TrimSpace(v)
		}
		out[k] = v
		rest = strings.TrimSpace(rest)
		s = strings.TrimPrefix(rest, ",")
	}
	return out
}

// NatsObjectStore implements PrecompiledObjectStore for nats://<bucket>/<key>
// URLs on a JetStream object store. A claim is two JetStream requests: a read
// of the last metadata message and a conditional re-publish of it. No chunk
// is read.
type NatsObjectStore struct {
	Conn *nats.Conn
}

func (n *NatsObjectStore) Claim(ctx context.Context, rawURL string) (bool, error) {
	bucket, key, err := splitNatsObjectURL(rawURL)
	if err != nil {
		return false, err
	}
	js, err := n.Conn.JetStream(nats.MaxWait(natsRequestTimeout))
	if err != nil {
		return false, fmt.Errorf("acquiring JetStream context: %w", err)
	}
	subj := metaSubject(bucket, base64.URLEncoding.EncodeToString([]byte(key)))
	msg, err := js.GetLastMsg(objectStreamName(bucket), subj, nats.Context(ctx))
	switch {
	case errors.Is(err, nats.ErrStreamNotFound), errors.Is(err, nats.ErrMsgNotFound):
		return false, nil
	case err != nil:
		return false, fmt.Errorf("reading metadata of %s: %w", rawURL, err)
	}
	var info nats.ObjectInfo
	if err := json.Unmarshal(msg.Data, &info); err != nil {
		return false, fmt.Errorf("decoding metadata of %s: %w", rawURL, err)
	}
	if info.Deleted || info.Size == 0 || info.NUID == "" {
		return false, nil
	}
	// Re-publish the same metadata. Its new message time is the ModTime the
	// GC reads, so this restarts the grace period. It only lands if nobody
	// (in particular the GC's delete) wrote the metadata since our read.
	if err := publishMetaIfUnchanged(js, bucket, subj, msg.Data, msg.Sequence, nats.Context(ctx)); err != nil {
		if isWrongLastSequence(err) {
			// Deleted or rewritten under us: treat as a miss. The Job path is
			// always safe.
			return false, nil
		}
		return false, fmt.Errorf("touching %s: %w", rawURL, err)
	}
	return true, nil
}

func splitNatsObjectURL(rawURL string) (bucket, key string, err error) {
	u, err := url.Parse(rawURL)
	if err != nil || u.Scheme != "nats" || u.Host == "" {
		return "", "", fmt.Errorf("not a nats://<bucket>/<key> URL: %q", rawURL)
	}
	key = strings.TrimPrefix(u.Path, "/")
	if key == "" {
		return "", "", fmt.Errorf("nats:// URL has no object key: %q", rawURL)
	}
	return u.Host, key, nil
}
