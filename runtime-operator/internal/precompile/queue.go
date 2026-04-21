// Package precompile defines the operator-side interfaces for the
// out-of-process precompile pipeline: a JobProducer enqueues compile work,
// and a CompletionConsumer subscribes to the events the worker emits.
//
// Implementations live in subpackages (see ./nats). The interfaces are kept
// transport-agnostic so a future S3-only or in-process variant could plug in
// without touching reconciler code.
package precompile

import "context"

// PrecompileJob mirrors the JSON wire-format the Rust worker decodes. Any
// change here must be matched in crates/precompile-worker/src/job.rs.
type PrecompileJob struct {
	ArtifactNamespace string `json:"artifact_namespace"`
	ArtifactName      string `json:"artifact_name"`
	ArtifactImage     string `json:"artifact_image"`
	RegistryUsername  string `json:"registry_username,omitempty"`
	RegistryPassword  string `json:"registry_password,omitempty"`
	Target            string `json:"target"`
	WasmtimeVersion   string `json:"wasmtime_version"`
}

// PublishedVariant carries the metadata the worker writes back when a
// compile succeeds. The reconciler appends this directly to
// Artifact.status.precompiled.
type PublishedVariant struct {
	Target          string `json:"target"`
	WasmtimeVersion string `json:"wasmtime_version"`
	CompatHash      string `json:"compat_hash"`
	ArtifactURL     string `json:"artifact_url"`
	Digest          string `json:"digest"`
}

// PrecompileCompletion mirrors the worker's PrecompileCompletion type.
//
// `Result` is encoded the way Rust's `Result<T, E>` serializes via serde:
// either `{"Ok": <variant>}` or `{"Err": "<message>"}`. The shape is captured
// here as two optional fields populated mutually exclusively.
type PrecompileCompletion struct {
	ArtifactNamespace string           `json:"artifact_namespace"`
	ArtifactName      string           `json:"artifact_name"`
	Target            string           `json:"target"`
	WasmtimeVersion   string           `json:"wasmtime_version"`
	Result            CompletionResult `json:"result"`
}

// CompletionResult mirrors `serde_json` encoding of `Result<PublishedVariant, String>`.
// Exactly one of Ok/Err is populated.
type CompletionResult struct {
	Ok  *PublishedVariant `json:"Ok,omitempty"`
	Err *string           `json:"Err,omitempty"`
}

// JobProducer enqueues compile work onto the worker pool's queue.
//
// Submit must be safe to call repeatedly with the same job; downstream
// content-addressed puts make duplicates harmless.
type JobProducer interface {
	Submit(ctx context.Context, job PrecompileJob) error
}

// CompletionConsumer subscribes to completion events. Handler is invoked
// once per event; the implementation handles redelivery semantics.
type CompletionConsumer interface {
	Subscribe(ctx context.Context, handler func(PrecompileCompletion)) error
}
