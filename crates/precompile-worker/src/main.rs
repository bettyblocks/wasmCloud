//! Out-of-process component precompiler.
//!
//! Pulls jobs from a NATS JetStream work queue, runs `Engine::precompile_component`
//! on the source `.wasm`, writes the resulting `.cwasm` to a JetStream Object
//! Store bucket, and publishes a completion event the operator subscribes to.
//!
//! Resource isolation is the point of this binary — wasmtime/Cranelift never
//! runs on workload hosts in steady state.

use clap::Parser;

mod job;
mod runner;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// NATS server URL.
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// JetStream stream that holds compile jobs.
    #[arg(long, env = "PRECOMPILE_STREAM", default_value = "wasmcloud-precompile")]
    stream: String,

    /// Durable consumer name on the stream.
    #[arg(long, env = "PRECOMPILE_CONSUMER", default_value = "workers")]
    consumer: String,

    /// JetStream object-store bucket where `.cwasm` artifacts are written.
    #[arg(long, env = "PRECOMPILE_BUCKET", default_value = "wasmcloud-cwasm")]
    bucket: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    runner::run(runner::WorkerConfig {
        nats_url: args.nats_url,
        stream: args.stream,
        consumer: args.consumer,
        bucket: args.bucket,
    })
    .await
}
