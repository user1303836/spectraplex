FROM rust:1.84-bookworm AS builder

WORKDIR /app
COPY . .

RUN cargo build --release --workspace

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/spectraplex-api /usr/local/bin/
COPY --from=builder /app/target/release/spectraplex-cli /usr/local/bin/
COPY --from=builder /app/migrations /app/migrations

WORKDIR /app

EXPOSE 3000

CMD ["spectraplex-api"]
