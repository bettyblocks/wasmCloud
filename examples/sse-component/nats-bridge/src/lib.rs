wit_bindgen::generate!({
    path: "../wit",
    world: "nats-bridge",
    with: {
        "wasmcloud:messaging/types@0.2.0": generate,
        "wasmcloud:messaging/consumer@0.2.0": generate,
    },
});

use crate::wasmcloud::messaging::types::BrokerMessage;
use serde_json::json;
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::net::TcpStream;

struct Component;
export!(Component);

const SSE_SERVICE_ADDR: &str = "127.0.0.1:9090";

impl exports::wasmcloud::messaging::handler::Guest for Component {
    fn handle_message(msg: BrokerMessage) -> Result<(), String> {
        eprintln!("nats-bridge: received message on subject={}", msg.subject);

        let payload = String::from_utf8(msg.body)
            .map_err(|e| format!("invalid UTF-8 in message body: {e}"))?;
        let subject = &msg.subject;

        // Build JSON push request using serde_json for safe escaping.
        // scope_key = NATS subject, so clients on GET /{subject} receive these events.
        let json_body = json!({
            "scope_key": subject,
            "data": payload,
        })
        .to_string();

        eprintln!("nats-bridge: pushing to SSE service: {json_body}");

        // POST to SSE service's /push endpoint
        wstd::runtime::block_on(async {
            let mut stream = TcpStream::connect(SSE_SERVICE_ADDR)
                .await
                .map_err(|e| format!("failed to connect to SSE service: {e}"))?;

            eprintln!("nats-bridge: connected to SSE service");

            let request = format!(
                "POST /push HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 \r\n\
                 {}",
                json_body.len(),
                json_body
            );
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|e| format!("failed to write to SSE service: {e}"))?;
            stream
                .flush()
                .await
                .map_err(|e| format!("failed to flush to SSE service: {e}"))?;

            // Read response (drain it, we don't need to parse)
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;

            eprintln!("nats-bridge: push complete");
            Ok(())
        })
    }
}
