//! SSE service: the long-lived service component that owns the
//! client-facing event streams, per the principle that per-request
//! components stay short-lived and anything long-lived runs as a service.
//!
//! Serves a raw line protocol on the workload's virtual loopback network
//! (guests cannot bind real host ports): the frontend's `/events/<id>`
//! route connects, sends `<request-id>\n`, and receives SSE frames until a
//! terminal event (cancelled/done) closes the stream. Events come from the
//! JobsPlugin via the non-blocking `demo:jobs/events` import, polled every
//! 100ms per connection.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::demo::jobs::events::{Event, poll_next, subscribe, unsubscribe};
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::iter::AsyncIterator;
use wstd::net::{TcpListener, TcpStream};
use wstd::time::Duration;

const LISTEN_ADDR: &str = "127.0.0.1:8081";
const KEEPALIVE_EVERY_TICKS: u32 = 100; // ~10s of inactivity

fn poll_interval() -> Duration {
    Duration::from_millis(100)
}

#[wstd::main]
async fn main() -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    eprintln!("[sse-service] listening on {LISTEN_ADDR} (virtual loopback)");

    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        match stream {
            Ok(stream) => {
                wstd::runtime::spawn(async move {
                    if let Err(e) = handle_connection(stream).await {
                        eprintln!("[sse-service] connection ended: {e}");
                    }
                })
                .detach();
            }
            Err(e) => eprintln!("[sse-service] accept failed: {e}"),
        }
    }

    Ok(())
}

async fn handle_connection(mut stream: TcpStream) -> Result<(), std::io::Error> {
    let request_id = read_line(&mut stream).await?;

    let subscription = match subscribe(&request_id) {
        Ok(subscription) => subscription,
        Err(e) => {
            stream
                .write_all(format!("event: error\ndata: {{\"error\":\"{e}\"}}\n\n").as_bytes())
                .await?;
            return Ok(());
        }
    };
    eprintln!("[sse-service] streaming {request_id} (subscription {subscription})");

    let result = stream_events(&mut stream, subscription).await;
    // On a client-side error the subscription may still be live; drop it.
    unsubscribe(subscription);
    result
}

async fn stream_events(stream: &mut TcpStream, subscription: u64) -> Result<(), std::io::Error> {
    let mut idle_ticks = 0u32;
    loop {
        let mut wrote = false;

        // Drain everything currently pending for this subscription.
        while let Some(event) = poll_next(subscription) {
            let (frame, terminal) = render(&event);
            stream.write_all(frame.as_bytes()).await?;
            wrote = true;
            if terminal {
                stream.flush().await?;
                return Ok(());
            }
        }

        if wrote {
            stream.flush().await?;
            idle_ticks = 0;
        } else {
            idle_ticks += 1;
            if idle_ticks >= KEEPALIVE_EVERY_TICKS {
                // SSE comment line: keeps the connection warm and lets us
                // notice a disconnected client even when nothing happens.
                stream.write_all(b": keepalive\n\n").await?;
                stream.flush().await?;
                idle_ticks = 0;
            }
        }

        wstd::task::sleep(poll_interval()).await;
    }
}

fn render(event: &Event) -> (String, bool) {
    match event {
        Event::Count(data) => (
            format!(
                "event: count\ndata: {{\"index\":{},\"count\":{}}}\n\n",
                data.index, data.count
            ),
            false,
        ),
        Event::Cancelled => ("event: cancelled\ndata: {}\n\n".to_string(), true),
        Event::Done => ("event: done\ndata: {}\n\n".to_string(), true),
    }
}

async fn read_line(stream: &mut TcpStream) -> Result<String, std::io::Error> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 256 {
        match stream.read(&mut byte).await? {
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ => line.push(byte[0]),
        }
    }
    Ok(String::from_utf8_lossy(&line).trim().to_string())
}
