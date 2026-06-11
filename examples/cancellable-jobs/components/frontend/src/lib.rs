//! Frontend: P3 async HTTP component.
//!
//! Routes:
//! - `POST /create[?mode=burn]` — register a fresh request-id with the
//!   cancel plugin (mapping THIS invocation's cancellation handle), spawn
//!   ten CONCURRENT linked calls into the counter component, and
//!   `task.return` the response immediately while they keep running
//!   (platform async submit — the P3 HTTP driver keeps this store alive
//!   while the linked calls are in flight).
//! - `POST /cancel/<id>` — trip the registered handle: the /create store
//!   (its background task and all ten in-flight counter calls) is torn
//!   down together by the runtime's actuators.
//! - `GET /events/<id>` — thin byte pipe from the wash HTTP ingress to the
//!   sse-service over the virtual loopback, streamed live to the client.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::demo::jobs::{control, runner};
use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};
use bindings::wasi::random::random::get_random_bytes;
use bindings::wasi::sockets::types::{
    IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket,
};
use wit_bindgen::StreamResult;

struct Component;

const COUNTERS_PER_GROUP: u32 = 10;

const SSE_SERVICE_ADDR: IpSocketAddress = IpSocketAddress::Ipv4(Ipv4SocketAddress {
    port: 8081,
    address: (127, 0, 0, 1),
});

impl Handler for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let method = request.get_method();

        use bindings::wasi::http::types::Method;
        match (&method, path.as_str()) {
            (Method::Post, p) if p == "/create" || p.starts_with("/create?") => {
                let burn = p.contains("mode=burn");
                Ok(create(burn).await)
            }
            (Method::Post, p) if p.starts_with("/cancel/") => {
                let id = p["/cancel/".len()..].to_string();
                let cancelled = control::cancel(&id);
                Ok(text_response(200, &format!("{cancelled}\n")))
            }
            (Method::Get, p) if p.starts_with("/events/") => {
                let id = p["/events/".len()..].to_string();
                Ok(events(&id).await)
            }
            _ => Ok(text_response(
                404,
                "usage: POST /create[?mode=burn] | POST /cancel/<id> | GET /events/<id>\n",
            )),
        }
    }
}

/// Register this invocation's cancel handle under a fresh id, spawn the
/// counter calls, and return immediately.
async fn create(burn: bool) -> Response {
    let id = random_id();
    control::register(&id);

    let spawn_id = id.clone();
    wit_bindgen::spawn(async move {
        // Ten concurrent linked calls into the counter component. The
        // invocation's store stays alive while they're in flight (each
        // call holds a host-work guard in the runtime).
        futures::future::join_all(
            (0..COUNTERS_PER_GROUP).map(|index| runner::run(spawn_id.clone(), index, burn)),
        )
        .await;
    });

    text_response(200, &format!("{id}\n"))
}

/// Pipe the sse-service's SSE byte stream for `id` into a streaming
/// response body.
async fn events(id: &str) -> Response {
    let Ok(socket) = TcpSocket::create(IpAddressFamily::Ipv4) else {
        return text_response(502, "socket create failed\n");
    };
    if socket.connect(SSE_SERVICE_ADDR).await.is_err() {
        return text_response(502, "sse-service unreachable\n");
    }

    // Register as a watcher.
    let (mut hello_tx, hello_rx) = bindings::wit_stream::new();
    let watch_line = format!("watch {id}\n").into_bytes();
    let (upstream_rx, _recv_result) = socket.receive();

    let headers = Fields::new();
    let _ = headers.append("content-type", b"text/event-stream");
    let _ = headers.append("cache-control", b"no-cache");

    let (mut body_tx, body_rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());

    wit_bindgen::spawn(async move {
        // The socket must stay owned by this task for the pipe's lifetime.
        let socket = socket;
        let mut upstream_rx = upstream_rx;

        futures::join!(
            async {
                let _ = socket.send(hello_rx).await;
            },
            async move {
                hello_tx.write_all(watch_line).await;
                drop(hello_tx);
            },
            async {
                loop {
                    let (result, data) = upstream_rx.read(Vec::with_capacity(4096)).await;
                    match result {
                        StreamResult::Complete(_) => {
                            if !data.is_empty() {
                                let leftover = body_tx.write_all(data).await;
                                if !leftover.is_empty() {
                                    // Client went away.
                                    break;
                                }
                            }
                        }
                        StreamResult::Dropped | StreamResult::Cancelled => break,
                    }
                }
                drop(body_tx);
                let _ = trailers_tx.write(Ok(None)).await;
            }
        );
    });

    let (response, _result) = Response::new(headers, Some(body_rx), trailers_rx);
    let _ = response.set_status_code(200);
    response
}

fn random_id() -> String {
    get_random_bytes(16)
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
