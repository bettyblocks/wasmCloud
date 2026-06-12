#!/usr/bin/env bash
# Build all demo components.
#
# All three guests (frontend, counter, sse-service) are P3 components
# using component-model async, which the wasm32-wasip2 toolchain cannot
# link yet — so they are built as wasm32-wasip1 core modules and
# componentized with the WASI preview1 reactor adapter by the `builder`
# helper (same pipeline as the wash-runtime test fixtures).
set -euo pipefail
cd "$(dirname "$0")"

cargo build --manifest-path components/Cargo.toml --target wasm32-wasip1 --release -p frontend -p counter -p sse-service
cargo run --quiet --manifest-path components/Cargo.toml -p builder
