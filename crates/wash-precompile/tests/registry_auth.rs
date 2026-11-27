//! The worker must send the registry credentials it gets from the
//! runtime-operator. These tests run the real binary against a tiny in-process
//! OCI registry that requires HTTP Basic auth, so they need no network and no
//! Docker.

// Test helpers outside `#[test]` fns; a panic is the right failure here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const REPO: &str = "test/comp";
const TAG: &str = "v1";
const USER: &str = "robot";
const PASS: &str = "s3cret";
/// base64("robot:s3cret")
const BASIC_AUTH: &str = "cm9ib3Q6czNjcmV0";

struct FakeRegistry {
    addr: String,
    authorized_requests: Arc<AtomicUsize>,
}

fn sha256(data: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(data))
}

/// Starts a registry that serves one Wasm component at `test/comp:v1` and
/// answers 401 to every request without the expected Basic credentials.
fn start_registry() -> FakeRegistry {
    let wasm = wat::parse_str("(component)").unwrap();
    let config = b"{}".to_vec();
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/wasm","digest":"{}","size":{}}}]}}"#,
        sha256(&config),
        config.len(),
        sha256(&wasm),
        wasm.len(),
    )
    .into_bytes();
    let blobs = [(sha256(&config), config), (sha256(&wasm), wasm)];

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let authorized_requests = Arc::new(AtomicUsize::new(0));
    let counter = authorized_requests.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            handle(stream, &manifest, &blobs, &counter);
        }
    });
    FakeRegistry {
        addr,
        authorized_requests,
    }
}

fn handle(mut stream: TcpStream, manifest: &[u8], blobs: &[(String, Vec<u8>)], ok: &AtomicUsize) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let mut authorized = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
            && value.trim() == format!("Basic {BASIC_AUTH}")
        {
            authorized = true;
        }
    }

    let (status, content_type, body): (&str, &str, &[u8]) = if !authorized {
        ("401 Unauthorized", "text/plain", b"unauthorized")
    } else {
        ok.fetch_add(1, Ordering::SeqCst);
        let blob_prefix = format!("/v2/{REPO}/blobs/");
        if path == "/v2/" {
            ("200 OK", "application/json", b"{}")
        } else if path == format!("/v2/{REPO}/manifests/{TAG}") {
            (
                "200 OK",
                "application/vnd.oci.image.manifest.v1+json",
                manifest,
            )
        } else if let Some((_, blob)) = path
            .strip_prefix(&blob_prefix)
            .and_then(|digest| blobs.iter().find(|(d, _)| d == digest))
        {
            ("200 OK", "application/octet-stream", blob)
        } else {
            ("404 Not Found", "text/plain", b"not found")
        }
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         WWW-Authenticate: Basic realm=\"fake\"\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn docker_config_json(registry: &str) -> String {
    format!(r#"{{"auths":{{"{registry}":{{"auth":"{BASIC_AUTH}"}}}}}}"#)
}

/// Runs the worker with a clean env (no HOME, so no ~/.docker fallback).
fn run_worker(registry: &FakeRegistry, extra_env: &[(&str, &str)]) -> (Output, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("out.cwasm");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wash-precompile"));
    cmd.env_clear()
        .env("INSECURE_REGISTRIES", &registry.addr)
        .arg("--image")
        .arg(format!("{}/{REPO}:{TAG}", registry.addr))
        .arg("--output")
        .arg(format!("file://{}", output_path.display()));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    (cmd.output().expect("failed to spawn wash-precompile"), dir)
}

fn assert_success(out: &Output, dir: &TempDir) {
    assert!(
        out.status.success(),
        "wash-precompile failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let len = std::fs::metadata(dir.path().join("out.cwasm"))
        .unwrap()
        .len();
    assert!(len > 0, "output file is empty");
}

#[test]
fn pulls_with_credentials_from_docker_config_json_env() {
    let registry = start_registry();
    let json = docker_config_json(&registry.addr);

    let (out, path) = run_worker(&registry, &[("DOCKER_CONFIG_JSON", &json)]);

    assert_success(&out, &path);
    assert!(registry.authorized_requests.load(Ordering::SeqCst) > 0);
}

/// An old operator mounts the Secret and sets DOCKER_CONFIG; a new worker must
/// still use it.
#[test]
fn pulls_with_credentials_from_docker_config_dir() {
    let registry = start_registry();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        docker_config_json(&registry.addr),
    )
    .unwrap();

    let (out, path) = run_worker(
        &registry,
        &[("DOCKER_CONFIG", dir.path().to_str().unwrap())],
    );

    assert_success(&out, &path);
}

#[test]
fn env_json_wins_over_docker_config_dir() {
    let registry = start_registry();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"auths":{"elsewhere.example.com":{"username":"x","password":"y"}}}"#,
    )
    .unwrap();
    let json = docker_config_json(&registry.addr);

    let (out, path) = run_worker(
        &registry,
        &[
            ("DOCKER_CONFIG", dir.path().to_str().unwrap()),
            ("DOCKER_CONFIG_JSON", &json),
        ],
    );

    assert_success(&out, &path);
}

#[test]
fn fails_without_credentials() {
    let registry = start_registry();

    let (out, _) = run_worker(&registry, &[]);

    assert!(!out.status.success(), "anonymous pull should be rejected");
    assert_eq!(registry.authorized_requests.load(Ordering::SeqCst), 0);
}

#[test]
fn fails_with_wrong_credentials() {
    let registry = start_registry();
    let json = format!(
        r#"{{"auths":{{"{}":{{"username":"{USER}","password":"wrong-{PASS}"}}}}}}"#,
        registry.addr
    );

    let (out, _) = run_worker(&registry, &[("DOCKER_CONFIG_JSON", &json)]);

    assert!(
        !out.status.success(),
        "wrong credentials should be rejected"
    );
}
