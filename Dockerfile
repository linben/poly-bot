FROM rust:1.96-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
RUN cargo build --release --features aws --bin scanner

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/scanner /usr/local/bin/scanner
COPY config /app/config
WORKDIR /app
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/scanner"]
