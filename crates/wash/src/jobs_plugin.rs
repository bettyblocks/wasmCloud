//! Demo job-group plugin backing the `examples/cancellable-jobs` example.
//!
//! Implements the `demo:jobs` package as host functions:
//! - `control` — create a job group (spawns N counter invocations) and
//!   cancel it (trips every invocation's cancellation handle).
//! - `reporter` — progress reporting from counters; doubles as the
//!   cancellation actuator via [`ensure_not_cancelled`].
//! - `events` — a non-blocking subscribe/poll feed consumed by the SSE
//!   service component.
//!
//! The plugin is inert unless a workload declares `demo:jobs` interfaces.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::Context as _;
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};
use wash_runtime::{
    engine::{
        ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
        workload::{ResolvedWorkload, WorkloadItem},
    },
    plugin::HostPlugin,
    wasmtime,
    wit::{WitInterface, WitWorld},
};

const JOBS_PLUGIN_ID: &str = "demo-jobs";
const COUNTERS_PER_GROUP: u32 = 10;
const EVENT_CHANNEL_CAPACITY: usize = 256;

mod bindings {
    use wash_runtime::wasmtime;

    wasmtime::component::bindgen!({
        world: "plugin-host",
        wasmtime_crate: wash_runtime::wasmtime,
        imports: { default: async | trappable },
        exports: { default: async },
        inline: "
            package demo:jobs@0.1.0;

            interface control {
                create: func() -> result<string, string>;
                cancel: func(request-id: string) -> bool;
            }

            interface reporter {
                report: func(request-id: string, index: u32, count: u32);
            }

            interface events {
                record count-data {
                    index: u32,
                    count: u32,
                }

                variant event {
                    count(count-data),
                    cancelled,
                    done,
                }

                subscribe: func(request-id: string) -> result<u64, string>;
                poll-next: func(subscription: u64) -> option<event>;
                unsubscribe: func(subscription: u64);
            }

            interface runner {
                run: func(request-id: string, index: u32);
            }

            world plugin-host {
                import control;
                import reporter;
                import events;
                export runner;
            }
        ",
    });
}

use bindings::demo::jobs::events::{CountData, Event};

#[derive(Clone, Copy, PartialEq)]
enum GroupState {
    Running,
    Cancelled,
    Done,
}

/// One job group: the unit /create returns and /cancel targets.
struct Group {
    /// Workload that created the group; cancel is scoped to it.
    creator_workload_id: String,
    state: GroupState,
    /// Cancellation handles of the counter invocations, one per store.
    handles: Vec<Arc<AtomicBool>>,
    events_tx: broadcast::Sender<Event>,
}

/// A subscription created by the SSE service. A group that is already in a
/// terminal state subscribes as `Terminal` so the first poll still delivers
/// the outcome.
enum Subscription {
    Live(broadcast::Receiver<Event>),
    Terminal(Event),
}

/// Everything needed to programmatically invoke the counter component.
#[derive(Clone)]
struct RunnerTarget {
    workload: ResolvedWorkload,
    component_id: String,
    pre: bindings::PluginHostPre<SharedCtx>,
}

#[derive(Default)]
pub struct JobsPlugin {
    groups: Arc<RwLock<HashMap<String, Group>>>,
    subscriptions: Arc<RwLock<HashMap<u64, Subscription>>>,
    next_subscription: AtomicU64,
    runner_component_id: RwLock<Option<String>>,
    runner_target: Arc<RwLock<Option<RunnerTarget>>>,
}

impl JobsPlugin {
    /// Spawn the group's counter invocations and a supervisor that marks
    /// the group done once they have all finished.
    fn spawn_group(&self, target: RunnerTarget, request_id: String) {
        let groups = self.groups.clone();

        tokio::spawn(async move {
            let mut set = tokio::task::JoinSet::new();

            for index in 0..COUNTERS_PER_GROUP {
                let target = target.clone();
                let groups = groups.clone();
                let request_id = request_id.clone();

                set.spawn(async move {
                    let mut store = target
                        .workload
                        .new_store(&target.component_id)
                        .await
                        .context("failed to create counter store")?;

                    // Register the invocation's cancel handle with the
                    // group before any guest code runs. If the group was
                    // cancelled in the meantime, trip it immediately.
                    let handle = store.data().active_ctx.cancel_handle.clone();
                    {
                        let mut groups = groups.write().await;
                        let Some(group) = groups.get_mut(&request_id) else {
                            anyhow::bail!("group disappeared before counter start");
                        };
                        group.handles.push(handle.clone());
                        if group.state == GroupState::Cancelled {
                            handle.store(true, Ordering::Relaxed);
                        }
                    }

                    let instance = target
                        .pre
                        .instantiate_async(&mut store)
                        .await
                        .map_err(anyhow::Error::from)
                        .context("failed to instantiate counter")?;

                    instance
                        .demo_jobs_runner()
                        .call_run(&mut store, &request_id, index)
                        .await
                        .map_err(anyhow::Error::from)
                        .with_context(|| format!("counter {index} ended"))
                });
            }

            while let Some(result) = set.join_next().await {
                match result {
                    // A trap here is the expected way a cancelled counter ends.
                    Ok(Err(e)) => debug!(request_id, "counter ended with error: {e:#}"),
                    Err(e) => warn!(request_id, "counter task panicked: {e}"),
                    Ok(Ok(())) => {}
                }
            }

            let mut groups = groups.write().await;
            if let Some(group) = groups.get_mut(&request_id)
                && group.state == GroupState::Running
            {
                group.state = GroupState::Done;
                let _ = group.events_tx.send(Event::Done);
                info!(request_id, "job group completed");
            }
        });
    }
}

impl bindings::demo::jobs::control::Host for ActiveCtx<'_> {
    async fn create(&mut self) -> wasmtime::Result<Result<String, String>> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(Err("jobs plugin not available".to_string()));
        };

        let Some(target) = plugin.runner_target.read().await.clone() else {
            return Ok(Err("counter component not available".to_string()));
        };

        let request_id = uuid::Uuid::new_v4().to_string();
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        plugin.groups.write().await.insert(
            request_id.clone(),
            Group {
                creator_workload_id: self.workload_id.to_string(),
                state: GroupState::Running,
                handles: Vec::with_capacity(COUNTERS_PER_GROUP as usize),
                events_tx,
            },
        );

        plugin.spawn_group(target, request_id.clone());
        info!(request_id, "job group created");

        Ok(Ok(request_id))
    }

    async fn cancel(&mut self, request_id: String) -> wasmtime::Result<bool> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(false);
        };

        let mut groups = plugin.groups.write().await;
        let Some(group) = groups.get_mut(&request_id) else {
            return Ok(false);
        };

        // Tenancy seam: only the workload that created a group may cancel it.
        if group.creator_workload_id != self.workload_id.as_ref() {
            warn!(
                request_id,
                caller = self.workload_id.as_ref(),
                "cancel denied: caller is not the creating workload"
            );
            return Ok(false);
        }

        if group.state != GroupState::Running {
            return Ok(false);
        }

        group.state = GroupState::Cancelled;
        for handle in &group.handles {
            handle.store(true, Ordering::Relaxed);
        }
        let _ = group.events_tx.send(Event::Cancelled);
        info!(request_id, "job group cancelled");

        Ok(true)
    }
}

impl bindings::demo::jobs::reporter::Host for ActiveCtx<'_> {
    async fn report(&mut self, request_id: String, index: u32, count: u32) -> wasmtime::Result<()> {
        // The cancellation actuator: a cancelled invocation traps here and
        // unwinds before producing any further effects.
        self.ensure_not_cancelled()?;

        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(());
        };

        if let Some(group) = plugin.groups.read().await.get(&request_id) {
            // No receivers yet is fine; counts before a subscriber are dropped.
            let _ = group.events_tx.send(Event::Count(CountData { index, count }));
        }

        Ok(())
    }
}

impl bindings::demo::jobs::events::Host for ActiveCtx<'_> {
    async fn subscribe(&mut self, request_id: String) -> wasmtime::Result<Result<u64, String>> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(Err("jobs plugin not available".to_string()));
        };

        let subscription = match plugin.groups.read().await.get(&request_id) {
            None => return Ok(Err(format!("unknown request-id: {request_id}"))),
            Some(group) => match group.state {
                GroupState::Running => Subscription::Live(group.events_tx.subscribe()),
                GroupState::Cancelled => Subscription::Terminal(Event::Cancelled),
                GroupState::Done => Subscription::Terminal(Event::Done),
            },
        };

        let id = plugin.next_subscription.fetch_add(1, Ordering::Relaxed);
        plugin.subscriptions.write().await.insert(id, subscription);
        Ok(Ok(id))
    }

    async fn poll_next(&mut self, subscription: u64) -> wasmtime::Result<Option<Event>> {
        let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) else {
            return Ok(None);
        };

        let mut subscriptions = plugin.subscriptions.write().await;
        let Some(sub) = subscriptions.get_mut(&subscription) else {
            // Unknown or already-terminal subscription: report done so the
            // caller closes its connection.
            return Ok(Some(Event::Done));
        };

        match sub {
            Subscription::Terminal(event) => {
                let event = event.clone();
                subscriptions.remove(&subscription);
                Ok(Some(event))
            }
            Subscription::Live(rx) => loop {
                match rx.try_recv() {
                    Ok(event @ (Event::Cancelled | Event::Done)) => {
                        subscriptions.remove(&subscription);
                        break Ok(Some(event));
                    }
                    Ok(event) => break Ok(Some(event)),
                    Err(broadcast::error::TryRecvError::Empty) => break Ok(None),
                    // Skipped some counts under load; keep draining.
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(broadcast::error::TryRecvError::Closed) => {
                        subscriptions.remove(&subscription);
                        break Ok(Some(Event::Done));
                    }
                }
            },
        }
    }

    async fn unsubscribe(&mut self, subscription: u64) -> wasmtime::Result<()> {
        if let Some(plugin) = self.get_plugin::<JobsPlugin>(JOBS_PLUGIN_ID) {
            plugin.subscriptions.write().await.remove(&subscription);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl HostPlugin for JobsPlugin {
    fn id(&self) -> &'static str {
        JOBS_PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: [WitInterface::from("demo:jobs/control,reporter,events@0.1.0")]
                .into_iter()
                .collect(),
            exports: [WitInterface::from("demo:jobs/runner@0.1.0")]
                .into_iter()
                .collect(),
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

        // Each item has its own linker; register all three host-side
        // interfaces — definitions a component doesn't import are ignored.
        bindings::demo::jobs::control::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;
        bindings::demo::jobs::reporter::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;
        bindings::demo::jobs::events::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;

        if item
            .world()
            .exports
            .contains(&WitInterface::from("demo:jobs/runner@0.1.0"))
        {
            debug!(component_id = item.id(), "tracking counter component");
            *self.runner_component_id.write().await = Some(item.id().to_string());
        }

        Ok(())
    }

    async fn on_workload_resolved(
        &self,
        workload: &ResolvedWorkload,
        component_id: &str,
    ) -> anyhow::Result<()> {
        if self.runner_component_id.read().await.as_deref() != Some(component_id) {
            return Ok(());
        }

        let instance_pre = workload.instantiate_pre(component_id).await?;
        let pre = bindings::PluginHostPre::new(instance_pre)
            .map_err(anyhow::Error::from)
            .context("counter component does not match the demo:jobs runner world")?;

        *self.runner_target.write().await = Some(RunnerTarget {
            workload: workload.clone(),
            component_id: component_id.to_string(),
            pre,
        });
        debug!(component_id, "counter runner target resolved");

        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        _workload_id: &str,
        _interfaces: std::collections::HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        // Single-workload demo plugin: drop everything on unbind so a dev
        // reload starts clean.
        *self.runner_target.write().await = None;
        *self.runner_component_id.write().await = None;
        self.groups.write().await.clear();
        self.subscriptions.write().await.clear();
        Ok(())
    }
}
