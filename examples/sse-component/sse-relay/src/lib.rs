use anyhow::{Context as _, Result};
use wstd::http::{Body, Client, Method, Request, Response, StatusCode};

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>> {
    let path = req.uri().path().to_string();

    match (req.method().clone(), path.as_str()) {
        (Method::POST, "/chat") => handle_chat(&mut req).await,
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Not Found\n".into())
            .map_err(Into::into),
    }
}

async fn handle_chat(req: &mut Request<Body>) -> Result<Response<Body>> {
    eprintln!("[sse-relay] handle_chat called");

    let api_key =
        std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY environment variable not set")?;
    eprintln!("[sse-relay] got API key (len={})", api_key.len());

    // Read the incoming request body to extract the prompt
    let body_bytes = req.body_mut().contents().await.unwrap_or(&[]);
    let prompt = extract_prompt(body_bytes);
    eprintln!("[sse-relay] prompt: {prompt}");

    // Build the upstream request body (OpenAI Responses API format)
    let upstream_body = serde_json::json!({
        "model": "gpt-4o",
        "input": prompt,
        "stream": true
    });

    // Build the outgoing HTTPS request
    let upstream_req = Request::builder()
        .method(Method::POST)
        .uri("https://api.openai.com/v1/responses")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {api_key}"))
        .body(Body::from(upstream_body.to_string()))?;

    eprintln!("[sse-relay] sending request to OpenAI...");

    // Send via wstd HTTP client (TLS handled by the wasmCloud runtime)
    let client = Client::new();
    let upstream_resp = client
        .send(upstream_req)
        .await
        .context("failed to send request to OpenAI API")?;

    eprintln!("[sse-relay] got response: status={}", upstream_resp.status());

    if !upstream_resp.status().is_success() {
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(format!("OpenAI API returned status {}", upstream_resp.status()).into())
            .map_err(Into::into);
    }

    // Pass the streaming body directly through — the Body from client.send()
    // is already a lazy WASI input stream, so SSE events flow incrementally.
    let (_parts, body) = upstream_resp.into_parts();

    eprintln!("[sse-relay] streaming response back to client");

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(body)
        .map_err(Into::into)
}

fn extract_prompt(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("prompt")?.as_str().map(String::from))
        .unwrap_or_else(|| "Hello".to_string())
}
