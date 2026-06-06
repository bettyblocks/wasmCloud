use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::iter::AsyncIterator;
use wstd::net::TcpListener;
use wstd::task::sleep;
use wstd::time::Duration;

/// Channel sender for pushing events to an SSE connection.
type EventSender = std::sync::mpsc::Sender<String>;

/// A registered SSE connection (metadata + event channel).
struct SseConnection {
    scope_key: String,
    stream_id: String,
    tx: EventSender,
}

type ConnectionRegistry = Arc<RwLock<HashMap<String, SseConnection>>>;

/// A stored message for replay support.
struct StoredMessage {
    id: u64,
    scope_key: String,
    data: String,
}

/// In-memory message store with monotonically increasing IDs.
/// Stores all messages for the lifetime of the service process.
struct MessageStore {
    messages: Vec<StoredMessage>,
    next_id: u64,
}

impl MessageStore {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            next_id: 1,
        }
    }

    fn append(&mut self, scope_key: &str, data: &str) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.messages.push(StoredMessage {
            id,
            scope_key: scope_key.to_string(),
            data: data.to_string(),
        });
        id
    }

    fn replay(&self, scope_key: &str, after_id: u64) -> Vec<&StoredMessage> {
        self.messages
            .iter()
            .filter(|m| m.scope_key == scope_key && m.id > after_id)
            .collect()
    }
}

type SharedMessageStore = Arc<RwLock<MessageStore>>;

#[derive(Deserialize)]
struct PushRequest {
    /// Push to a specific stream by ID.
    #[serde(default)]
    stream_id: Option<String>,
    /// Push to all streams matching a scope key.
    #[serde(default)]
    scope_key: Option<String>,
    /// The event data to send.
    data: String,
}

#[derive(Serialize)]
struct ConnectionInfo {
    stream_id: String,
    scope_key: String,
}

#[wstd::main]
async fn main() -> Result<()> {
    let bind_addr = std::env::var("SSE_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9090".into());

    let connections: ConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));
    let message_store: SharedMessageStore = Arc::new(RwLock::new(MessageStore::new()));

    let listener = TcpListener::bind(&bind_addr).await?;
    eprintln!("SSE component listening on {bind_addr}");

    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };

        let conns = Arc::clone(&connections);
        let store = Arc::clone(&message_store);
        wstd::runtime::spawn(async move {
            if let Err(e) = handle_connection(stream, conns, store).await {
                eprintln!("connection error: {e}");
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    Ok(())
}

async fn handle_connection(
    stream: wstd::net::TcpStream,
    connections: ConnectionRegistry,
    message_store: SharedMessageStore,
) -> Result<()> {
    // Read the HTTP request headers
    let mut buf = [0u8; 4096];
    let mut request_data = Vec::new();
    let (reader, writer) = stream.split();
    let mut reader = reader;
    let mut writer = writer;

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

    // Parse the HTTP request manually (avoid httparse dependency in wasm)
    let header_str = std::str::from_utf8(&request_data)
        .context("non-UTF8 request")?;
    let mut lines = header_str.split("\r\n");

    // Parse request line: "GET /path HTTP/1.1"
    let request_line = lines.next().context("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    // Parse headers
    let mut accept = String::new();
    let mut content_length: usize = 0;
    let mut last_event_id: Option<u64> = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(": ") {
            let name_lower = name.to_ascii_lowercase();
            if name_lower == "accept" {
                accept = value.to_string();
            } else if name_lower == "content-length" {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name_lower == "last-event-id" {
                last_event_id = Some(value.trim().parse().unwrap_or(0));
            }
        }
    }

    let is_sse = accept.contains("text/event-stream");

    // Find where the body starts
    let header_end = request_data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(request_data.len());

    match (method, is_sse) {
        ("GET", true) => {
            let scope_key = path.trim_start_matches('/').to_string();
            handle_sse_connection(
                &mut reader,
                &mut writer,
                scope_key,
                connections,
                message_store,
                last_event_id,
            )
            .await
        }
        ("POST", _) if path == "/push" => {
            // Read remaining body if needed
            let mut body_data = request_data[header_end..].to_vec();
            while body_data.len() < content_length {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                body_data.extend_from_slice(&buf[..n]);
            }
            handle_push(&mut writer, &body_data, connections, message_store).await
        }
        ("GET", false) if path.starts_with("/connections") => {
            let scope_key = path
                .strip_prefix("/connections/")
                .unwrap_or("")
                .to_string();
            handle_list_connections(&mut writer, &scope_key, connections).await
        }
        _ => {
            let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
            writer.write_all(response.as_bytes()).await?;
            writer.flush().await?;
            Ok(())
        }
    }
}

async fn handle_sse_connection<'a>(
    _reader: &mut wstd::net::ReadHalf<'a>,
    writer: &mut wstd::net::WriteHalf<'a>,
    scope_key: String,
    connections: ConnectionRegistry,
    message_store: SharedMessageStore,
    last_event_id: Option<u64>,
) -> Result<()> {
    let stream_id = generate_stream_id();

    // Send SSE response headers
    let headers = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         X-SSE-Stream-Id: {stream_id}\r\n\
         \r\n"
    );
    writer.write_all(headers.as_bytes()).await?;
    writer.flush().await?;

    eprintln!("SSE connection opened: stream_id={stream_id}, scope={scope_key}");

    // Replay stored messages if client sent Last-Event-ID header.
    // Last-Event-ID: 0 replays all messages; Last-Event-ID: N replays after N.
    if let Some(after_id) = last_event_id {
        let store = message_store.read().unwrap();
        let replayed = store.replay(&scope_key, after_id);
        eprintln!(
            "Replaying {} messages for scope={scope_key} after id={after_id}",
            replayed.len()
        );
        for msg in replayed {
            let line = format!("id: {}\ndata: {}\n\n", msg.id, msg.data);
            if writer.write_all(line.as_bytes()).await.is_err() {
                return Ok(());
            }
        }
        writer.flush().await?;
    }

    // Create a channel for receiving events
    let (tx, rx) = std::sync::mpsc::channel::<String>();

    // Register connection
    {
        let mut conns = connections.write().unwrap();
        conns.insert(
            stream_id.clone(),
            SseConnection {
                scope_key,
                stream_id: stream_id.clone(),
                tx,
            },
        );
    }

    // Event loop: check for events and send keepalives
    let mut keepalive_counter = 0u32;
    loop {
        // Check for events (non-blocking)
        match rx.try_recv() {
            Ok(event_data) => {
                // event_data is pre-formatted with "id: ...\ndata: ..."
                let msg = format!("{event_data}\n\n");
                if writer.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
                keepalive_counter = 0;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        // Send keepalive every ~30 seconds (sleep 100ms per iteration, 300 iterations = 30s)
        keepalive_counter += 1;
        if keepalive_counter >= 300 {
            if writer.write_all(b":keepalive\n\n").await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
            keepalive_counter = 0;
        }

        // Sleep briefly to avoid busy-waiting, then check for events again.
        // We rely on write failures to detect disconnection since wstd
        // doesn't support non-blocking reads.
        sleep(Duration::from_millis(100)).await;
    }

    // Cleanup: remove from registry
    {
        let mut conns = connections.write().unwrap();
        conns.remove(&stream_id);
    }
    eprintln!("SSE connection closed: stream_id={stream_id}");
    Ok(())
}

async fn handle_push(
    writer: &mut wstd::net::WriteHalf<'_>,
    body: &[u8],
    connections: ConnectionRegistry,
    message_store: SharedMessageStore,
) -> Result<()> {
    let push_req: PushRequest =
        serde_json::from_slice(body).context("invalid JSON body")?;

    // Determine scope_key for storage
    let scope_key_for_store = if let Some(ref sk) = push_req.scope_key {
        sk.clone()
    } else if let Some(ref stream_id) = push_req.stream_id {
        let conns = connections.read().unwrap();
        conns
            .get(stream_id)
            .map(|c| c.scope_key.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Store the message and get its ID
    let event_id = {
        let mut store = message_store.write().unwrap();
        store.append(&scope_key_for_store, &push_req.data)
    };

    // Format as SSE event with id
    let formatted_event = format!("id: {event_id}\ndata: {}", push_req.data);
    let mut sent_count = 0u32;
    let mut failed_ids: Vec<String> = Vec::new();

    {
        let conns = connections.read().unwrap();

        let targets: Vec<&SseConnection> = if let Some(ref stream_id) = push_req.stream_id {
            conns.get(stream_id).into_iter().collect()
        } else if let Some(ref scope_key) = push_req.scope_key {
            conns.values().filter(|c| c.scope_key == *scope_key).collect()
        } else {
            vec![]
        };

        for conn in targets {
            match conn.tx.send(formatted_event.clone()) {
                Ok(()) => sent_count += 1,
                Err(_) => failed_ids.push(conn.stream_id.clone()),
            }
        }
    }

    // Remove dead connections
    if !failed_ids.is_empty() {
        let mut conns = connections.write().unwrap();
        for id in &failed_ids {
            conns.remove(id);
        }
        eprintln!("Removed {} dead connections", failed_ids.len());
    }

    let response_body = serde_json::json!({
        "sent": sent_count,
        "failed": failed_ids.len(),
    });
    let response_body = response_body.to_string();
    let status = if sent_count > 0 { "200 OK" } else { "404 Not Found" };
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {response_body}",
        response_body.len()
    );
    writer.write_all(response.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn handle_list_connections(
    writer: &mut wstd::net::WriteHalf<'_>,
    scope_key: &str,
    connections: ConnectionRegistry,
) -> Result<()> {
    let infos: Vec<ConnectionInfo> = {
        let conns = connections.read().unwrap();
        conns
            .values()
            .filter(|c| scope_key.is_empty() || c.scope_key == scope_key)
            .map(|c| ConnectionInfo {
                stream_id: c.stream_id.clone(),
                scope_key: c.scope_key.clone(),
            })
            .collect()
    };

    let body = serde_json::to_string(&infos)?;
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    );
    writer.write_all(response.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Generate a unique stream ID using an atomic counter.
fn generate_stream_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("sse-conn-{id}")
}
