//! Async-submit fixture: returns its HTTP response immediately while a
//! spawned background task keeps running and makes outbound HTTP requests.
//!
//! The callback authority (host:port of the test server) is passed in the
//! request path: `GET /submit?cb=<authority>`. The background task POSTs to
//! `/done1`, awaits the (deliberately slow) response, then POSTs `/done2`.
//! The test asserts the client got the fixture's response long before
//! `/done2` lands — proving the store keeps being driven after task.return.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::http::client::send as outbound;
use bindings::wasi::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};

struct Component;

impl Handler for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let callback = path
            .split_once("cb=")
            .map(|(_, cb)| cb.to_string())
            .unwrap_or_default();

        wit_bindgen::spawn(async move {
            // First POST: the test server delays its response, keeping this
            // request in flight (tracked host work) well past our response.
            post(&callback, "/done1").await;
            // Second POST proves the store was still alive after the first
            // completed.
            post(&callback, "/done2").await;
        });

        let headers = Fields::new();
        let (mut tx, rx) = bindings::wit_stream::new();
        let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());
        wit_bindgen::spawn(async move {
            tx.write_all(b"submitted".to_vec()).await;
            drop(tx);
            let _ = trailers_tx.write(Ok(None)).await;
        });

        let (response, _result) = Response::new(headers, Some(rx), trailers_rx);
        Ok(response)
    }
}

async fn post(authority: &str, path: &str) {
    let headers = Fields::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());
    wit_bindgen::spawn(async move {
        let _ = trailers_tx.write(Ok(None)).await;
    });

    let (request, _result) = Request::new(headers, None, trailers_rx, None);
    let _ = request.set_method(&Method::Post);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some(authority));
    let _ = request.set_path_with_query(Some(path));

    let _ = outbound(request).await;
}

bindings::export!(Component with_types_in bindings);
