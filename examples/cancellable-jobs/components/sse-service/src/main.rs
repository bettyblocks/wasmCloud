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
//! Throughput design (shaped by load testing — see loadtest.sh):
//! - **Buffered line reads**: feeds are read in 4KiB chunks and split into
//!   lines, instead of one byte per poll.
//! - **Per-watcher queues**: a feed pushes frames into each watcher's
//!   queue synchronously and never awaits a watcher's socket. Each
//!   watcher connection drains its own queue in its own task, so a slow
//!   watcher can't backpressure the feeds (and thereby the counters,
//!   whose tick loops await their feed writes). Queues are unbounded —
//!   acceptable at demo scale; a slow client costs memory, not group
//!   throughput.
//!
//! Single-threaded: wstd tasks share state via Rc<RefCell<..>>; pushes
//! are synchronous, so no RefCell borrow ever spans an await.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc;
use wstd::io::{AsyncRead, AsyncWrite};
use wstd::iter::AsyncIterator;
use wstd::net::{TcpListener, TcpStream};

const LISTEN_ADDR: &str = "127.0.0.1:8081";
const READ_CHUNK: usize = 4096;

/// A frame queued for one watcher. Terminal frames close the connection
/// after delivery.
enum Frame {
    Data(String),
    Terminal(String),
}

#[derive(Default)]
struct Group {
    active_feeds: u32,
    any_aborted: bool,
    terminal: Option<&'static str>,
    watchers: Vec<mpsc::UnboundedSender<Frame>>,
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

/// Buffered line reader over a TcpStream: reads in chunks, yields complete
/// lines. Returns None on EOF or error (a partial trailing line counts as
/// an abrupt close).
struct LineReader {
    stream: TcpStream,
    buf: Vec<u8>,
    start: usize,
}

impl LineReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(READ_CHUNK),
            start: 0,
        }
    }

    fn into_stream(self) -> TcpStream {
        self.stream
    }

    async fn next_line(&mut self) -> Option<String> {
        loop {
            if let Some(nl) = self.buf[self.start..].iter().position(|&b| b == b'\n') {
                let end = self.start + nl;
                let line = String::from_utf8_lossy(&self.buf[self.start..end])
                    .trim()
                    .to_string();
                self.start = end + 1;
                return Some(line);
            }

            // No complete line buffered: compact, then read another chunk.
            if self.start > 0 {
                self.buf.drain(..self.start);
                self.start = 0;
            }
            let mut chunk = [0u8; READ_CHUNK];
            match self.stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }
}

async fn handle_connection(stream: TcpStream, groups: Groups) -> Result<(), std::io::Error> {
    let mut lines = LineReader::new(stream);
    let Some(hello) = lines.next_line().await else {
        return Ok(());
    };

    let mut parts = hello.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("feed"), Some(request_id), Some(index)) => {
            let index: u32 = index.parse().unwrap_or(u32::MAX);
            handle_feed(lines, groups, request_id.to_string(), index).await
        }
        (Some("watch"), Some(request_id), None) => {
            handle_watch(lines.into_stream(), groups, request_id.to_string()).await
        }
        _ => {
            let mut stream = lines.into_stream();
            stream.write_all(b"bad hello\n").await?;
            Ok(())
        }
    }
}

/// Push a data frame to every watcher of the group; dead watchers (queue
/// receiver dropped) are pruned. Synchronous — never blocks the feed.
fn push_data(groups: &Groups, request_id: &str, frame: &str) {
    let mut groups = groups.borrow_mut();
    let Some(group) = groups.get_mut(request_id) else {
        return;
    };
    group
        .watchers
        .retain(|tx| tx.unbounded_send(Frame::Data(frame.to_string())).is_ok());
}

/// Deliver the terminal frame to every watcher and drop them.
fn push_terminal(groups: &Groups, request_id: &str, terminal: &'static str) {
    let frame = format!("event: {terminal}\ndata: {{}}\n\n");
    let mut groups = groups.borrow_mut();
    let Some(group) = groups.get_mut(request_id) else {
        return;
    };
    for tx in group.watchers.drain(..) {
        let _ = tx.unbounded_send(Frame::Terminal(frame.clone()));
    }
}

/// Ingest one counter's feed until it completes ("done") or dies abruptly
/// (cancelled/trapped). When the group's last feed ends, emit the terminal
/// event to all watchers.
async fn handle_feed(
    mut lines: LineReader,
    groups: Groups,
    request_id: String,
    index: u32,
) -> Result<(), std::io::Error> {
    {
        let mut groups = groups.borrow_mut();
        let group = groups.entry(request_id.clone()).or_default();
        group.active_feeds += 1;
    }
    eprintln!("[sse-service] feed connected: {request_id} #{index}");

    let mut completed = false;
    while let Some(line) = lines.next_line().await {
        if line == "done" {
            completed = true;
            break;
        }
        if let Some(count) = line.strip_prefix("count ") {
            let frame = format!("event: count\ndata: {{\"index\":{index},\"count\":{count}}}\n\n");
            push_data(&groups, &request_id, &frame);
        }
    }

    let maybe_finalize = {
        let mut groups = groups.borrow_mut();
        let Some(group) = groups.get_mut(&request_id) else {
            return Ok(());
        };
        group.active_feeds -= 1;
        if !completed {
            group.any_aborted = true;
        }
        group.active_feeds == 0 && group.terminal.is_none()
    };

    if maybe_finalize {
        // Grace window: counters of one group register their feeds in a
        // staggered fashion, so "no active feeds" right now may just mean
        // the others have not connected yet. Only finalize if the group is
        // still quiet after the grace period.
        wstd::task::sleep(wstd::time::Duration::from_secs(2)).await;
        let finalized = {
            let mut groups = groups.borrow_mut();
            let Some(group) = groups.get_mut(&request_id) else {
                return Ok(());
            };
            if group.active_feeds == 0 && group.terminal.is_none() {
                group.terminal = Some(if group.any_aborted { "cancelled" } else { "done" });
                group.terminal
            } else {
                None
            }
        };
        if let Some(terminal) = finalized {
            eprintln!("[sse-service] group {request_id}: {terminal}");
            push_terminal(&groups, &request_id, terminal);
        }
    }

    Ok(())
}

/// Serve one watcher: register a queue with the group and drain it into
/// the connection. The feed side never touches this socket — a slow
/// watcher only grows its own queue.
async fn handle_watch(
    mut stream: TcpStream,
    groups: Groups,
    request_id: String,
) -> Result<(), std::io::Error> {
    let mut rx = {
        let mut groups = groups.borrow_mut();
        let group = groups.entry(request_id.clone()).or_default();
        if let Some(terminal) = group.terminal {
            // Group already ended: deliver the outcome immediately.
            let frame = format!("event: {terminal}\ndata: {{}}\n\n");
            drop(groups);
            stream.write_all(frame.as_bytes()).await?;
            stream.flush().await?;
            return Ok(());
        }
        let (tx, rx) = mpsc::unbounded();
        group.watchers.push(tx);
        rx
    };

    while let Some(frame) = rx.next().await {
        match frame {
            Frame::Data(data) => {
                if stream.write_all(data.as_bytes()).await.is_err()
                    || stream.flush().await.is_err()
                {
                    // Client went away; dropping rx prunes us from the
                    // group on the next push.
                    break;
                }
            }
            Frame::Terminal(data) => {
                let _ = stream.write_all(data.as_bytes()).await;
                let _ = stream.flush().await;
                break;
            }
        }
    }

    Ok(())
}
