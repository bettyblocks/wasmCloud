use anyhow::{Context as _, Result};
use wstd::http::{Body, Request, Response, StatusCode};
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::net::TcpStream;

const SSE_SERVICE_ADDR: &str = "127.0.0.1:9090";

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>> {
    let path = req.uri().path().to_string();
    let is_sse = req
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    match (req.method().clone(), is_sse) {
        (wstd::http::Method::GET, true) => proxy_sse_stream(&path).await,
        (wstd::http::Method::POST, _) if path == "/push" => proxy_request(req, &path).await,
        (wstd::http::Method::GET, _) if path.starts_with("/connections") => {
            proxy_request(req, &path).await
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Not Found\n".into())
            .map_err(Into::into),
    }
}

/// Proxy an SSE request to the SSE service and stream the response back.
async fn proxy_sse_stream(path: &str) -> Result<Response<Body>> {
    let mut stream = TcpStream::connect(SSE_SERVICE_ADDR)
        .await
        .context("failed to connect to SSE service")?;

    // Forward the SSE request (write first, then read)
    let raw_request = format!(
        "GET {path} HTTP/1.1\r\n\
         Accept: text/event-stream\r\n\
         Host: localhost\r\n\
         \r\n"
    );
    stream.write_all(raw_request.as_bytes()).await?;
    stream.flush().await?;

    // Read response headers from the SSE service
    let mut header_buf = Vec::with_capacity(4096);
    let mut buf = [0u8; 1];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("SSE service closed connection before sending headers");
        }
        header_buf.push(buf[0]);
        if header_buf.len() >= 4 && header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if header_buf.len() > 8192 {
            anyhow::bail!("SSE service response headers too large");
        }
    }

    // Parse X-SSE-Stream-Id from headers
    let header_str = std::str::from_utf8(&header_buf).unwrap_or("");
    let stream_id = header_str
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("x-sse-stream-id:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();

    // Create a streaming body that reads from the SSE service TCP connection.
    // unfold takes ownership of the TcpStream and yields chunks lazily.
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
        .header("X-SSE-Stream-Id", &stream_id)
        .body(body)?;

    Ok(response)
}

/// Proxy a non-streaming request (push, list connections) to the SSE service.
async fn proxy_request(mut req: Request<Body>, path: &str) -> Result<Response<Body>> {
    let method = req.method().to_string();
    let mut stream = TcpStream::connect(SSE_SERVICE_ADDR)
        .await
        .context("failed to connect to SSE service")?;

    // Read request body if present
    let body_bytes = req.body_mut().contents().await.unwrap_or(&[]);

    // Forward the request
    let raw_request = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n",
        body_bytes.len()
    );
    stream.write_all(raw_request.as_bytes()).await?;
    if !body_bytes.is_empty() {
        stream.write_all(&body_bytes).await?;
    }
    stream.flush().await?;

    // Read the full response
    let mut response_data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        response_data.extend_from_slice(&buf[..n]);
        // Check if we have complete headers + body
        if let Some(header_end) = response_data.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_str = std::str::from_utf8(&response_data[..header_end]).unwrap_or("");
            let content_length: usize = header_str
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split_once(':'))
                .and_then(|(_, v)| v.trim().parse().ok())
                .unwrap_or(0);
            let body_start = header_end + 4;
            if response_data.len() >= body_start + content_length {
                break;
            }
        }
    }

    // Parse the response
    let header_end = response_data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response_data.len());
    let header_str = std::str::from_utf8(&response_data[..header_end]).unwrap_or("");

    let status: u16 = header_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(502);

    let body_start = header_end + 4;
    let body = if body_start < response_data.len() {
        String::from_utf8_lossy(&response_data[body_start..]).to_string()
    } else {
        String::new()
    };

    let content_type = header_str
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| "application/json".to_string());

    Response::builder()
        .status(status)
        .header("Content-Type", &content_type)
        .body(body.into())
        .map_err(Into::into)
}
