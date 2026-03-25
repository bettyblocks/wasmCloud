use anyhow::{Context as _, Result};
use wstd::http::{Body, Method, Request, Response, StatusCode};
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::net::TcpStream;

const DEFAULT_UPSTREAM_ADDR: &str = "127.0.0.1:9091";

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
    let upstream_addr =
        std::env::var("UPSTREAM_ADDR").unwrap_or_else(|_| DEFAULT_UPSTREAM_ADDR.into());

    // Read the incoming request body to extract the prompt
    let body_bytes = req.body_mut().contents().await.unwrap_or(&[]);
    let prompt = extract_prompt(body_bytes);

    // Build the upstream request body
    let upstream_body = serde_json::json!({
        "model": "gpt-4o",
        "input": prompt,
        "stream": true
    });
    let upstream_body_str = upstream_body.to_string();

    // Connect to the mock/upstream service via TCP
    let mut stream = TcpStream::connect(&upstream_addr)
        .await
        .context("failed to connect to upstream service")?;

    // Send HTTP POST to /v1/responses
    let raw_request = format!(
        "POST /v1/responses HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {upstream_body_str}",
        upstream_body_str.len()
    );
    stream.write_all(raw_request.as_bytes()).await?;
    stream.flush().await?;

    // Read upstream response headers
    let mut header_buf = Vec::with_capacity(4096);
    let mut buf = [0u8; 1];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("upstream closed connection before sending headers");
        }
        header_buf.push(buf[0]);
        if header_buf.len() >= 4 && header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if header_buf.len() > 8192 {
            anyhow::bail!("upstream response headers too large");
        }
    }

    // Check for 200 status
    let header_str = std::str::from_utf8(&header_buf).unwrap_or("");
    let status: u16 = header_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if status != 200 {
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(format!("Upstream returned status {status}").into())
            .map_err(Into::into);
    }

    // Stream the upstream SSE body back to the caller
    let body_stream = futures_lite::stream::unfold(stream, |mut stream| async move {
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => Some((buf[..n].to_vec(), stream)),
        }
    });

    let body = Body::from_stream(body_stream);

    let response = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(body)?;

    Ok(response)
}

fn extract_prompt(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("prompt")?.as_str().map(String::from))
        .unwrap_or_else(|| "Hello".to_string())
}
