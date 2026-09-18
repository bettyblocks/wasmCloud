# syntax=docker/dockerfile:1-labs


FROM cgr.dev/chainguard/wolfi-base@sha256:9a8d954d8f03a21bcf2be73d4628f0ad26d35c3275469925de63a34eebd58f13 AS chef
USER root
WORKDIR /src

RUN apk add --no-cache \
        gcc \
        glibc-dev \
        binutils \
        rust-1.96 \
        curl
ENV PATH=/root/.cargo/bin:$PATH
RUN curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | sh
RUN cargo binstall -y --disable-telemetry cargo-chef

FROM chef AS planner
COPY --exclude=rust-toolchain.toml . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

# Docker/buildx sets TARGETARCH automatically ("amd64"/"arm64") to match the
# build's target platform. This image is always built natively per-arch (amd64
# on ubuntu-latest, arm64 on ubuntu-24.04-arm runners, never cross-compiled),
# so TARGETARCH always matches the host's own already-installed Rust target.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu ;; \
      arm64) echo aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust_target.txt

RUN apk --no-cache add protoc protobuf protobuf-dev

COPY --from=planner /src/recipe.json recipe.json
# Notice that we are specifying the --target flag!
RUN cargo chef cook --release --target "$(cat /rust_target.txt)" --recipe-path recipe.json
COPY --exclude=rust-toolchain.toml --chown=nonroot:nonroot . .

# Optional comma-separated cargo feature list for opt-in extras (e.g.
# "wasi-tls", "wasi-webgpu"). WASI Preview 3 is already compiled into the
# default wash build, so it needs no feature flag here.
ARG CARGO_FEATURES=""

# build static binary
RUN cargo build --release --target "$(cat /rust_target.txt)" --bin wash ${CARGO_FEATURES:+--features ${CARGO_FEATURES}} \
    && cp "target/$(cat /rust_target.txt)/release/wash" /src/wash

# Release image
FROM cgr.dev/chainguard/wolfi-base@sha256:9a8d954d8f03a21bcf2be73d4628f0ad26d35c3275469925de63a34eebd58f13
RUN apk add --no-cache git

COPY --from=builder /src/wash /usr/local/bin/wash

# Smoke test the binary against THIS stage's libc.
RUN ["/usr/local/bin/wash", "--version"]

ENTRYPOINT ["/usr/local/bin/wash"]
