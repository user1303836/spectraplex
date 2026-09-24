# syntax=docker/dockerfile:1
FROM rust:1.94-bookworm AS builder
WORKDIR /app
COPY . .
ARG CARGO_BUILD_JOBS=2
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release --workspace \
    && cp target/release/spectraplex-api target/release/spectraplex-cli /tmp/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && adduser --disabled-password --gecos "" appuser \
    && mkdir -p /app/exports && chown -R appuser:appuser /app \
    && chmod 2770 /app/exports
COPY --from=builder /tmp/spectraplex-api /usr/local/bin/
COPY --from=builder /tmp/spectraplex-cli /usr/local/bin/
WORKDIR /app
USER appuser
EXPOSE 3000
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s \
    CMD curl -fsS http://127.0.0.1:3000/ready || exit 1
CMD ["spectraplex-api"]
