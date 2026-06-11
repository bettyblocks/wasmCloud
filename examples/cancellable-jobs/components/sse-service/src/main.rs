//! SSE service: the long-lived service component that owns both sides of
//! the event flow — counters feed it, clients watch it. Components stay
//! short-lived; this service is the only long-running piece.
//!
//! Listens on the workload's VIRTUAL loopback (guests cannot bind real
//! host ports); both peers are guests of the same workload:
//!
//!   counter  -> "feed <request-id> <index>\n"  then "count <n>" lines,
//!               "done" on completion. An abrupt close without "done"
//!               means the invocation was cancelled (trapped).
//!   frontend -> "watch <request-id>\n" (piping for a client's
//!               GET /events/<id>); receives SSE frames until the group
//!               ends: every live counter's counts as they arrive, then
//!               "event: done" or "event: cancelled".
//!
//! Single-threaded: wstd tasks share state via Rc<RefCell<..>>; RefCell
//! borrows are never held across an await.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wstd::io::{AsyncRead, AsyncWrite};
use wstd::iter::AsyncIterator;
use wstd::net::{TcpListener, TcpStream};

const LISTEN_ADDR: &str = "127.0.0.1:8081";

#[derive(Default)]
struct Group {
    seen_feeds: u32,
    active_feeds: u32,
    any_aborted: bool,
    terminal: Option<&'static str>,
    watchers: Vec<TcpStream>,
}

type Groups = Rc<RefCell<HashMap<String, Group>>>;

fn main() -> Result<(), std::io::Error> {
    wstd::runtime::block_on(async {
        let listener = TcpListener::bind(LISTEN_ADDR).await?;
        eprintln!("[sse-service] listening on {LISTEN_ADDR} (virtual loopback)");

        let groups: Groups = Rc::default();

        let mut incoming = listener.incoming();
        while let Some(stream) = incoming.next().await {
            match stream {
                Ok(stream) => {
                    let groups = groups.clone();
                    wstd::runtime::spawn(async move {
                        if let Err(e) = handle_connection(stream, groups).await {
                            eprintln!("[sse-service] connection ended: {e}");
                        }
                    })
                    .detach();
                }
                Err(e) => eprintln!("[sse-service] accept failed: {e}"),
            }
        }
        Ok(())
    })
}

async fn handle_connection(mut stream: TcpStream, groups: Groups) -> Result<(), std::io::Error> {
    let hello = read_line(&mut stream).await?;
    let mut parts = hello.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("feed"), Some(request_id), Some(index)) => {
            let index: u32 = index.parse().unwrap_or(u32::MAX);
            handle_feed(stream, groups, request_id.to_string(), index).await
        }
        (Some("watch"), Some(request_id), None) => {
            handle_watch(stream, groups, request_id.to_string()).await
        }
        _ => {
            stream.write_all(b"bad hello\n").await?;
            Ok(())
        }
    }
}

/// Ingest one counter's feed until it completes ("done") or dies abruptly
/// (cancelled/trapped). When the group's last feed ends, emit the terminal
/// event to all watchers and close them.
async fn handle_feed(
    mut stream: TcpStream,
    groups: Groups,
    request_id: String,
    index: u32,
) -> Result<(), std::io::Error> {
    {
        let mut groups = groups.borrow_mut();
        let group = groups.entry(request_id.clone()).or_default();
        group.seen_feeds += 1;
        group.active_feeds += 1;
    }
    eprintln!("[sse-service] feed connected: {request_id} #{index}");

    let mut completed = false;
    loop {
        let line = match read_line(&mut stream).await {
            Ok(line) if line.is_empty() => break, // EOF
            Ok(line) => line,
            Err(_) => break, // abrupt close (trapped counter)
        };
        if line == "done" {
            completed = true;
            break;
        }
        if let Some(count) = line.strip_prefix("count ") {
            let frame = format!("event: count\ndata: {{\"index\":{index},\"count\":{count}}}\n\n");
            broadcast(&groups, &request_id, &frame, false).await;
        }
    }

    let group_ended = {
        let mut groups = groups.borrow_mut();
        let Some(group) = groups.get_mut(&request_id) else {
            return Ok(());
        };
        group.active_feeds -= 1;
        if !completed {
            group.any_aborted = true;
        }
        if group.active_feeds == 0 && group.terminal.is_none() {
            group.terminal = Some(if group.any_aborted { "cancelled" } else { "done" });
            group.terminal
        } else {
            None
        }
    };

    if let Some(terminal) = group_ended {
        eprintln!("[sse-service] group {request_id}: {terminal}");
        let frame = format!("event: {terminal}\ndata: {{}}\n\n");
        broadcast(&groups, &request_id, &frame, true).await;
    }

    Ok(())
}

/// Register a watcher: it receives frames as feeds produce them. The
/// stream's ownership moves into the group; a watcher of an already-ended
/// group gets the terminal frame immediately.
async fn handle_watch(
    mut stream: TcpStream,
    groups: Groups,
    request_id: String,
) -> Result<(), std::io::Error> {
    let terminal = {
        let groups = groups.borrow();
        groups.get(&request_id).and_then(|g| g.terminal)
    };

    if let Some(terminal) = terminal {
        let frame = format!("event: {terminal}\ndata: {{}}\n\n");
        stream.write_all(frame.as_bytes()).await?;
        stream.flush().await?;
        return Ok(());
    }

    // Group may not have started yet (watcher raced the counters): create
    // it so the watcher is registered either way.
    let mut groups = groups.borrow_mut();
    let group = groups.entry(request_id).or_default();
    group.watchers.push(stream);
    Ok(())
}

/// Write a frame to every watcher of the group; drop watchers whose client
/// went away. With `close`, all watchers are dropped after the write
/// (terminal event).
async fn broadcast(groups: &Groups, request_id: &str, frame: &str, close: bool) {
    let watchers = {
        let mut groups = groups.borrow_mut();
        match groups.get_mut(request_id) {
            Some(group) => std::mem::take(&mut group.watchers),
            None => return,
        }
    };

    let mut kept = Vec::new();
    for mut watcher in watchers {
        let alive = watcher.write_all(frame.as_bytes()).await.is_ok()
            && watcher.flush().await.is_ok();
        if alive && !close {
            kept.push(watcher);
        }
    }

    if !close {
        let mut groups = groups.borrow_mut();
        if let Some(group) = groups.get_mut(request_id) {
            // New watchers may have registered while we were writing.
            group.watchers.append(&mut kept);
        }
    }
}

async fn read_line(stream: &mut TcpStream) -> Result<String, std::io::Error> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await? {
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ => {
                line.push(byte[0]);
                if line.len() > 256 {
                    break;
                }
            }
        }
    }
    Ok(String::from_utf8_lossy(&line).trim().to_string())
}
