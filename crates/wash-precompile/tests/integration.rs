use std::process::Command;

use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio::io::AsyncReadExt;

const TEST_IMAGE: &str = "ghcr.io/wasmcloud/components/http-hello-world-rust:0.1.0";

#[test]
#[ignore = "network: pulls a real component from ghcr.io"]
fn end_to_end_pull_compile_write() {
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("out.cwasm");
    let output_url = format!("file://{}", output_path.display());

    let status = Command::new(env!("CARGO_BIN_EXE_wash-precompile"))
        .arg("--image")
        .arg("ghcr.io/wasmcloud/components/http-hello-world-rust:0.1.0")
        .arg("--output")
        .arg(&output_url)
        .status()
        .expect("failed to spawn wash-precompile");

    assert!(status.success(), "wash-precompile exited with {status}");

    let metadata = std::fs::metadata(&output_path).expect("output file not written");
    assert!(metadata.len() > 0, "output file is empty");
}

#[tokio::test]
#[ignore = "network: pulls a real component, requires Docker for NATS container"]
async fn end_to_end_nats_output() {
    let container = GenericImage::new("nats", "2-alpine")
        .with_exposed_port(4222.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js"])
        .start()
        .await
        .expect("failed to start NATS container");

    let port = container
        .get_host_port_ipv4(4222)
        .await
        .expect("failed to get NATS host port");
    let nats_url = format!("nats://127.0.0.1:{port}");

    let bucket = "test-precompile";
    let key = "myapp/test.cwasm";
    let output_url = format!("nats://{bucket}/{key}");

    // NOTE: Output to the same url two times so that we test if the bucket
    // exists flow also succeeds
    let status = Command::new(env!("CARGO_BIN_EXE_wash-precompile"))
        .env("NATS_URL", &nats_url)
        .arg("--image")
        .arg(TEST_IMAGE)
        .arg("--output")
        .arg(&output_url)
        .status()
        .expect("failed to spawn wash-precompile");

    assert!(status.success(), "wash-precompile exited with {status}");

    let status = Command::new(env!("CARGO_BIN_EXE_wash-precompile"))
        .env("NATS_URL", &nats_url)
        .arg("--image")
        .arg(TEST_IMAGE)
        .arg("--output")
        .arg(&output_url)
        .status()
        .expect("failed to spawn wash-precompile");

    assert!(status.success(), "wash-precompile exited with {status}");

    let client = async_nats::connect(&nats_url)
        .await
        .expect("failed to connect to NATS");
    let jetstream = async_nats::jetstream::new(client);
    let store = jetstream
        .get_object_store(bucket)
        .await
        .expect("bucket not created by worker");

    let mut object = store.get(key).await.expect("object not found in bucket");
    let mut bytes = Vec::new();
    object
        .read_to_end(&mut bytes)
        .await
        .expect("failed to read object");

    assert!(!bytes.is_empty(), "object is empty");
}

// Batch mode. These items fail without network access, so they run by default.
#[cfg(test)]
mod batch_mode {
    use std::process::Command;

    /// Runs the binary in batch mode with items that fail without network access.
    fn run_batch(lines: &str) -> (i32, serde_json::Value, String) {
        let dir = tempfile::tempdir().unwrap();
        let batch = dir.path().join("batch");
        let term = dir.path().join("termination-log");
        std::fs::write(&batch, lines).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_wash-precompile"))
            .arg("--batch-file")
            .arg(&batch)
            .arg("--termination-log")
            .arg(&term)
            .output()
            .expect("failed to spawn wash-precompile");
        let summary = serde_json::from_slice(&std::fs::read(&term).unwrap()).unwrap();
        (
            out.status.code().unwrap(),
            summary,
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    }

    #[test]
    fn batch_reports_every_item_and_exits_retryable() {
        // Item 0: a bad output scheme (permanent). Item 1: a refused connection (retryable).
        let (code, summary, stdout) =
            run_batch("127.0.0.1:1/a:1 s3://bucket/a\n127.0.0.1:1/b:1 file:///tmp/b.cwasm\n");

        assert_eq!(code, 1, "{stdout}");
        assert_eq!(summary["total"], 2);
        assert_eq!(summary["ok"], serde_json::json!([]));
        assert_eq!(summary["failed"][0]["index"], 0);
        assert_eq!(summary["failed"][0]["permanent"], true);
        assert_eq!(summary["failed"][1]["index"], 1);
        assert_eq!(summary["failed"][1]["permanent"], false);
        assert_eq!(
            stdout.lines().filter(|l| l.starts_with("fail\t")).count(),
            2
        );
    }

    #[test]
    fn batch_with_only_permanent_failures_exits_2() {
        let (code, summary, _) = run_batch("bad@@ref file:///tmp/a.cwasm\n");
        assert_eq!(code, 2);
        assert_eq!(summary["failed"][0]["permanent"], true);
    }

    #[test]
    fn invalid_batch_file_exits_2_with_a_summary() {
        let (code, summary, _) = run_batch("only-one-field\n");
        assert_eq!(code, 2);
        assert_eq!(summary["permanent"], true);
    }
}
