use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use url::Url;

mod batch;
mod failure;
mod output;
mod precompile;
mod pull;

use batch::{Item, Pipeline};
use failure::{EXIT_PERMANENT, EXIT_RETRYABLE, Permanent};

/// Single mode (frozen CLI): `--image <ref> --output <url>`, exactly once.
///
/// Batch mode: repeat the pair (`--image A --output X --image B --output Y`,
/// paired by position) and/or pass `--batch-file <path>` with one
/// `<image> <output>` pair per line. All items share one wasmtime engine, one
/// registry client and one NATS connection. A failed item does not stop the
/// others. See the README for the output and the exit codes.
#[derive(Parser, Debug)]
#[command(name = "wash-precompile")]
struct Args {
    /// OCI reference to the source component (repeat for a batch; pairs with --output by position)
    #[arg(long)]
    image: Vec<String>,

    /// URL where the precompiled .cwasm bytes will be written (repeat for a batch)
    #[arg(long)]
    output: Vec<String>,

    /// File with one `<image> <output>` pair per line (blank lines and `#` comments are ignored)
    #[arg(long)]
    batch_file: Option<PathBuf>,

    /// Use batch mode (per-item lines, JSON summary, exit codes 0/1/2) also
    /// for a single `--image/--output` pair
    #[arg(long)]
    batch: bool,

    /// Batch mode: how many components to compile at the same time
    /// [default: the CPUs available to this process, cgroup-aware]
    #[arg(long, env = "PRECOMPILE_CONCURRENCY")]
    concurrency: Option<usize>,

    /// Batch mode: where to write the JSON summary that Kubernetes reports as
    /// the container's termination message
    #[arg(
        long,
        env = "PRECOMPILE_TERMINATION_LOG",
        default_value = "/dev/termination-log"
    )]
    termination_log: PathBuf,
}

impl Args {
    /// Exactly one `--image/--output` pair, no batch file and no `--batch`:
    /// the original CLI.
    fn is_single(&self) -> bool {
        !self.batch && self.batch_file.is_none() && self.image.len() == 1 && self.output.len() == 1
    }

    fn items(&self) -> Result<Vec<Item>> {
        if self.image.len() != self.output.len() {
            bail!(
                "--image given {} times but --output {} times",
                self.image.len(),
                self.output.len()
            );
        }
        let mut items = self
            .image
            .iter()
            .zip(&self.output)
            .map(|(image, output)| Item::new(image, output))
            .collect::<Result<Vec<_>>>()?;
        if let Some(path) = &self.batch_file {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read batch file {}", path.display()))?;
            items.extend(batch::parse_batch(&text)?);
        }
        if items.is_empty() {
            bail!("nothing to do: give --image/--output or --batch-file");
        }
        Ok(items)
    }

    fn compile_concurrency(&self) -> usize {
        self.concurrency
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            })
            .max(1)
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: failed to start the async runtime: {e}");
            return ExitCode::from(EXIT_RETRYABLE as u8);
        }
    };

    if args.is_single() {
        // Unchanged behaviour: the error goes to stderr and the exit code is 1.
        return match runtime.block_on(run_single(&args)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: {e:?}");
                ExitCode::FAILURE
            }
        };
    }

    let code = runtime.block_on(run_batch(&args));
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn run_single(args: &Args) -> Result<()> {
    let (Some(image), Some(output)) = (args.image.first(), args.output.first()) else {
        bail!("--image and --output are required");
    };
    let output_url = Url::parse(output)?;

    let wasm = pull::fetch(image).await?;
    let cwasm = precompile::compile_with(&precompile::engine()?, &wasm)?;
    output::Sink::default().write(&output_url, &cwasm).await?;
    Ok(())
}

async fn run_batch(args: &Args) -> i32 {
    let (items, engine) = match args
        .items()
        // An invalid batch is the same on every retry.
        .map_err(|e| e.context(Permanent))
        .and_then(|items| Ok((items, precompile::engine()?)))
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {e:#}");
            write_termination_log(&args.termination_log, &batch::fatal_message(&e));
            return if failure::is_permanent(&e) {
                EXIT_PERMANENT
            } else {
                EXIT_RETRYABLE
            };
        }
    };

    let concurrency = args.compile_concurrency();
    // Let pulls overlap with compiles, but keep the memory bounded.
    let max_in_flight = concurrency.saturating_mul(2);
    let pipeline = Arc::new(Pipeline::new(engine, concurrency));
    let start = Instant::now();

    let report = batch::run_with(
        &items,
        max_in_flight,
        |item| pipeline.clone().process(item),
        |item, outcome| match &outcome.result {
            Ok(()) => println!(
                "ok\t{}\t{}\t{}\t{}ms",
                outcome.index,
                item.image,
                item.output,
                outcome.took.as_millis()
            ),
            Err(e) => println!(
                "fail\t{}\t{}\t{}\t{}\t{e:#}",
                outcome.index,
                item.image,
                item.output,
                if failure::is_permanent(e) {
                    "permanent"
                } else {
                    "retryable"
                }
            ),
        },
    )
    .await;

    eprintln!(
        "batch: {} items, {} failed, {} ms, compile concurrency {concurrency}",
        report.total,
        report.failed_count(),
        start.elapsed().as_millis()
    );
    write_termination_log(&args.termination_log, &report.termination_message());
    report.exit_code()
}

/// Best effort: outside Kubernetes the default path is usually not writable.
fn write_termination_log(path: &Path, message: &str) {
    if let Err(e) = std::fs::write(path, message) {
        eprintln!(
            "note: could not write the summary to {}: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_and_output_flags() {
        let args = Args::parse_from([
            "wash-precompile",
            "--image",
            "ghcr.io/example/comp:v1",
            "--output",
            "file:///tmp/out.cwasm",
        ]);
        assert_eq!(args.image, vec!["ghcr.io/example/comp:v1"]);
        assert_eq!(args.output, vec!["file:///tmp/out.cwasm"]);
        assert!(args.is_single());
    }

    #[test]
    fn repeated_pairs_are_batch_mode() {
        let args = Args::parse_from([
            "wash-precompile",
            "--image",
            "r/a:1",
            "--output",
            "file:///tmp/a",
            "--image",
            "r/b:1",
            "--output",
            "file:///tmp/b",
        ]);
        assert!(!args.is_single());
        let items = args.items().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].image, "r/b:1");
        assert_eq!(items[1].output.as_str(), "file:///tmp/b");
    }

    #[test]
    fn batch_file_is_batch_mode_and_appends_after_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch");
        std::fs::write(&path, "r/b:1 file:///tmp/b\n").unwrap();
        let args = Args::parse_from([
            "wash-precompile",
            "--image",
            "r/a:1",
            "--output",
            "file:///tmp/a",
            "--batch-file",
            path.to_str().unwrap(),
        ]);
        assert!(!args.is_single());
        let images: Vec<String> = args.items().unwrap().into_iter().map(|i| i.image).collect();
        assert_eq!(images, vec!["r/a:1", "r/b:1"]);
    }

    #[test]
    fn batch_flag_forces_batch_mode_for_one_pair() {
        let args = Args::parse_from([
            "wash-precompile",
            "--batch",
            "--image",
            "r/a:1",
            "--output",
            "file:///tmp/a",
        ]);
        assert!(!args.is_single());
        assert_eq!(args.items().unwrap().len(), 1);
    }

    #[test]
    fn mismatched_pairs_error() {
        let args = Args::parse_from(["wash-precompile", "--image", "r/a:1"]);
        assert!(!args.is_single());
        assert!(args.items().is_err());
    }

    #[test]
    fn empty_batch_errors() {
        let args = Args::parse_from(["wash-precompile"]);
        assert!(args.items().is_err());
    }

    #[test]
    fn concurrency_is_at_least_one() {
        let args = Args::parse_from(["wash-precompile", "--concurrency", "0"]);
        assert_eq!(args.compile_concurrency(), 1);
    }

    #[test]
    fn exit_codes_are_distinct() {
        assert_ne!(EXIT_PERMANENT, EXIT_RETRYABLE);
    }
}
