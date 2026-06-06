use anyhow::{Context as _, Result};
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::iter::AsyncIterator;
use wstd::net::TcpListener;
use wstd::task::sleep;
use wstd::time::Duration;

/// Hardcoded delta chunks to simulate streaming text.
const DELTA_CHUNKS: &[&str] = &["Hello", " from", " mock!"];

#[wstd::main]
async fn main() -> Result<()> {
    let bind_addr =
        std::env::var("MOCK_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9091".into());

    let listener = TcpListener::bind(&bind_addr).await?;
    eprintln!("Mock OpenAI listening on {bind_addr}");

    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };

        wstd::runtime::spawn(async move {
            if let Err(e) = handle_connection(stream).await {
                eprintln!("connection error: {e}");
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    Ok(())
}

async fn handle_connection(stream: wstd::net::TcpStream) -> Result<()> {
    let mut buf = [0u8; 4096];
    let mut request_data = Vec::new();
    let (mut reader, mut writer) = stream.split();

    // Read HTTP request headers
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("connection closed before complete request");
        }
        request_data.extend_from_slice(&buf[..n]);
        if request_data.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if request_data.len() > 8192 {
            anyhow::bail!("request too large");
        }
    }

    let header_str = std::str::from_utf8(&request_data).context("non-UTF8 request")?;
    let mut lines = header_str.split("\r\n");

    let request_line = lines.next().context("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    // Only accept POST /v1/responses
    if method != "POST" || path != "/v1/responses" {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
        writer.write_all(response.as_bytes()).await?;
        writer.flush().await?;
        return Ok(());
    }

    // Send SSE response headers
    let headers = "HTTP/1.1 200 OK\r\n\
                   Content-Type: text/event-stream\r\n\
                   Cache-Control: no-cache\r\n\
                   Connection: keep-alive\r\n\
                   \r\n";
    writer.write_all(headers.as_bytes()).await?;
    writer.flush().await?;

    // Event 1: response.created
    let created = serde_json::json!({
        "type": "response.created",
        "response": {
            "id": "resp_mock_001",
            "status": "in_progress"
        }
    });
    write_sse_event(&mut writer, "response.created", &created.to_string()).await?;

    // Events 2..N: response.output_text.delta
    let mut full_text = String::new();
    for (i, chunk) in DELTA_CHUNKS.iter().enumerate() {
        sleep(Duration::from_millis(50)).await;

        full_text.push_str(chunk);
        let delta = serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "item_001",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": i + 1,
            "delta": chunk
        });
        write_sse_event(&mut writer, "response.output_text.delta", &delta.to_string()).await?;
    }

    // Final event: response.completed
    sleep(Duration::from_millis(50)).await;
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "resp_mock_001",
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": full_text
                }]
            }]
        }
    });
    write_sse_event(&mut writer, "response.completed", &completed.to_string()).await?;

    Ok(())
}

async fn write_sse_event(
    writer: &mut wstd::net::WriteHalf<'_>,
    event_type: &str,
    data: &str,
) -> Result<()> {
    let msg = format!("event: {event_type}\ndata: {data}\n\n");
    writer.write_all(msg.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}
