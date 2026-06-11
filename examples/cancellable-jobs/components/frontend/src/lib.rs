//! Frontend: short-lived per-request HTTP component.
//!
//! Routes:
//! - `POST /create`       -> start a job group, respond with its request-id
//! - `POST /cancel/<id>`  -> cancel the group, respond "true"/"false"
//! - `GET  /events/<id>`  -> thin byte pipe to the sse-service over the
//!   workload's virtual loopback network. The SSE state and connections
//!   live in the service; this invocation only moves bytes between the
//!   wash HTTP ingress and the service.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::{
    demo::jobs::control,
    exports::wasi::http::incoming_handler::Guest,
    wasi::http::types::{
        Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
    },
};
use wstd::io::{AsyncRead, AsyncWrite};

const SSE_SERVICE_ADDR: &str = "127.0.0.1:8081";

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let method = request.method();
        let path = request.path_with_query().unwrap_or_default();
        match (&method, path.as_str()) {
            (Method::Post, p) if p == "/create" || p.starts_with("/create?") => {
                // `POST /create?mode=burn` starts pure-CPU counters that are
                // cancellable only via epoch interruption (Layer 2).
                let burn = p.contains("mode=burn");
                match control::create(burn) {
                    Ok(id) => respond(response_out, 200, &format!("{id}\n")),
                    Err(e) => respond(response_out, 503, &format!("create failed: {e}\n")),
                }
            }
            (Method::Post, p) if p.starts_with("/cancel/") => {
                let id = &p["/cancel/".len()..];
                respond(response_out, 200, &format!("{}\n", control::cancel(id)));
            }
            (Method::Get, p) if p.starts_with("/events/") => {
                let id = p["/events/".len()..].to_string();
                proxy_events(&id, response_out);
            }
            _ => respond(
                response_out,
                404,
                "usage: POST /create | POST /cancel/<id> | GET /events/<id>\n",
            ),
        }
    }
}

/// Pipe the service's SSE byte stream for `request_id` into this response.
fn proxy_events(request_id: &str, response_out: ResponseOutparam) {
    let request_id = request_id.to_string();
    wstd::runtime::block_on(async move {
        let mut upstream = match wstd::net::TcpStream::connect(SSE_SERVICE_ADDR).await {
            Ok(stream) => stream,
            Err(e) => {
                return respond(response_out, 502, &format!("sse-service unreachable: {e}\n"));
            }
        };

        // Raw line protocol: register as a watcher for this group.
        if let Err(e) = upstream
            .write_all(format!("watch {request_id}\n").as_bytes())
            .await
        {
            return respond(response_out, 502, &format!("sse-service write failed: {e}\n"));
        }

        let headers = Fields::from_list(&[
            ("content-type".to_string(), b"text/event-stream".to_vec()),
            ("cache-control".to_string(), b"no-cache".to_vec()),
        ])
        .expect("valid headers");
        let response = OutgoingResponse::new(headers);
        response.set_status_code(200).unwrap();
        let outgoing_body = response.body().unwrap();
        ResponseOutparam::set(response_out, Ok(response));
        let stream = outgoing_body.write().unwrap();

        let mut buf = [0u8; 4096];
        loop {
            match upstream.read(&mut buf).await {
                // Service closed the stream (terminal event sent).
                Ok(0) => break,
                Ok(n) => {
                    // Client gone: drop the upstream connection so the
                    // service unsubscribes on its next write.
                    if stream.blocking_write_and_flush(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        drop(stream);
        OutgoingBody::finish(outgoing_body, None).unwrap();
    });
}

fn respond(response_out: ResponseOutparam, status: u16, body: &str) {
    let response = OutgoingResponse::new(Fields::new());
    response.set_status_code(status).unwrap();
    let outgoing_body = response.body().unwrap();
    ResponseOutparam::set(response_out, Ok(response));

    let stream = outgoing_body.write().unwrap();
    stream.blocking_write_and_flush(body.as_bytes()).unwrap();
    drop(stream);
    OutgoingBody::finish(outgoing_body, None).unwrap();
}

bindings::export!(Component with_types_in bindings);
