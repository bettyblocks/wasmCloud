//! Batch mode: precompile many `<image> <output>` items in one process with one
//! wasmtime engine, one registry client and one NATS connection.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use url::Url;

use crate::failure::{self, EXIT_PERMANENT, EXIT_RETRYABLE, Permanent};
use crate::output::{self, Sink};
use crate::precompile;
use crate::pull::Puller;

/// The kubelet keeps at most 4096 bytes of a termination message.
pub const TERMINATION_MESSAGE_LIMIT: usize = 4096;

/// Longest error text kept per failed item in the termination message. Shorter
/// limits are tried when the message would not fit.
const ERROR_TEXT_LIMITS: [usize; 5] = [512, 256, 128, 48, 0];

/// One unit of work: pull `image`, precompile it, write it to `output`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub image: String,
    pub output: Url,
}

impl Item {
    pub fn new(image: &str, output: &str) -> Result<Self> {
        let output = Url::parse(output)
            .context(Permanent)
            .with_context(|| format!("invalid output URL '{output}'"))?;
        Ok(Self {
            image: image.to_string(),
            output,
        })
    }
}

/// Parses a batch file: one `<image> <output>` pair per line, separated by
/// whitespace. Blank lines and lines starting with `#` are skipped.
pub fn parse_batch(text: &str) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(image), Some(output), None) = (parts.next(), parts.next(), parts.next()) else {
            bail!("batch file line {}: expected `<image> <output>`", n + 1);
        };
        items.push(Item::new(image, output).with_context(|| format!("batch file line {}", n + 1))?);
    }
    Ok(items)
}

/// The result of one item.
#[derive(Debug)]
pub struct Outcome {
    pub index: usize,
    pub result: Result<()>,
    pub took: Duration,
}

/// The per-item results of a batch, in item order.
#[derive(Debug, Default)]
pub struct Report {
    pub total: usize,
    pub outcomes: Vec<Outcome>,
}

impl Report {
    fn failures(&self) -> impl Iterator<Item = (&Outcome, &anyhow::Error)> {
        self.outcomes
            .iter()
            .filter_map(|o| o.result.as_ref().err().map(|e| (o, e)))
    }

    pub fn failed_count(&self) -> usize {
        self.failures().count()
    }

    /// 0 when every item succeeded; [`EXIT_PERMANENT`] when every failure is
    /// permanent; [`EXIT_RETRYABLE`] otherwise.
    pub fn exit_code(&self) -> i32 {
        let mut failures = self.failures().peekable();
        if failures.peek().is_none() {
            0
        } else if failures.all(|(_, e)| failure::is_permanent(e)) {
            EXIT_PERMANENT
        } else {
            EXIT_RETRYABLE
        }
    }

    /// JSON for `/dev/termination-log`, at most [`TERMINATION_MESSAGE_LIMIT`]
    /// bytes. Error texts are shortened (and dropped as a last resort) to fit.
    pub fn termination_message(&self) -> String {
        let ok: Vec<usize> = self
            .outcomes
            .iter()
            .filter(|o| o.result.is_ok())
            .map(|o| o.index)
            .collect();
        let mut last = String::new();
        for limit in ERROR_TEXT_LIMITS {
            let summary = Summary {
                total: self.total,
                ok: &ok,
                failed: self
                    .failures()
                    .map(|(o, e)| FailedItem {
                        index: o.index,
                        permanent: failure::is_permanent(e),
                        error: truncate(&format!("{e:#}"), limit),
                    })
                    .collect(),
            };
            last = serde_json::to_string(&summary).unwrap_or_default();
            if last.len() <= TERMINATION_MESSAGE_LIMIT {
                return last;
            }
        }
        last
    }
}

/// The JSON written to the termination log. Item indexes are 0-based positions
/// in the item list of this process. An index missing from both `ok` and
/// `failed` has no result (only possible when the message was cut).
#[derive(Debug, Serialize)]
struct Summary<'a> {
    total: usize,
    ok: &'a [usize],
    failed: Vec<FailedItem>,
}

#[derive(Debug, Serialize)]
struct FailedItem {
    index: usize,
    permanent: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
}

/// JSON for the termination log when the batch could not start at all.
pub fn fatal_message(err: &anyhow::Error) -> String {
    #[derive(Serialize)]
    struct Fatal {
        error: String,
        permanent: bool,
    }
    serde_json::to_string(&Fatal {
        error: truncate(&format!("{err:#}"), 1024),
        permanent: failure::is_permanent(err),
    })
    .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", s.get(..end).unwrap_or_default())
}

/// Runs `work` for every item. At most `max_in_flight` items run at the same
/// time. A failed item does not stop the others. `on_done` is called as each
/// item finishes (for progress output).
pub async fn run_with<F, Fut>(
    items: &[Item],
    max_in_flight: usize,
    work: F,
    mut on_done: impl FnMut(&Item, &Outcome),
) -> Report
where
    F: Fn(Item) -> Fut,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let slots = Arc::new(Semaphore::new(max_in_flight.max(1)));
    let mut set = JoinSet::new();
    let mut task_index = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        let fut = work(item.clone());
        let slots = slots.clone();
        let handle = set.spawn(async move {
            let _slot = slots.acquire_owned().await?;
            let start = Instant::now();
            let result = fut.await;
            anyhow::Ok((result, start.elapsed()))
        });
        task_index.insert(handle.id(), index);
    }

    let mut outcomes = Vec::with_capacity(items.len());
    while let Some(joined) = set.join_next_with_id().await {
        let (id, result, took) = match joined {
            Ok((id, Ok((result, took)))) => (id, result, took),
            Ok((id, Err(e))) => (id, Err(e), Duration::ZERO),
            // A panic in one item must not hide the results of the others.
            Err(e) => (
                e.id(),
                Err(anyhow::anyhow!("item task failed: {e}")),
                Duration::ZERO,
            ),
        };
        let Some(&index) = task_index.get(&id) else {
            continue;
        };
        let outcome = Outcome {
            index,
            result,
            took,
        };
        if let Some(item) = items.get(index) {
            on_done(item, &outcome);
        }
        outcomes.push(outcome);
    }
    outcomes.sort_by_key(|o| o.index);
    Report {
        total: items.len(),
        outcomes,
    }
}

/// Shared state for the real pull -> compile -> write pipeline.
pub struct Pipeline {
    engine: wasmtime::Engine,
    puller: Puller,
    sink: Sink,
    /// Bounds the CPU-heavy compiles; pulls and writes are not limited by it.
    compiles: Semaphore,
}

impl Pipeline {
    pub fn new(engine: wasmtime::Engine, compile_concurrency: usize) -> Self {
        Self {
            engine,
            puller: Puller::from_env(),
            sink: Sink::default(),
            compiles: Semaphore::new(compile_concurrency.max(1)),
        }
    }

    pub async fn process(self: Arc<Self>, item: Item) -> Result<()> {
        // A bad output URL will not get better; fail before the pull.
        output::validate(&item.output).context(Permanent)?;
        let wasm = self.puller.fetch(&item.image).await?;
        let cwasm = {
            let _permit = self.compiles.acquire().await?;
            let engine = self.engine.clone();
            tokio::task::spawn_blocking(move || precompile::compile_with(&engine, &wasm))
                .await
                .context("compile task failed")?
                // Wasm that does not compile will not compile on a retry either.
                .context(Permanent)?
        };
        self.sink.write(&item.output, &cwasm).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn item(n: usize) -> Item {
        Item::new(&format!("r/c{n}:1"), &format!("file:///tmp/c{n}.cwasm")).unwrap()
    }

    fn report(results: Vec<Result<()>>) -> Report {
        Report {
            total: results.len(),
            outcomes: results
                .into_iter()
                .enumerate()
                .map(|(index, result)| Outcome {
                    index,
                    result,
                    took: Duration::ZERO,
                })
                .collect(),
        }
    }

    fn permanent(msg: &str) -> Result<()> {
        Err(anyhow!(msg.to_string())).context(Permanent)
    }

    #[test]
    fn parses_batch_file_text() {
        let items = parse_batch("# c\n\nr/a:1 nats://b/k1\n  r/b:1   file:///x \n").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].image, "r/a:1");
        assert_eq!(items[0].output.as_str(), "nats://b/k1");
        assert_eq!(items[1].output.as_str(), "file:///x");
    }

    #[test]
    fn batch_file_rejects_bad_lines() {
        let err = parse_batch("r/a:1 nats://b/k\nonly-one-field\n").unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err:#}");
        assert!(parse_batch("a b c\n").is_err());
        assert!(failure::is_permanent(
            &parse_batch("r/a:1 not-a-url\n").unwrap_err()
        ));
    }

    #[tokio::test]
    async fn continues_after_a_failed_item() {
        let items: Vec<Item> = (0..5).map(item).collect();
        let mut seen = Vec::new();
        let report = run_with(
            &items,
            2,
            |item| async move {
                if item.image == "r/c2:1" {
                    Err(anyhow!("boom"))
                } else {
                    Ok(())
                }
            },
            |item, _| seen.push(item.image.clone()),
        )
        .await;

        assert_eq!(report.total, 5);
        assert_eq!(seen.len(), 5);
        let indexes: Vec<usize> = report.outcomes.iter().map(|o| o.index).collect();
        assert_eq!(indexes, vec![0, 1, 2, 3, 4]);
        assert_eq!(report.failed_count(), 1);
        assert!(report.outcomes[2].result.is_err());
        assert_eq!(report.exit_code(), EXIT_RETRYABLE);
    }

    #[tokio::test]
    async fn never_runs_more_than_max_in_flight() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let items: Vec<Item> = (0..12).map(item).collect();
        let report = run_with(
            &items,
            3,
            |_| {
                let (running, peak) = (running.clone(), peak.clone());
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            |_, _| {},
        )
        .await;
        assert_eq!(report.failed_count(), 0);
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }

    #[test]
    fn exit_code_reflects_the_kind_of_failure() {
        assert_eq!(report(vec![Ok(()), Ok(())]).exit_code(), 0);
        assert_eq!(
            report(vec![Ok(()), permanent("invalid wasm")]).exit_code(),
            EXIT_PERMANENT
        );
        assert_eq!(
            report(vec![permanent("not found"), Err(anyhow!("nats down"))]).exit_code(),
            EXIT_RETRYABLE
        );
    }

    #[test]
    fn termination_message_lists_ok_and_failed_items() {
        let msg = report(vec![
            Ok(()),
            permanent("invalid wasm"),
            Ok(()),
            Err(anyhow!("timeout")),
        ])
        .termination_message();
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["total"], 4);
        assert_eq!(v["ok"], serde_json::json!([0, 2]));
        assert_eq!(v["failed"][0]["index"], 1);
        assert_eq!(v["failed"][0]["permanent"], true);
        assert!(
            v["failed"][0]["error"]
                .as_str()
                .unwrap()
                .contains("invalid wasm")
        );
        assert_eq!(v["failed"][1]["index"], 3);
        assert_eq!(v["failed"][1]["permanent"], false);
    }

    #[test]
    fn termination_message_fits_the_kubelet_limit() {
        let long = "x".repeat(3000);
        let results = (0..60)
            .map(|n| {
                if n % 2 == 0 {
                    Ok(())
                } else {
                    Err(anyhow!(long.clone()))
                }
            })
            .collect();
        let msg = report(results).termination_message();
        assert!(msg.len() <= TERMINATION_MESSAGE_LIMIT, "{}", msg.len());
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["ok"].as_array().unwrap().len(), 30);
        assert_eq!(v["failed"].as_array().unwrap().len(), 30);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 2), "h...");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn fatal_message_is_json() {
        let err = Err::<(), _>(anyhow!("bad file"))
            .context(Permanent)
            .unwrap_err();
        let v: serde_json::Value = serde_json::from_str(&fatal_message(&err)).unwrap();
        assert_eq!(v["permanent"], true);
    }
}
