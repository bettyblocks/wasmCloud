//! Demo job-group plugin backing the `examples/cancellable-jobs` example.
//!
//! Reduced to the single thing a guest cannot do: reach the host-side
//! cancellation handles. The frontend component spawns the counter work
//! itself (concurrent linked calls + P3 async submit), reports flow over
//! the workload's virtual loopback network, and cancellation is enforced
//! by the runtime (host-boundary checks + epoch interruption) — this
//! plugin only maps a request-id to the registering invocation's handle
//! and trips it on cancel.
//!
//! The plugin is inert unless a workload declares `demo:jobs/control`.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::RwLock;
use tracing::{info, warn};
use wash_runtime::{
    engine::{
        ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
        workload::WorkloadItem,
    },
    plugin::HostPlugin,
    wasmtime,
    wit::{WitInterface, WitWorld},
};

const JOBS_PLUGIN_ID: &str = "demo-jobs";

mod bindings {
    use wash_runtime::wasmtime;

    wasmtime::component::bindgen!({
        world: "plugin-host",
        wasmtime_crate: wash_runtime::wasmtime,
        imports: { default: async | trappable },
        inline: "
            package demo:jobs@0.1.0;

            interface control {
                register: func(request-id: string);
                cancel: func(request-id: string) -> bool;
            }

            world plugin-host {
                import control;
            }
        ",
    });
}

/// One registration: the unit /create returns and /cancel targets.
struct Registration {
    /// Workload that registered the id; cancel is scoped to it.
    creator_workload_id: String,
    cancelled: bool,
    /// The registering invocation's cancellation handle (one per store).
    handle: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct JobsPlugin {
    registrations: Arc<RwLock<HashMap<String, Registration>>>,
}

impl bindings::demo::jobs::control::Host for ActiveCtx<'_> {
    async fn register(&mut self, request_id: String) -> wasmtime::Result<()> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Err(wasmtime::format_err!("jobs plugin not available"));
        };

        // The caller's OWN cancellation handle: tripping it later cancels
        // the registering invocation's whole store.
        let handle = self.cancel_handle.clone();
        let mut registrations = plugin.registrations.write().await;
        if registrations.contains_key(&request_id) {
            return Err(wasmtime::format_err!(
                "request-id '{request_id}' already registered"
            ));
        }
        registrations.insert(
            request_id.clone(),
            Registration {
                creator_workload_id: self.workload_id.to_string(),
                cancelled: false,
                handle,
            },
        );
        info!(request_id, "job registered");
        Ok(())
    }

    async fn cancel(&mut self, request_id: String) -> wasmtime::Result<bool> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(false);
        };

        let mut registrations = plugin.registrations.write().await;
        let Some(registration) = registrations.get_mut(&request_id) else {
            return Ok(false);
        };

        // Tenancy seam: only the workload that registered an id may cancel it.
        if registration.creator_workload_id != self.workload_id.as_ref() {
            warn!(
                request_id,
                caller = self.workload_id.as_ref(),
                "cancel denied: caller is not the registering workload"
            );
            return Ok(false);
        }

        if registration.cancelled {
            return Ok(false);
        }

        registration.cancelled = true;
        registration.handle.store(true, Ordering::Relaxed);
        info!(request_id, "job cancelled");

        Ok(true)
    }
}

#[async_trait::async_trait]
impl HostPlugin for JobsPlugin {
    fn id(&self) -> &'static str {
        JOBS_PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: [WitInterface::from("demo:jobs/control@0.1.0")]
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: std::collections::HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        if !interfaces
            .iter()
            .any(|i| i.namespace == "demo" && i.package == "jobs")
        {
            return Ok(());
        }

        bindings::demo::jobs::control::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;
        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        _workload_id: &str,
        _interfaces: std::collections::HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        // Single-workload demo plugin: drop everything on unbind so a dev
        // reload starts clean.
        self.registrations.write().await.clear();
        Ok(())
    }
}
