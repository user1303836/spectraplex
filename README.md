# spectraplex

Spectraplex is a Rust-based indexing primitive that currently supports the Solana blockchain using the Yellowstone gRPC interface. It handles the connection management, protobuf deserialization, and filtering of slots, allowing downstream services to subscribe to low-latency chain events (transactions, account updates, and block metadata) with minimal overhead. This is a work in progress as of 12/15/25.
