//! Frontend: P3 async HTTP component — the cancellation demo's front door.
//!
//! Routes:
//! - `POST /do-work?mode=tick|cpu` — register a fresh request-id with the
//!   cancel plugin (mapping THIS invocation's own cancellation handle),
//!   start ONE linked call into the counter component, and return the id
//!   immediately (P3 async submit). The linked call stays in flight inside
//!   this store, which is what keeps the detached store alive while the
//!   counter runs.
//! - `POST /cancel?id=<id>` — trip the registered handle. The /do-work
//!   store (and the counter call running inside it) is trapped by the
//!   runtime's epoch interruption and torn down; the plugin writes the
//!   terminal `cancelled` status.
//! - `GET /status?id=<id>` — read the plugin's status line for the id.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::demo::jobs::{control, runner};
use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::http::types::{ErrorCode, Fields, Method, Request, Response};
use bindings::wasi::random::random::get_random_bytes;

struct Component;

impl Handler for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let method = request.get_method();
        let (route, query) = split_query(&path);

        match (&method, route) {
            (Method::Post, "/do-work") => Ok(do_work(mode_of(query)).await),
            (Method::Post, "/cancel") => match param(query, "id") {
                Some(id) => Ok(text_response(200, &format!("{}\n", control::cancel(&id)))),
                None => Ok(text_response(400, "missing ?id=<id>\n")),
            },
            (Method::Get, "/status") => match param(query, "id") {
                Some(id) => Ok(text_response(200, &format!("{}\n", control::status(&id)))),
                None => Ok(text_response(400, "missing ?id=<id>\n")),
            },
            _ => Ok(text_response(
                404,
                "usage: POST /do-work?mode=tick|cpu | POST /cancel?id=<id> | GET /status?id=<id>\n",
            )),
        }
    }
}

/// Register this invocation's cancel handle under a fresh id, start the
/// counter, and return the id without waiting for it to finish.
async fn do_work(mode: String) -> Response {
    let id = random_id();
    // Register BEFORE starting the work, so a cancel issued the instant the
    // id is returned can never race ahead of registration.
    control::register(&id);

    let spawn_id = id.clone();
    wit_bindgen::spawn(async move {
        // One linked call into the counter. It runs inside THIS store and
        // stays in flight (the runtime counts it as host work), keeping the
        // detached driver from reaping the store while the counter works.
        runner::run(spawn_id, mode).await;
    });

    text_response(202, &format!("{id}\n"))
}

/// `tick` (default) or `cpu`.
fn mode_of(query: Option<&str>) -> String {
    match param(query, "mode").as_deref() {
        Some("cpu") => "cpu".to_string(),
        _ => "tick".to_string(),
    }
}

fn split_query(path: &str) -> (&str, Option<&str>) {
    match path.split_once('?') {
        Some((route, query)) => (route, Some(query)),
        None => (path, None),
    }
}

/// Pull `key`'s value out of a `&str` query string (`a=1&b=2`).
fn param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn random_id() -> String {
    get_random_bytes(8)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn text_response(status: u16, body: &str) -> Response {
    let headers = Fields::new();
    let (mut tx, rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());
    let body = body.as_bytes().to_vec();
    wit_bindgen::spawn(async move {
        tx.write_all(body).await;
        drop(tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });
    let (response, _result) = Response::new(headers, Some(rx), trailers_rx);
    let _ = response.set_status_code(status);
    response
}

bindings::export!(Component with_types_in bindings);
