# syntax=docker/dockerfile:1-labs

FROM cgr.dev/chainguard/rust:latest-dev AS builder
WORKDIR /src
ENV RUST_BACKTRACE=1

# tools
USER root
RUN apk --no-cache add protoc protobuf protobuf-dev
USER nonroot

COPY --chown=nonroot:nonroot . .

RUN cargo build --release --bin precompile-worker

FROM cgr.dev/chainguard/wolfi-base
COPY --from=builder /src/target/release/precompile-worker /usr/local/bin/precompile-worker
ENTRYPOINT ["/usr/local/bin/precompile-worker"]
