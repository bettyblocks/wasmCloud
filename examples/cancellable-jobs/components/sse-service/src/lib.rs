//! SSE service: the long-lived service component that owns both sides of
//! the event flow — counters feed it, clients watch it. Components stay
//! short-lived; this service is the only long-running piece.
//!
//! P3 (WASI 0.3 / component-model async) component exporting
//! `wasi:cli/run`, executed pinned by the runtime's P3 service path. The
//! earlier WASI 0.2 (wstd) version hit a hard scheduling wall at 13
//! concurrent groups: the 0.2 poll ABI hands the host the FULL pollable
//! list on every wait, so per-turn cost grew with connection count and
//! the single-threaded reactor melted down quadratically (see the README
//! load-ceiling section). Under P3 each task waits on its own waitable —
//! there is no guest-side poll list — so that cost structure does not
//! exist.
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
//! Single-threaded: tasks share state via Rc<RefCell<..>>; pushes are
//! synchronous, so no RefCell borrow ever spans an await.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use bindings::exports::wasi::cli::run::Guest;
use bindings::wasi::clocks::monotonic_clock::wait_for;
use bindings::wasi::sockets::types::{
    IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket,
};
use futures::StreamExt;
use futures::channel::mpsc;
use wit_bindgen::{StreamReader, StreamResult};

const LISTEN_ADDR: IpSocketAddress = IpSocketAddress::Ipv4(Ipv4SocketAddress {
    port: 8081,
    address: (127, 0, 0, 1),
});
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

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), ()> {
        let listener = TcpSocket::create(IpAddressFamily::Ipv4).map_err(|_| ())?;
        listener.bind(LISTEN_ADDR).map_err(|_| ())?;
        let _ = listener.set_listen_backlog_size(1024);
        let mut accept = listener.listen().map_err(|_| ())?;
        eprintln!("[sse-service] listening on 127.0.0.1:8081 (virtual loopback, P3)");

        let groups: Groups = Rc::default();
        while let Some(socket) = accept.next().await {
            let groups = groups.clone();
            wit_bindgen::spawn(async move {
                handle_connection(socket, groups).await;
            });
        }
        Ok(())
    }
}

/// Buffered line reader over a P3 receive stream: reads in chunks, yields
/// complete lines. Returns None on stream end (a partial trailing line
/// counts as an abrupt close).
struct LineReader {
    rx: StreamReader<u8>,
    buf: Vec<u8>,
    start: usize,
}

impl LineReader {
    fn new(rx: StreamReader<u8>) -> Self {
        Self {
            rx,
            buf: Vec::with_capacity(READ_CHUNK),
            start: 0,
        }
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
            let (result, data) = self.rx.read(Vec::with_capacity(READ_CHUNK)).await;
            match result {
                StreamResult::Complete(_) => self.buf.extend_from_slice(&data),
                StreamResult::Dropped | StreamResult::Cancelled => return None,
            }
        }
    }
}

async fn handle_connection(socket: TcpSocket, groups: Groups) {
    let (rx, _recv_result) = socket.receive();
    let mut lines = LineReader::new(rx);
    let Some(hello) = lines.next_line().await else {
        return;
    };

    let mut parts = hello.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("feed"), Some(request_id), Some(index)) => {
            let index: u32 = index.parse().unwrap_or(u32::MAX);
            handle_feed(socket, lines, groups, request_id.to_string(), index).await;
        }
        (Some("watch"), Some(request_id), None) => {
            handle_watch(socket, lines, groups, request_id.to_string()).await;
        }
        _ => eprintln!("[sse-service] bad hello: {hello}"),
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
/// event to all watchers. Owns the socket for the connection's lifetime.
async fn handle_feed(
    socket: TcpSocket,
    mut lines: LineReader,
    groups: Groups,
    request_id: String,
    index: u32,
) {
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
    drop(socket);

    let maybe_finalize = {
        let mut groups = groups.borrow_mut();
        let Some(group) = groups.get_mut(&request_id) else {
            return;
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
        wait_for(2_000_000_000).await;
        let finalized = {
            let mut groups = groups.borrow_mut();
            let Some(group) = groups.get_mut(&request_id) else {
                return;
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
}

/// Serve one watcher: register a queue with the group and drain it into
/// the connection. The feed side never touches this socket — a slow
/// watcher only grows its own queue. Owns the socket (and the read half)
/// for the connection's lifetime.
async fn handle_watch(socket: TcpSocket, lines: LineReader, groups: Groups, request_id: String) {
    let mut rx = {
        let mut groups = groups.borrow_mut();
        let group = groups.entry(request_id.clone()).or_default();
        let (tx, rx) = mpsc::unbounded();
        if let Some(terminal) = group.terminal {
            // Group already ended: deliver the outcome immediately.
            let frame = format!("event: {terminal}\ndata: {{}}\n\n");
            let _ = tx.unbounded_send(Frame::Terminal(frame));
        } else {
            group.watchers.push(tx);
        }
        rx
    };

    let (mut tx, stream_rx) = bindings::wit_stream::new();
    futures::join!(
        async {
            let _ = socket.send(stream_rx).await;
        },
        async move {
            while let Some(frame) = rx.next().await {
                match frame {
                    Frame::Data(data) => {
                        let leftover = tx.write_all(data.into_bytes()).await;
                        if !leftover.is_empty() {
                            // Client went away; dropping rx prunes us from
                            // the group on the next push.
                            break;
                        }
                    }
                    Frame::Terminal(data) => {
                        let _ = tx.write_all(data.into_bytes()).await;
                        break;
                    }
                }
            }
            drop(tx);
        }
    );
    drop(lines);
}

bindings::export!(Component with_types_in bindings);
