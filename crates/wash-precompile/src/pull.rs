use std::collections::HashSet;

use anyhow::{Context, Result, anyhow};
use docker_credential::{DockerCredential, get_credential};
use oci_client::{
    ParseError, Reference,
    client::{Client, ClientConfig, ClientProtocol},
    errors::{OciDistributionError, OciErrorCode},
    secrets::RegistryAuth,
};
use oci_wasm::WASM_LAYER_MEDIA_TYPE;

// Pre-Wasm-WG-standardization media type that wasmCloud's existing
// public images still ship with. Match wash-runtime's behavior of
// accepting both. See wash-runtime crate.
const LEGACY_WASMCLOUD_MEDIA_TYPE: &str = "application/vnd.module.wasm.content.layer.v1+wasm";

/// Pulls wasm components. One `Puller` is shared by all items of a batch so
/// that the HTTP connection pool and the registry token cache are reused.
pub struct Puller {
    insecure_registries: HashSet<String>,
    https: Client,
    http: Client,
}

impl Puller {
    /// Reads `INSECURE_REGISTRIES` (comma-separated hosts pulled over plain HTTP).
    pub fn from_env() -> Self {
        Self::new(&std::env::var("INSECURE_REGISTRIES").unwrap_or_default())
    }

    fn new(insecure_registries: &str) -> Self {
        let client = |protocol| {
            Client::new(ClientConfig {
                protocol,
                ..Default::default()
            })
        };
        Self {
            insecure_registries: insecure_registries
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            https: client(ClientProtocol::Https),
            http: client(ClientProtocol::Http),
        }
    }

    fn client_for(&self, registry: &str) -> &Client {
        if self.insecure_registries.contains(registry) {
            &self.http
        } else {
            &self.https
        }
    }

    /// Pulls the single wasm layer of `reference`. The error keeps the
    /// `oci_client` error in its chain, so [`is_permanent_pull_error`] can
    /// classify it.
    pub async fn fetch(&self, reference: &str) -> Result<Vec<u8>> {
        let parsed = Reference::try_from(reference)
            .with_context(|| format!("invalid OCI reference: {reference}"))?;

        let auth = resolve_auth(parsed.registry());
        let image = self
            .client_for(parsed.registry())
            .pull(
                &parsed,
                &auth,
                vec![LEGACY_WASMCLOUD_MEDIA_TYPE, WASM_LAYER_MEDIA_TYPE],
            )
            .await
            .with_context(|| format!("failed to pull {reference}"))?;

        // NOTE: Wasm OCI images contain a single layer
        // See: https://tag-runtime.cncf.io/wgs/wasm/deliverables/wasm-oci-artifact/
        let bytes = image
            .layers
            .first()
            .ok_or_else(|| anyhow!("no layers in pulled artifact: {reference}"))?
            .data
            .clone();

        // BettyBlocks: oci-client's layer `.data` is now `bytes::Bytes` after upstream's dep bump; convert to Vec<u8>.
        Ok(bytes.to_vec())
    }
}

/// Pulls one component with a fresh client. This is the original single-item path.
pub async fn fetch(reference: &str) -> Result<Vec<u8>> {
    Puller::from_env().fetch(reference).await
}

/// True when the chain of `err` holds a pull error that a retry cannot fix:
/// an invalid reference, an unknown image or a wrong artifact type. Auth
/// errors, rate limits, 5xx and network errors may fix themselves, so they
/// stay retryable.
pub fn is_permanent_pull_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<ParseError>()
            || cause
                .downcast_ref::<OciDistributionError>()
                .is_some_and(is_permanent)
    })
}

fn is_permanent(err: &OciDistributionError) -> bool {
    match err {
        OciDistributionError::ImageManifestNotFoundError(_)
        | OciDistributionError::IncompatibleLayerMediaTypeError(_)
        | OciDistributionError::PullNoLayersError
        | OciDistributionError::UnsupportedMediaTypeError(_)
        | OciDistributionError::UnsupportedSchemaVersionError(_) => true,
        OciDistributionError::ServerError { code, .. } => *code == 404,
        OciDistributionError::RegistryError { envelope, .. } => {
            !envelope.errors.is_empty()
                && envelope.errors.iter().all(|e| {
                    matches!(
                        e.code,
                        OciErrorCode::ManifestUnknown
                            | OciErrorCode::NameUnknown
                            | OciErrorCode::NameInvalid
                            | OciErrorCode::NotFound
                            | OciErrorCode::BlobUnknown
                    )
                })
        }
        _ => false,
    }
}

fn resolve_auth(registry: &str) -> RegistryAuth {
    match get_credential(registry) {
        Ok(DockerCredential::UsernamePassword(user, pass)) => RegistryAuth::Basic(user, pass),
        Ok(DockerCredential::IdentityToken(_)) | Err(_) => RegistryAuth::Anonymous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_client::errors::{OciEnvelope, OciError};

    fn registry_error(code: OciErrorCode) -> OciDistributionError {
        OciDistributionError::RegistryError {
            envelope: OciEnvelope {
                errors: vec![OciError {
                    code,
                    message: String::new(),
                    detail: Default::default(),
                }],
            },
            url: "https://r/v2/x/manifests/1".to_string(),
        }
    }

    #[test]
    fn unknown_image_is_permanent() {
        assert!(is_permanent(&registry_error(OciErrorCode::ManifestUnknown)));
        assert!(is_permanent(&registry_error(OciErrorCode::NameUnknown)));
        assert!(is_permanent(&OciDistributionError::ServerError {
            code: 404,
            url: String::new(),
            message: String::new(),
        }));
    }

    #[test]
    fn auth_rate_limit_and_server_errors_are_retryable() {
        assert!(!is_permanent(&registry_error(OciErrorCode::Unauthorized)));
        assert!(!is_permanent(&registry_error(OciErrorCode::Denied)));
        assert!(!is_permanent(&registry_error(
            OciErrorCode::Toomanyrequests
        )));
        assert!(!is_permanent(&OciDistributionError::UnauthorizedError {
            url: String::new()
        }));
        assert!(!is_permanent(&OciDistributionError::ServerError {
            code: 503,
            url: String::new(),
            message: String::new(),
        }));
    }

    #[tokio::test]
    async fn invalid_reference_is_permanent() {
        let err = Puller::new("").fetch("Not A Reference!").await.unwrap_err();
        assert!(is_permanent_pull_error(&err), "{err:#}");
    }

    #[tokio::test]
    async fn unreachable_registry_is_retryable() {
        // Port 1 on localhost refuses the connection: a network error, not a missing image.
        let err = Puller::new("127.0.0.1:1")
            .fetch("127.0.0.1:1/comp:v1")
            .await
            .unwrap_err();
        assert!(!is_permanent_pull_error(&err), "{err:#}");
    }

    #[test]
    fn classifies_through_the_anyhow_chain() {
        let err = anyhow::Error::new(registry_error(OciErrorCode::ManifestUnknown))
            .context("failed to pull r/a:1");
        assert!(is_permanent_pull_error(&err));
        let err = anyhow::Error::new(registry_error(OciErrorCode::Unauthorized))
            .context("failed to pull r/a:1");
        assert!(!is_permanent_pull_error(&err));
    }

    #[test]
    fn insecure_registries_are_trimmed() {
        let p = Puller::new(" localhost:5000 , ,registry.local ");
        assert!(p.insecure_registries.contains("localhost:5000"));
        assert!(p.insecure_registries.contains("registry.local"));
        assert_eq!(p.insecure_registries.len(), 2);
    }
}
