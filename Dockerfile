FROM rust:1.84-bookworm AS builder

WORKDIR /app
COPY . .

RUN cargo build --release --workspace

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

RUN adduser --disabled-password --gecos "" appuser

COPY --from=builder /app/target/release/spectraplex-api /usr/local/bin/
COPY --from=builder /app/target/release/spectraplex-cli /usr/local/bin/
COPY --from=builder /app/migrations /app/migrations

RUN chown -R appuser:appuser /app

WORKDIR /app

USER appuser

EXPOSE 3000

CMD ["spectraplex-api"]
