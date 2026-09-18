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
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo chef cook --release --target x86_64-unknown-linux-gnu --recipe-path recipe.json
COPY --exclude=rust-toolchain.toml . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --features epoch-interruption --target x86_64-unknown-linux-gnu -p wash-precompile --bin wash-precompile && \
    cp target/x86_64-unknown-linux-gnu/release/wash-precompile /tmp/wash-precompile

# Release image — minimal base. wash-precompile is a one-shot worker so we
# need only the binary; no shell, no package manager.
FROM cgr.dev/chainguard/wolfi-base@sha256:9a8d954d8f03a21bcf2be73d4628f0ad26d35c3275469925de63a34eebd58f13
COPY --from=builder /tmp/wash-precompile /usr/local/bin/wash-precompile

# Smoke test the binary against THIS stage's libc.
RUN ["/usr/local/bin/wash-precompile", "--help"]

ENTRYPOINT ["/usr/local/bin/wash-precompile"]
