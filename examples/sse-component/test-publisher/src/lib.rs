mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "test-publisher",
        generate_all,
    });
}

use bindings::wasmcloud::messaging::{consumer, types::BrokerMessage};
use serde::Deserialize;
use wstd::http::{Body, Request, Response, StatusCode};

#[derive(Deserialize)]
struct PublishRequest {
    subject: String,
    data: String,
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> anyhow::Result<Response<Body>> {
    eprintln!("test-publisher: received request {} {}", req.method(), req.uri().path());

    if req.method() != wstd::http::Method::POST || req.uri().path() != "/publish" {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Not Found\n".into())
            .map_err(Into::into);
    }

    let body_bytes = req.body_mut().contents().await.unwrap_or(&[]);
    let publish_req: PublishRequest = serde_json::from_slice(body_bytes)
        .map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?;

    eprintln!(
        "test-publisher: publishing to subject={} data={}",
        publish_req.subject, publish_req.data
    );

    let msg = BrokerMessage {
        subject: publish_req.subject.clone(),
        body: publish_req.data.into_bytes(),
        reply_to: None,
    };

    match consumer::publish(&msg) {
        Ok(()) => {
            eprintln!("test-publisher: published successfully");
            Response::builder()
                .status(StatusCode::OK)
                .body(format!("Published to {}\n", publish_req.subject).into())
                .map_err(Into::into)
        }
        Err(e) => {
            eprintln!("test-publisher: publish failed: {e}");
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!("Publish failed: {e}\n").into())
                .map_err(Into::into)
        }
    }
}
