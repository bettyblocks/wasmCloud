//! Cancellation control plugin backing the `examples/cancellable-counter`
//! example.
//!
//! It owns the one thing a guest cannot reach — the host-side per-invocation
//! cancellation handles — plus a small per-id lifecycle record. A workload
//! `register`s the calling invocation's own handle under a request-id and
//! reports `progress`/`complete`; a separate request `cancel`s it (tripping
//! the handle, which the runtime's epoch interruption turns into a trap that
//! tears down the work's store) and `status` reads the outcome back. The
//! frozen `count` under a `cancelled` status is the proof the work stopped.
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
                progress: func(request-id: string, count: u32);
                complete: func(request-id: string);
                cancel: func(request-id: string) -> bool;
                status: func(request-id: string) -> string;
            }

            world plugin-host {
                import control;
            }
        ",
    });
}

/// Terminal-or-running state of a registration. `Started` is the only
/// non-terminal state; the trapped work can never write `Cancelled` itself
/// (it is interrupted mid-execution), so `cancel` writes it from the live
/// canceller invocation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Started,
    Finished,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Started => "started",
            Status::Finished => "finished",
            Status::Cancelled => "cancelled",
        }
    }
}

/// One registration: the unit /do-work returns and /cancel targets.
struct Registration {
    /// Workload that registered the id; all operations are scoped to it.
    creator_workload_id: String,
    /// The registering invocation's cancellation handle (one per store).
    handle: Arc<AtomicBool>,
    /// Lifecycle state. The frozen `count` under a `Cancelled` status is the
    /// proof the work actually stopped advancing.
    status: Status,
    /// Last progress tick the work reported (0 in cpu mode, which never
    /// reports per-tick progress).
    count: u32,
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
                handle,
                status: Status::Started,
                count: 0,
            },
        );
        info!(request_id, "work registered");
        Ok(())
    }

    /// Record the work's latest progress tick. Best-effort: ignored if the id
    /// is gone, not the caller's, or already terminal (a late tick from work
    /// that is unwinding must not overwrite `cancelled`).
    async fn progress(&mut self, request_id: String, count: u32) -> wasmtime::Result<()> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(());
        };
        let mut registrations = plugin.registrations.write().await;
        if let Some(reg) = registrations.get_mut(&request_id) {
            if reg.creator_workload_id == self.workload_id.as_ref() && reg.status == Status::Started
            {
                reg.count = count;
            }
        }
        Ok(())
    }

    /// Mark natural completion. Only moves `Started -> Finished`, so it can
    /// never clobber a `Cancelled` outcome.
    async fn complete(&mut self, request_id: String) -> wasmtime::Result<()> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(());
        };
        let mut registrations = plugin.registrations.write().await;
        if let Some(reg) = registrations.get_mut(&request_id) {
            if reg.creator_workload_id == self.workload_id.as_ref() && reg.status == Status::Started
            {
                reg.status = Status::Finished;
                info!(request_id, count = reg.count, "work completed");
            }
        }
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

        // Idempotent and terminal-safe: only a running registration cancels.
        if registration.status != Status::Started {
            return Ok(false);
        }

        // The trapped work can't record its own demise, so the live canceller
        // writes the terminal status here while tripping the handle. `count`
        // is left frozen wherever the last progress tick landed — that frozen
        // value is the proof the work stopped advancing.
        registration.status = Status::Cancelled;
        registration.handle.store(true, Ordering::Relaxed);
        info!(request_id, count = registration.count, "work cancelled");

        Ok(true)
    }

    /// Human-readable status line, e.g. `"cancelled count=3"`. Scoped to the
    /// caller's workload so an id is not readable across tenants. Returns
    /// `"unknown"` for an unregistered or foreign id.
    async fn status(&mut self, request_id: String) -> wasmtime::Result<String> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok("unknown".to_string());
        };
        let registrations = plugin.registrations.read().await;
        let line = match registrations.get(&request_id) {
            Some(reg) if reg.creator_workload_id == self.workload_id.as_ref() => {
                format!("{} count={}", reg.status.as_str(), reg.count)
            }
            _ => "unknown".to_string(),
        };
        Ok(line)
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
