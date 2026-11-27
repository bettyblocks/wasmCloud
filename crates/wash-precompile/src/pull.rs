use std::collections::HashSet;

use anyhow::{Context, Result, anyhow};
use docker_credential::{
    CredentialRetrievalError, DockerCredential, get_credential, get_credential_from_reader,
};
use oci_client::{
    Reference,
    client::{Client, ClientConfig, ClientProtocol},
    secrets::RegistryAuth,
};
use oci_wasm::WASM_LAYER_MEDIA_TYPE;
// Pre-Wasm-WG-standardization media type that wasmCloud's existing
// public images still ship with. Match wash-runtime's behavior of
// accepting both. See wash-runtime crate.
const LEGACY_WASMCLOUD_MEDIA_TYPE: &str = "application/vnd.module.wasm.content.layer.v1+wasm";

/// Env var with the content of a docker config.json (the `.dockerconfigjson`
/// key of a `kubernetes.io/dockerconfigjson` Secret). The runtime-operator sets
/// it from the Artifact's imagePullSecret. When it is not set we fall back to
/// `$DOCKER_CONFIG/config.json`, which older operators mount as a volume.
const DOCKER_CONFIG_JSON_ENV: &str = "DOCKER_CONFIG_JSON";

pub async fn fetch(reference: &str) -> Result<Vec<u8>> {
    let parsed = Reference::try_from(reference)
        .with_context(|| format!("invalid OCI reference: {reference}"))?;

    let insecure_registries: HashSet<String> = std::env::var("INSECURE_REGISTRIES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let client = Client::new(ClientConfig {
        protocol: if insecure_registries.contains(parsed.registry()) {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        ..Default::default()
    });
    let auth = resolve_auth(
        parsed.registry(),
        std::env::var(DOCKER_CONFIG_JSON_ENV).ok().as_deref(),
    );

    let image = client
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

type CredentialResult = std::result::Result<DockerCredential, CredentialRetrievalError>;

/// Resolve the registry credentials. `docker_config_json` (the content of
/// `DOCKER_CONFIG_JSON`) wins over `$DOCKER_CONFIG/config.json`. As before,
/// a missing or unusable credential falls back to an anonymous pull; the
/// registry then answers with a clear 401 when it needs credentials.
fn resolve_auth(registry: &str, docker_config_json: Option<&str>) -> RegistryAuth {
    resolve_auth_with(registry, docker_config_json, get_credential)
}

fn resolve_auth_with(
    registry: &str,
    docker_config_json: Option<&str>,
    from_docker_config_dir: impl FnOnce(&str) -> CredentialResult,
) -> RegistryAuth {
    let credential = match docker_config_json.filter(|json| !json.trim().is_empty()) {
        Some(json) => get_credential_from_reader(json.as_bytes(), registry).inspect_err(|e| {
            if !matches!(e, CredentialRetrievalError::NoCredentialConfigured) {
                eprintln!("wash-precompile: ignoring {DOCKER_CONFIG_JSON_ENV} for {registry}: {e}");
            }
        }),
        // Same as before this env var existed: a missing or unreadable
        // config.json means an anonymous pull.
        None => from_docker_config_dir(registry),
    };
    match credential {
        Ok(DockerCredential::UsernamePassword(user, pass)) => RegistryAuth::Basic(user, pass),
        Ok(DockerCredential::IdentityToken(_)) | Err(_) => RegistryAuth::Anonymous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "registry.example.com";
    // base64("robot:s3cret")
    const AUTH_JSON: &str = r#"{"auths":{"registry.example.com":{"auth":"cm9ib3Q6czNjcmV0"}}}"#;

    fn basic(user: &str, pass: &str) -> RegistryAuth {
        RegistryAuth::Basic(user.to_string(), pass.to_string())
    }

    /// Stands in for `$DOCKER_CONFIG/config.json` (the old volume mount).
    fn dir_has(user: &'static str, pass: &'static str) -> impl FnOnce(&str) -> CredentialResult {
        move |_| Ok(DockerCredential::UsernamePassword(user.into(), pass.into()))
    }

    fn dir_missing(_: &str) -> CredentialResult {
        Err(CredentialRetrievalError::ConfigReadError)
    }

    #[test]
    fn env_json_with_auth_field_gives_basic_auth() {
        let auth = resolve_auth_with(REGISTRY, Some(AUTH_JSON), dir_missing);
        assert_eq!(auth, basic("robot", "s3cret"));
    }

    #[test]
    fn env_json_with_username_and_password_gives_basic_auth() {
        let json =
            r#"{"auths":{"https://registry.example.com/v1/":{"username":"u","password":"p"}}}"#;
        let auth = resolve_auth_with(REGISTRY, Some(json), dir_missing);
        assert_eq!(auth, basic("u", "p"));
    }

    #[test]
    fn env_json_wins_over_docker_config_dir() {
        let auth = resolve_auth_with(REGISTRY, Some(AUTH_JSON), dir_has("old", "creds"));
        assert_eq!(auth, basic("robot", "s3cret"));
    }

    #[test]
    fn unset_env_json_falls_back_to_docker_config_dir() {
        let auth = resolve_auth_with(REGISTRY, None, dir_has("old", "creds"));
        assert_eq!(auth, basic("old", "creds"));
    }

    #[test]
    fn empty_env_json_falls_back_to_docker_config_dir() {
        let auth = resolve_auth_with(REGISTRY, Some("  "), dir_has("old", "creds"));
        assert_eq!(auth, basic("old", "creds"));
    }

    #[test]
    fn no_credentials_anywhere_is_anonymous() {
        let auth = resolve_auth_with(REGISTRY, None, dir_missing);
        assert_eq!(auth, RegistryAuth::Anonymous);
    }

    #[test]
    fn env_json_without_entry_for_registry_is_anonymous() {
        let auth = resolve_auth_with(
            "other.example.com",
            Some(AUTH_JSON),
            dir_has("old", "creds"),
        );
        assert_eq!(auth, RegistryAuth::Anonymous);
    }

    #[test]
    fn invalid_env_json_is_anonymous() {
        let auth = resolve_auth_with(REGISTRY, Some("not json"), dir_has("old", "creds"));
        assert_eq!(auth, RegistryAuth::Anonymous);
    }

    #[test]
    fn identity_token_is_anonymous() {
        let json = r#"{"auths":{"registry.example.com":{"identitytoken":"tok"}}}"#;
        let auth = resolve_auth_with(REGISTRY, Some(json), dir_missing);
        assert_eq!(auth, RegistryAuth::Anonymous);
    }
}
