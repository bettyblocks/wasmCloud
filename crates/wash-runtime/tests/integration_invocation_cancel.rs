//! POC integration test for per-invocation cancellation through the real
//! invocation path (design: `docs/WORKLOAD_CANCELLATION.md`).
//!
//! The pieces under test:
//! 1. **Identity link** — `new_store_from_metadata` mints one cancellation
//!    handle per store and announces `(workload_id, token, handle)` to the
//!    bound plugins via `on_invocation_start`, before any guest code runs.
//! 2. **Listener** — a guest-routed HTTP `/cancel/<token>` request calls the
//!    `wasmcloud:cancel-spinner/canceller` import; the test-local
//!    [`CancelPlugin`] looks the token up scoped to the *calling* workload
//!    and trips the handle.
//! 3. **Actuator** — the keyvalue host functions check the handle on entry
//!    and trap, so the spinning invocation unwinds at its next host call.

use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::time::{sleep, timeout};

use wash_runtime::{
    engine::{
        Engine,
        ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
        workload::WorkloadItem,
    },
    host::{
        HostApi, HostBuilder,
        http::{DevRouter, HttpServer},
    },
    plugin::{HostPlugin, wasi_keyvalue::InMemoryKeyValue},
    types::{Component, LocalResources, Workload, WorkloadStartRequest},
    wit::{WitInterface, WitWorld},
};

mod bindings {
    wasmtime::component::bindgen!({
        imports: { default: async | trappable },
        inline: "
            package wasmcloud:cancel-spinner@0.1.0;

            interface canceller {
                cancel: func(token: string) -> bool;
            }

            world host {
                import canceller;
            }
        "
    });
}

const CANCEL_SPINNER_WASM: &[u8] = include_bytes!("wasm/cancel_spinner.wasm");
const CANCEL_PLUGIN_ID: &str = "invocation-cancel";
const HOST_HEADER: &str = "spinner";

/// Test-local cancel plugin: holds the registry mapping
/// `(workload_id, token)` to that invocation's cancellation handle.
///
/// Keying by workload id is the tenancy seam: the `cancel` host function
/// only looks up tokens under the *calling* context's workload id, so a
/// workload can never trip another workload's invocations.
#[derive(Default)]
struct CancelPlugin {
    registry: RwLock<HashMap<(String, String), Arc<AtomicBool>>>,
}

impl CancelPlugin {
    fn entries(&self) -> Vec<((String, String), Arc<AtomicBool>)> {
        self.registry
            .read()
            .expect("registry lock poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

impl bindings::wasmcloud::cancel_spinner::canceller::Host for ActiveCtx<'_> {
    async fn cancel(&mut self, token: String) -> wasmtime::Result<bool> {
        let plugin = self
            .get_plugin::<CancelPlugin>(CANCEL_PLUGIN_ID)
            .ok_or_else(|| wasmtime::format_err!("cancel plugin not available"))?;

        let registry = plugin
            .registry
            .read()
            .map_err(|_| wasmtime::format_err!("registry lock poisoned"))?;
        match registry.get(&(self.workload_id.to_string(), token)) {
            Some(handle) => {
                handle.store(true, Ordering::Relaxed);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[async_trait::async_trait]
impl HostPlugin for CancelPlugin {
    fn id(&self) -> &'static str {
        CANCEL_PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from(
                "wasmcloud:cancel-spinner/canceller@0.1.0",
            )]),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        _interfaces: HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        bindings::wasmcloud::cancel_spinner::canceller::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;
        Ok(())
    }

    fn on_invocation_start(&self, workload_id: &str, token: &str, cancel_handle: Arc<AtomicBool>) {
        self.registry
            .write()
            .expect("registry lock poisoned")
            .insert((workload_id.to_string(), token.to_string()), cancel_handle);
    }
}

fn spinner_workload_request(workload_id: &str) -> WorkloadStartRequest {
    WorkloadStartRequest {
        workload_id: workload_id.to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "cancel-spinner".to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![Component {
                name: "spinner".to_string(),
                digest: None,
                bytes: bytes::Bytes::from_static(CANCEL_SPINNER_WASM),
                local_resources: LocalResources {
                    memory_limit_mb: 128,
                    cpu_limit: 1,
                    config: HashMap::new(),
                    environment: HashMap::new(),
                    volume_mounts: vec![],
                    allowed_hosts: Default::default(),
                },
                pool_size: 1,
                max_invocations: 100,
                is_precompiled: false,
            }],
            host_interfaces: vec![
                WitInterface {
                    namespace: "wasi".to_string(),
                    package: "http".to_string(),
                    interfaces: ["incoming-handler".to_string()].into_iter().collect(),
                    version: Some(semver::Version::parse("0.2.2").unwrap()),
                    config: HashMap::from([("host".to_string(), HOST_HEADER.to_string())]),
                    name: None,
                },
                WitInterface {
                    namespace: "wasi".to_string(),
                    package: "keyvalue".to_string(),
                    interfaces: ["store".to_string(), "atomics".to_string()]
                        .into_iter()
                        .collect(),
                    version: Some(semver::Version::parse("0.2.0-draft").unwrap()),
                    config: HashMap::new(),
                    name: None,
                },
                WitInterface {
                    namespace: "wasmcloud".to_string(),
                    package: "cancel-spinner".to_string(),
                    interfaces: ["canceller".to_string()].into_iter().collect(),
                    version: Some(semver::Version::parse("0.1.0").unwrap()),
                    config: HashMap::new(),
                    name: None,
                },
            ],
            volumes: vec![],
        },
    }
}

/// Start a host with the cancel-spinner workload deployed. Returns the HTTP
/// address, the cancel plugin (for registry inspection), the workload id,
/// and the running host (which must be kept alive by the caller).
async fn start_spinner_host() -> Result<(
    std::net::SocketAddr,
    Arc<CancelPlugin>,
    String,
    impl HostApi,
)> {
    let engine = Engine::builder().build()?;
    let http_server = HttpServer::new(DevRouter::default(), "127.0.0.1:0".parse()?).await?;
    let addr = http_server.addr();

    let cancel_plugin = Arc::new(CancelPlugin::default());
    let host = HostBuilder::new()
        .with_engine(engine)
        .with_http_handler(Arc::new(http_server))
        .with_plugin(Arc::new(InMemoryKeyValue::new()))?
        .with_plugin(cancel_plugin.clone())?
        .build()?;
    let host = host.start().await.context("failed to start host")?;

    let workload_id = uuid::Uuid::new_v4().to_string();
    host.workload_start(spinner_workload_request(&workload_id))
        .await
        .context("failed to start cancel-spinner workload")?;

    Ok((addr, cancel_plugin, workload_id, host))
}

/// Poll the registry until exactly one invocation token is present.
async fn wait_for_single_token(
    cancel_plugin: &CancelPlugin,
) -> Result<((String, String), Arc<AtomicBool>)> {
    timeout(Duration::from_secs(10), async {
        loop {
            let entries = cancel_plugin.entries();
            if let [entry] = entries.as_slice() {
                break entry.clone();
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("invocation never registered a cancellation token")
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_request_traps_spinning_invocation() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (addr, cancel_plugin, workload_id, _host) = start_spinner_host().await?;

    let client = reqwest::Client::new();

    // Kick off the long-running invocation. It loops on keyvalue increments
    // for up to 30s unless cancelled.
    let spin_started = Instant::now();
    let spin_task = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .get(format!("http://{addr}/spin"))
                .header("HOST", HOST_HEADER)
                .timeout(Duration::from_secs(60))
                .send()
                .await
        }
    });

    // The spinner's token appears in the registry at store creation, before
    // any guest code runs — so polling for exactly one entry can only
    // observe the spin invocation (no cancel request has been sent yet).
    let (registry_key, spin_handle) = wait_for_single_token(&cancel_plugin).await?;

    let (registered_workload_id, token) = registry_key;
    assert_eq!(
        registered_workload_id, workload_id,
        "registry entry must be keyed by the spinner's workload id"
    );
    assert!(
        !spin_handle.load(Ordering::Relaxed),
        "handle must not be tripped before cancel"
    );

    // A token that doesn't exist (under this workload) must not cancel
    // anything: the guest reports `false` from the canceller import.
    let bogus = client
        .get(format!("http://{addr}/cancel/no-such-token"))
        .header("HOST", HOST_HEADER)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("bogus cancel request failed")?;
    assert_eq!(bogus.status(), 200);
    assert_eq!(bogus.text().await?, "false");
    assert!(
        !spin_handle.load(Ordering::Relaxed),
        "bogus token must not trip the handle"
    );

    // The real cancel: routed to the guest, which calls the canceller
    // import; the plugin trips the spin invocation's handle.
    let cancelled_at = Instant::now();
    let real = client
        .get(format!("http://{addr}/cancel/{token}"))
        .header("HOST", HOST_HEADER)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("cancel request failed")?;
    assert_eq!(real.status(), 200);
    assert_eq!(real.text().await?, "true");
    assert!(
        spin_handle.load(Ordering::Relaxed),
        "cancel must trip the handle"
    );

    // The spinning invocation traps at its next keyvalue call: the response
    // arrives promptly and is not the loop's success message.
    let spin_response = spin_task.await.context("spin task panicked")?;
    let cancel_to_death = cancelled_at.elapsed();
    let total_spin = spin_started.elapsed();

    match spin_response {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            assert!(
                !status.is_success(),
                "cancelled invocation must not succeed, got {status} with body: {body}"
            );
            assert!(
                !body.starts_with("completed"),
                "spin loop must not run to completion: {body}"
            );
        }
        // A torn-down invocation may also surface as a connection-level
        // error rather than an HTTP status.
        Err(e) => {
            assert!(!e.is_timeout(), "spin request timed out instead of dying");
        }
    }

    assert!(
        cancel_to_death < Duration::from_secs(5),
        "invocation should die at its next host call, took {cancel_to_death:?}"
    );
    assert!(
        total_spin < Duration::from_secs(25),
        "invocation must die well before the 30s spin deadline, took {total_spin:?}"
    );

    // The rest of the workload is untouched: new invocations still work.
    let after = client
        .get(format!("http://{addr}/cancel/no-such-token"))
        .header("HOST", HOST_HEADER)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("post-cancel request failed")?;
    assert_eq!(after.status(), 200);
    assert_eq!(after.text().await?, "false");

    Ok(())
}

/// Layer 2 proof through the real invocation path: a PURE CPU-bound guest
/// (the `/spin-cpu` route makes zero host calls inside its loop) is
/// invisible to the Layer 1 host-boundary gate — only epoch interruption
/// can stop it. Cancelling must trap it mid-wasm, promptly.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_traps_cpu_bound_invocation() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (addr, cancel_plugin, workload_id, _host) = start_spinner_host().await?;
    let client = reqwest::Client::new();

    let spin_task = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .get(format!("http://{addr}/spin-cpu"))
                .header("HOST", HOST_HEADER)
                .timeout(Duration::from_secs(120))
                .send()
                .await
        }
    });

    let ((registered_workload_id, token), spin_handle) =
        wait_for_single_token(&cancel_plugin).await?;
    assert_eq!(registered_workload_id, workload_id);

    // Give the guest a moment to be deep inside its CPU loop, so the trap
    // provably lands mid-wasm rather than before the loop started.
    sleep(Duration::from_millis(250)).await;

    let cancelled_at = Instant::now();
    let real = client
        .get(format!("http://{addr}/cancel/{token}"))
        .header("HOST", HOST_HEADER)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("cancel request failed")?;
    assert_eq!(real.text().await?, "true");
    assert!(spin_handle.load(Ordering::Relaxed));

    // Without epoch interruption this would only return after the full CPU
    // spin (tens of seconds); the timeout is the failure detector.
    let spin_response = timeout(Duration::from_secs(10), spin_task)
        .await
        .context("CPU-bound invocation survived cancel — epoch interruption ineffective")?
        .context("spin task panicked")?;

    match spin_response {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            assert!(
                !status.is_success(),
                "cancelled CPU-bound invocation must not succeed, got {status}: {body}"
            );
            assert!(
                !body.starts_with("completed"),
                "cpu spin must not run to completion: {body}"
            );
        }
        Err(e) => {
            assert!(!e.is_timeout(), "spin request timed out instead of dying");
        }
    }

    assert!(
        cancelled_at.elapsed() < Duration::from_secs(5),
        "epoch trap should land within ticks, took {:?}",
        cancelled_at.elapsed()
    );

    Ok(())
}
