//! Component execution context for wasmtime stores.
//!
//! This module provides the [`Ctx`] type which serves as the store context
//! for wasmtime when executing WebAssembly components. It integrates WASI
//! interfaces, HTTP capabilities, and plugin access into a unified context.

use std::{
    any::Any,
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

use crate::plugin::HostPlugin;

/// Counts in-flight host-side work performed on behalf of one invocation
/// (store). Used by the P3 HTTP driver to decide when a store with
/// post-response background guest work may be dropped: background work is
/// guaranteed to stay alive while it has tracked host activity in flight.
///
/// Pure-compute gaps between host operations are bridged by a grace window
/// in the driver, not by this counter — see `host/http_p3.rs`.
#[derive(Default)]
pub struct HostWorkTracker {
    count: std::sync::atomic::AtomicUsize,
}

impl HostWorkTracker {
    /// Number of in-flight tracked operations.
    pub fn in_flight(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Track one host-side operation; the returned guard decrements on drop.
    pub fn track(self: &Arc<Self>) -> HostWorkGuard {
        self.count.fetch_add(1, Ordering::Relaxed);
        HostWorkGuard {
            tracker: self.clone(),
        }
    }
}

/// RAII guard for one tracked host-side operation.
pub struct HostWorkGuard {
    tracker: Arc<HostWorkTracker>,
}

impl Drop for HostWorkGuard {
    fn drop(&mut self) {
        self.tracker.count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Shared context for linked components
pub struct SharedCtx {
    /// Current active context
    pub active_ctx: Ctx,
    /// The resource table used to manage resources in the Wasmtime store.
    pub table: wasmtime::component::ResourceTable,
    /// Contexts for linked components
    pub contexts: HashMap<Arc<str>, Ctx>,
    /// Instances of linked components in this store, keyed by component id.
    /// Populated eagerly on the P3 dispatch path (concurrent host functions
    /// cannot instantiate) and lazily by the classic async trampoline.
    pub linked_instances: HashMap<Arc<str>, wasmtime::component::Instance>,
    /// In-flight linked calls (callee component ids), used to switch the
    /// active context around inter-component calls. See
    /// [`SharedCtx::enter_linked_call`] for the isolation contract.
    linked_call_stack: Vec<Arc<str>>,
    /// The store's root (dispatched) component, restored when the linked
    /// call stack empties.
    root_component_id: Arc<str>,
}

impl SharedCtx {
    pub fn new(context: Ctx) -> Self {
        let root_component_id = context.component_id.clone();
        Self {
            active_ctx: context,
            table: ResourceTable::new(),
            contexts: Default::default(),
            linked_instances: Default::default(),
            linked_call_stack: Vec::new(),
            root_component_id,
        }
    }

    pub fn set_active_ctx(&mut self, id: &Arc<str>) -> wasmtime::Result<()> {
        if id == &self.active_ctx.component_id {
            return Ok(());
        }

        if let Some(ctx) = self.contexts.remove(id) {
            let old_ctx = std::mem::replace(&mut self.active_ctx, ctx);
            self.contexts.insert(old_ctx.component_id.clone(), old_ctx);
            Ok(())
        } else {
            Err(wasmtime::format_err!(
                "Context for component {id} not found"
            ))
        }
    }

    /// Switch the active context to `callee` for the duration of a linked
    /// inter-component call. Must be paired with
    /// [`SharedCtx::exit_linked_call`] (including on error paths).
    ///
    /// Isolation contract: the active-context slot is a single value, so
    /// per-component context isolation is exact only while linked calls do
    /// not overlap, or while every overlapping call targets the *same*
    /// callee and the caller performs no context-sensitive host calls in
    /// the meantime. Wasmtime exposes no caller identity to host functions,
    /// so overlapping calls into *different* callees leave a window where a
    /// host call observes the wrong component's context — detected and
    /// warned about here.
    pub fn enter_linked_call(&mut self, callee: &Arc<str>) -> wasmtime::Result<()> {
        if let Some(top) = self.linked_call_stack.last()
            && top != callee
        {
            tracing::warn!(
                in_flight = top.as_ref(),
                entering = callee.as_ref(),
                "overlapping linked calls into different components: \
                 per-component context isolation is best-effort here"
            );
        }
        self.linked_call_stack.push(callee.clone());
        self.set_active_ctx(callee)
    }

    /// Pop one in-flight linked call to `callee` and restore the context of
    /// the most recent remaining call (or the root component).
    pub fn exit_linked_call(&mut self, callee: &Arc<str>) -> wasmtime::Result<()> {
        if let Some(pos) = self.linked_call_stack.iter().rposition(|c| c == callee) {
            self.linked_call_stack.remove(pos);
        }
        let target = self
            .linked_call_stack
            .last()
            .cloned()
            .unwrap_or_else(|| self.root_component_id.clone());
        self.set_active_ctx(&target)
    }
}

impl wasmtime::component::HasData for SharedCtx {
    type Data<'a> = ActiveCtx<'a>;
}

pub fn extract_active_ctx(ctx: &mut SharedCtx) -> ActiveCtx<'_> {
    ActiveCtx {
        table: &mut ctx.table,
        ctx: &mut ctx.active_ctx,
    }
}

pub fn extract_sockets(ctx: &mut SharedCtx) -> crate::sockets::WasiSocketsCtxView<'_> {
    crate::sockets::WasiSocketsCtxView {
        ctx: &mut ctx.active_ctx.sockets,
        table: &mut ctx.table,
    }
}

pub struct ActiveCtx<'a> {
    pub table: &'a mut wasmtime::component::ResourceTable,
    pub ctx: &'a mut Ctx,
}

impl<'a> Deref for ActiveCtx<'a> {
    type Target = Ctx;

    fn deref(&self) -> &Self::Target {
        self.ctx
    }
}

impl<'a> DerefMut for ActiveCtx<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx
    }
}

/// The context for a component store and linker, providing access to implementations of:
/// - wasi@0.2 interfaces
/// - wasi:http@0.2 interfaces
pub struct Ctx {
    /// Unique identifier for this component context. This is a [uuid::Uuid::new_v4] string.
    pub id: String,
    /// The unique identifier for the workload component this instance belongs to
    pub component_id: Arc<str>,
    /// The unique identifier for the workload this component belongs to
    pub workload_id: Arc<str>,
    /// The WASI context used to provide WASI functionality to the components using this context.
    pub ctx: WasiCtx,
    /// The HTTP context used to provide HTTP functionality to the component.
    pub http: WasiHttpCtx,
    /// The sockets context used to provide socket functionality (with loopback support).
    pub sockets: crate::sockets::WasiSocketsCtx,
    /// Per-invocation cancellation handle. One underlying flag per store: the
    /// active context and every linked context hold clones of the same `Arc`.
    /// Detectors (e.g. a cancel plugin) set it; the store's epoch-deadline
    /// callback reads it on each tick and traps the invocation mid-wasm if
    /// tripped (see `new_store_from_metadata`).
    pub cancel_handle: Arc<AtomicBool>,
    /// Per-invocation tracker of in-flight host-side work (outbound HTTP
    /// requests, concurrent linked-component calls, ...). One tracker per
    /// store, shared by all contexts like `cancel_handle`. The P3 HTTP
    /// driver keeps the store alive after the response was returned for as
    /// long as tracked work remains — this is what makes guest background
    /// work (async submit) survive past the response.
    pub host_work: Arc<HostWorkTracker>,
    /// Plugin instances stored by string ID for access during component execution.
    /// These all implement the [`HostPlugin`] trait, but they are cast as `Arc<dyn Any + Send + Sync>`
    /// to support downcasting to the specific plugin type in [`Ctx::get_plugin`]
    plugins: HashMap<&'static str, Arc<dyn Any + Send + Sync>>,
    /// The HTTP hooks for outgoing HTTP requests (implements WasiHttpHooks for P2).
    http_hooks: CtxHttpHooks,
    /// The HTTP hooks for outgoing HTTP requests (implements WasiHttpHooks for P3).
    #[cfg(feature = "wasip3")]
    http_hooks_p3: CtxHttpHooksP3,
}

impl Ctx {
    /// Get a plugin by its string ID and downcast to the expected type
    pub fn get_plugin<T: HostPlugin + 'static>(&self, plugin_id: &str) -> Option<Arc<T>> {
        self.plugins.get(plugin_id)?.clone().downcast().ok()
    }

    /// Create a new [`CtxBuilder`] to construct a [`Ctx`]
    pub fn builder(
        workload_id: impl Into<Arc<str>>,
        component_id: impl Into<Arc<str>>,
    ) -> CtxBuilder {
        CtxBuilder::new(workload_id, component_id)
    }
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("id", &self.id)
            .field("workload_id", &self.workload_id.as_ref())
            .finish()
    }
}

// TODO(#103): Do some cleverness to pull up the WasiCtx based on what component is actively executing
impl WasiView for SharedCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.active_ctx.ctx,
            table: &mut self.table,
        }
    }
}

impl wasmtime_wasi_io::IoView for SharedCtx {
    fn table(&mut self) -> &mut wasmtime_wasi::ResourceTable {
        &mut self.table
    }
}

// Implement WasiHttpView for wasi:http@0.2
impl WasiHttpView for SharedCtx {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.active_ctx.http,
            table: &mut self.table,
            hooks: &mut self.active_ctx.http_hooks,
        }
    }
}

// Implement WasiHttpView for wasi:http P3
#[cfg(feature = "wasip3")]
impl wasmtime_wasi_http::p3::WasiHttpView for SharedCtx {
    fn http(&mut self) -> wasmtime_wasi_http::p3::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p3::WasiHttpCtxView {
            ctx: &mut self.active_ctx.http,
            table: &mut self.table,
            hooks: &mut self.active_ctx.http_hooks_p3,
        }
    }
}

/// HTTP hooks implementation that delegates to a [`HostHandler`](crate::host::http::HostHandler).
struct CtxHttpHooks {
    http_handler: Option<Arc<dyn crate::host::http::HostHandler>>,
    workload_id: Arc<str>,
    allowed_hosts: Arc<[String]>,
}

impl WasiHttpHooks for CtxHttpHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<wasmtime_wasi_http::p2::body::HyperOutgoingBody>,
        config: wasmtime_wasi_http::p2::types::OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::p2::HttpResult<wasmtime_wasi_http::p2::types::HostFutureIncomingResponse>
    {
        match &self.http_handler {
            Some(handler) => {
                handler.outgoing_request(&self.workload_id, request, config, &self.allowed_hosts)
            }
            None => Err(wasmtime_wasi_http::p2::HttpError::trap(
                wasmtime::format_err!("http client not available"),
            )),
        }
    }
}

/// P3 HTTP hooks implementation that enforces allowed hosts and delegates
/// to the default send_request for actual HTTP transport.
#[cfg(feature = "wasip3")]
struct CtxHttpHooksP3 {
    allowed_hosts: Arc<[String]>,
    /// Outbound requests count as in-flight host work so the P3 driver
    /// keeps the store alive while a background guest task awaits one.
    host_work: Arc<HostWorkTracker>,
}

#[cfg(feature = "wasip3")]
impl wasmtime_wasi_http::p3::WasiHttpHooks for CtxHttpHooksP3 {
    fn send_request(
        &mut self,
        request: hyper::http::Request<
            http_body_util::combinators::UnsyncBoxBody<
                bytes::Bytes,
                wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
            >,
        >,
        options: Option<wasmtime_wasi_http::p3::RequestOptions>,
        fut: Box<
            dyn std::future::Future<
                    Output = Result<(), wasmtime_wasi_http::p3::bindings::http::types::ErrorCode>,
                > + Send,
        >,
    ) -> Box<
        dyn std::future::Future<
                Output = Result<
                    (
                        hyper::http::Response<
                            http_body_util::combinators::UnsyncBoxBody<
                                bytes::Bytes,
                                wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                            >,
                        >,
                        Box<
                            dyn std::future::Future<
                                    Output = Result<
                                        (),
                                        wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                                    >,
                                > + Send,
                        >,
                    ),
                    wasmtime_wasi::TrappableError<
                        wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                    >,
                >,
            > + Send,
    > {
        use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode as P3ErrorCode;

        // Check allowed hosts before sending
        if let Err(_e) = crate::host::http::check_allowed_hosts(&request, &self.allowed_hosts) {
            return Box::new(async move {
                Err(wasmtime_wasi::TrappableError::from(
                    P3ErrorCode::HttpRequestDenied,
                ))
            });
        }

        // Delegate to the default send_request implementation. The guard
        // keeps the request counted as in-flight host work until the
        // response (head) has arrived.
        _ = fut;
        let guard = self.host_work.track();
        Box::new(async move {
            use http_body_util::BodyExt;
            let _guard = guard;
            let (res, io) = wasmtime_wasi_http::p3::default_send_request(request, options).await?;
            Ok((
                res.map(BodyExt::boxed_unsync),
                Box::new(io)
                    as Box<dyn std::future::Future<Output = Result<(), P3ErrorCode>> + Send>,
            ))
        })
    }
}

/// Helper struct to build a [`Ctx`] with a builder pattern
pub struct CtxBuilder {
    id: String,
    workload_id: Arc<str>,
    component_id: Arc<str>,
    ctx: Option<WasiCtx>,
    sockets: Option<crate::sockets::WasiSocketsCtx>,
    plugins: HashMap<&'static str, Arc<dyn HostPlugin + Send + Sync>>,
    http_handler: Option<Arc<dyn crate::host::http::HostHandler>>,
    allowed_hosts: Arc<[String]>,
    cancel_handle: Option<Arc<AtomicBool>>,
    host_work: Option<Arc<HostWorkTracker>>,
}

impl CtxBuilder {
    pub fn new(workload_id: impl Into<Arc<str>>, component_id: impl Into<Arc<str>>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            component_id: component_id.into(),
            workload_id: workload_id.into(),
            ctx: None,
            sockets: None,
            http_handler: None,
            plugins: HashMap::new(),
            allowed_hosts: Default::default(),
            cancel_handle: None,
            host_work: None,
        }
    }

    /// Set a custom [WasiCtx]
    pub fn with_wasi_ctx(mut self, ctx: WasiCtx) -> Self {
        self.ctx = Some(ctx);
        self
    }

    pub fn with_sockets(mut self, sockets: crate::sockets::WasiSocketsCtx) -> Self {
        self.sockets = Some(sockets);
        self
    }

    pub fn with_http_handler(
        mut self,
        http_handler: Arc<dyn crate::host::http::HostHandler>,
    ) -> Self {
        self.http_handler = Some(http_handler);
        self
    }

    pub fn with_plugins(
        mut self,
        plugins: HashMap<&'static str, Arc<dyn HostPlugin + Send + Sync>>,
    ) -> Self {
        self.plugins.extend(plugins);
        self
    }

    pub fn with_allowed_hosts(mut self, allowed_hosts: Arc<[String]>) -> Self {
        self.allowed_hosts = allowed_hosts;
        self
    }

    /// Share an existing cancellation handle with this context. All contexts
    /// of one store must hold the same handle; if none is provided, a fresh
    /// (never-tripped) one is created in [`CtxBuilder::build`].
    pub fn with_cancel_handle(mut self, cancel_handle: Arc<AtomicBool>) -> Self {
        self.cancel_handle = Some(cancel_handle);
        self
    }

    /// Share an existing host-work tracker with this context. All contexts
    /// of one store must hold the same tracker; if none is provided, a fresh
    /// one is created in [`CtxBuilder::build`].
    pub fn with_host_work(mut self, host_work: Arc<HostWorkTracker>) -> Self {
        self.host_work = Some(host_work);
        self
    }

    pub fn build(self) -> Ctx {
        let plugins = self
            .plugins
            .into_iter()
            .map(|(k, v)| (k, v as Arc<dyn Any + Send + Sync>))
            .collect();

        let host_work = self.host_work.unwrap_or_default();

        #[cfg(feature = "wasip3")]
        let http_hooks_p3 = CtxHttpHooksP3 {
            allowed_hosts: self.allowed_hosts.clone(),
            host_work: host_work.clone(),
        };

        let http_hooks = CtxHttpHooks {
            http_handler: self.http_handler,
            workload_id: self.workload_id.clone(),
            allowed_hosts: self.allowed_hosts,
        };

        Ctx {
            id: self.id,
            ctx: self.ctx.unwrap_or_else(|| {
                WasiCtxBuilder::new()
                    .args(&["main.wasm"])
                    .inherit_stderr()
                    .build()
            }),
            workload_id: self.workload_id,
            component_id: self.component_id,
            http: WasiHttpCtx::new(),
            sockets: self.sockets.unwrap_or_default(),
            cancel_handle: self.cancel_handle.unwrap_or_default(),
            host_work,
            plugins,
            http_hooks,
            #[cfg(feature = "wasip3")]
            http_hooks_p3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_builder_sets_ids() {
        let ctx = Ctx::builder("wk-1", "comp-1").build();
        assert_eq!(ctx.workload_id.as_ref(), "wk-1");
        assert_eq!(ctx.component_id.as_ref(), "comp-1");
    }

    #[test]
    fn ctx_builder_generates_uuid_id() {
        let ctx = Ctx::builder("wk", "comp").build();
        // id should be a valid UUID v4 string
        assert!(uuid::Uuid::parse_str(&ctx.id).is_ok());
    }

    #[test]
    fn ctx_builder_uses_default_wasi_ctx_when_none_provided() {
        // Should not panic — proves default WasiCtx is created
        let _ctx = Ctx::builder("wk", "comp").build();
    }

    #[test]
    fn shared_ctx_new_sets_active_ctx() {
        let ctx = Ctx::builder("wk", "comp-a").build();
        let shared = SharedCtx::new(ctx);
        assert_eq!(shared.active_ctx.component_id.as_ref(), "comp-a");
        assert!(shared.contexts.is_empty());
    }

    #[test]
    fn set_active_ctx_swaps_context() {
        let ctx_a = Ctx::builder("wk", "comp-a").build();
        let ctx_b = Ctx::builder("wk", "comp-b").build();
        let comp_b_id: Arc<str> = Arc::from("comp-b");

        let mut shared = SharedCtx::new(ctx_a);
        shared.contexts.insert(comp_b_id.clone(), ctx_b);

        shared.set_active_ctx(&comp_b_id).unwrap();
        assert_eq!(shared.active_ctx.component_id.as_ref(), "comp-b");
        // The old context should now be in the map
        assert!(
            shared
                .contexts
                .contains_key(&Arc::from("comp-a") as &Arc<str>)
        );
    }

    #[test]
    fn set_active_ctx_returns_error_for_unknown_id() {
        let ctx = Ctx::builder("wk", "comp-a").build();
        let mut shared = SharedCtx::new(ctx);
        let unknown: Arc<str> = Arc::from("nonexistent");
        let result = shared.set_active_ctx(&unknown);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn set_active_ctx_is_noop_when_already_active() {
        let ctx = Ctx::builder("wk", "comp-a").build();
        let mut shared = SharedCtx::new(ctx);
        let comp_a: Arc<str> = Arc::from("comp-a");
        // Should succeed and be a no-op
        shared.set_active_ctx(&comp_a).unwrap();
        assert_eq!(shared.active_ctx.component_id.as_ref(), "comp-a");
        assert!(shared.contexts.is_empty());
    }
}
