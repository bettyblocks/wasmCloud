//! Ergonomic OpenTelemetry helpers for wasmCloud components.
//!
//! Wraps the raw `wasi:otel` interfaces with a guard-based API where spans
//! automatically end when they go out of scope.
//!
//! # Usage
//!
//! ```rust,ignore
//! use component_otel::{Tracer, SpanKind};
//!
//! let tracer = Tracer::from_host("my-service", "0.1.0");
//! let _root = tracer.span("handle-request")
//!     .kind(SpanKind::Server)
//!     .attr("http.method", "GET")
//!     .start();
//!
//! tracer.log_info("processing request");
//!
//! {
//!     let _child = tracer.span("db-query")
//!         .kind(SpanKind::Client)
//!         .start();
//!     do_query();
//! } // child span ends here automatically
//! ```

#[allow(warnings)]
pub mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use std::cell::{Cell, RefCell};

use bindings::wasi::clocks::wall_clock;
use bindings::wasi::otel;

pub use otel::tracing::SpanKind;

/// Create a key-value attribute with a string value.
///
/// Handles the JSON encoding required by the `wasi:otel` interface.
pub fn kv(key: &str, value: &str) -> otel::types::KeyValue {
    otel::types::KeyValue {
        key: key.into(),
        value: format!("\"{value}\""),
    }
}

/// Create a key-value attribute with a numeric value.
pub fn kv_num(key: &str, value: impl std::fmt::Display) -> otel::types::KeyValue {
    otel::types::KeyValue {
        key: key.into(),
        value: value.to_string(),
    }
}

fn now() -> wall_clock::Datetime {
    wall_clock::now()
}

/// Per-request tracer that manages trace context and span nesting.
///
/// Create one at the start of each request via [`Tracer::from_host`].
/// Spans created from this tracer automatically nest under each other
/// and are correlated to the host's trace context.
pub struct Tracer {
    trace_id: String,
    #[allow(dead_code)]
    root_parent_span_id: String,
    service_name: String,
    service_version: String,
    span_counter: Cell<u64>,
    current_span_id: RefCell<String>,
}

impl Tracer {
    /// Connect to the host's trace context and create a new tracer.
    ///
    /// Calls `outer_span_context()` to retrieve the host's active trace,
    /// so all spans created from this tracer will appear as children
    /// of the host's span in your observability backend.
    pub fn from_host(service_name: &str, service_version: &str) -> Self {
        let outer = otel::tracing::outer_span_context();
        let root_parent_span_id = outer.span_id.clone();
        let trace_id = outer.trace_id;
        Self {
            current_span_id: RefCell::new(root_parent_span_id.clone()),
            trace_id,
            root_parent_span_id,
            service_name: service_name.into(),
            service_version: service_version.into(),
            span_counter: Cell::new(1),
        }
    }

    /// Start building a new span.
    pub fn span<'a>(&'a self, name: &str) -> SpanBuilder<'a> {
        SpanBuilder {
            tracer: self,
            name: name.into(),
            kind: SpanKind::Internal,
            attributes: Vec::new(),
        }
    }

    /// Emit an INFO log correlated to the current trace.
    pub fn log_info(&self, message: &str) {
        self.emit_log(9, "INFO", message);
    }

    /// Emit a DEBUG log correlated to the current trace.
    pub fn log_debug(&self, message: &str) {
        self.emit_log(5, "DEBUG", message);
    }

    /// Emit a WARN log correlated to the current trace.
    pub fn log_warn(&self, message: &str) {
        self.emit_log(13, "WARN", message);
    }

    /// Emit an ERROR log correlated to the current trace.
    pub fn log_error(&self, message: &str) {
        self.emit_log(17, "ERROR", message);
    }

    fn emit_log(&self, severity: u8, severity_text: &str, body: &str) {
        otel::logs::on_emit(&otel::logs::LogRecord {
            timestamp: Some(now()),
            observed_timestamp: None,
            severity_text: Some(severity_text.into()),
            severity_number: Some(severity),
            body: Some(format!("\"{body}\"")),
            attributes: None,
            event_name: None,
            resource: Some(self.resource()),
            instrumentation_scope: Some(self.scope()),
            trace_id: Some(self.trace_id.clone()),
            span_id: Some(self.current_span_id.borrow().clone()),
            trace_flags: None,
        });
    }

    fn scope(&self) -> otel::types::InstrumentationScope {
        otel::types::InstrumentationScope {
            name: self.service_name.clone(),
            version: Some(self.service_version.clone()),
            schema_url: None,
            attributes: vec![],
        }
    }

    fn resource(&self) -> otel::types::Resource {
        otel::types::Resource {
            attributes: vec![kv("service.name", &self.service_name)],
            schema_url: None,
        }
    }

    fn new_span_id(&self) -> String {
        let id = self.span_counter.get();
        self.span_counter.set(id + 1);
        format!("{id:016x}")
    }
}

/// Builder for configuring a span before starting it.
pub struct SpanBuilder<'a> {
    tracer: &'a Tracer,
    name: String,
    kind: SpanKind,
    attributes: Vec<otel::types::KeyValue>,
}

impl<'a> SpanBuilder<'a> {
    /// Set the span kind (Client, Server, Producer, Consumer, Internal).
    pub fn kind(mut self, kind: SpanKind) -> Self {
        self.kind = kind;
        self
    }

    /// Add a string attribute to the span.
    pub fn attr(mut self, key: &str, value: &str) -> Self {
        self.attributes.push(kv(key, value));
        self
    }

    /// Add a numeric attribute to the span.
    pub fn attr_num(mut self, key: &str, value: impl std::fmt::Display) -> Self {
        self.attributes.push(kv_num(key, value));
        self
    }

    /// Start the span. Returns a guard that ends the span on drop.
    pub fn start(self) -> SpanGuard<'a> {
        let span_id = self.tracer.new_span_id();
        let parent_span_id = self.tracer.current_span_id.borrow().clone();

        let ctx = otel::tracing::SpanContext {
            trace_id: self.tracer.trace_id.clone(),
            span_id: span_id.clone(),
            trace_flags: otel::tracing::TraceFlags::SAMPLED,
            is_remote: false,
            trace_state: vec![],
        };
        otel::tracing::on_start(&ctx);

        // Update current span to this one
        *self.tracer.current_span_id.borrow_mut() = span_id;

        SpanGuard {
            tracer: self.tracer,
            ctx,
            parent_span_id,
            name: self.name,
            kind: self.kind,
            attributes: self.attributes,
            events: Vec::new(),
            status: otel::tracing::Status::Ok,
            start_time: now(),
        }
    }
}

/// RAII guard that ends a span when dropped.
///
/// The span is automatically ended with the correct duration when the guard
/// goes out of scope. You can add events or set error status before it drops.
pub struct SpanGuard<'a> {
    tracer: &'a Tracer,
    ctx: otel::tracing::SpanContext,
    parent_span_id: String,
    name: String,
    kind: SpanKind,
    attributes: Vec<otel::types::KeyValue>,
    events: Vec<otel::tracing::Event>,
    status: otel::tracing::Status,
    start_time: wall_clock::Datetime,
}

impl<'a> SpanGuard<'a> {
    /// Get the span ID (useful for manual correlation).
    pub fn span_id(&self) -> &str {
        &self.ctx.span_id
    }

    /// Add an event to the span.
    pub fn event(&mut self, name: &str, attributes: Vec<otel::types::KeyValue>) {
        self.events.push(otel::tracing::Event {
            name: name.into(),
            time: now(),
            attributes,
        });
    }

    /// Add an attribute to the span after it has started.
    pub fn add_attr(&mut self, key: &str, value: &str) {
        self.attributes.push(kv(key, value));
    }

    /// Add a numeric attribute to the span after it has started.
    pub fn add_attr_num(&mut self, key: &str, value: impl std::fmt::Display) {
        self.attributes.push(kv_num(key, value));
    }

    /// Mark the span as an error with a message.
    pub fn set_error(&mut self, message: &str) {
        self.status = otel::tracing::Status::Error(message.into());
    }
}

impl<'a> Drop for SpanGuard<'a> {
    fn drop(&mut self) {
        // Restore parent as current span
        *self.tracer.current_span_id.borrow_mut() = self.parent_span_id.clone();

        otel::tracing::on_end(&otel::tracing::SpanData {
            span_context: otel::tracing::SpanContext {
                trace_id: self.ctx.trace_id.clone(),
                span_id: self.ctx.span_id.clone(),
                trace_flags: self.ctx.trace_flags,
                is_remote: self.ctx.is_remote,
                trace_state: self.ctx.trace_state.clone(),
            },
            parent_span_id: self.parent_span_id.clone(),
            span_kind: self.kind,
            name: self.name.clone(),
            start_time: self.start_time,
            end_time: now(),
            attributes: std::mem::take(&mut self.attributes),
            events: std::mem::take(&mut self.events),
            links: vec![],
            status: std::mem::replace(&mut self.status, otel::tracing::Status::Ok),
            instrumentation_scope: self.tracer.scope(),
            dropped_attributes: 0,
            dropped_events: 0,
            dropped_links: 0,
        });
    }
}
