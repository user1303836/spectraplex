# spectraplex

[![CI](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml)
[![Security Audit](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml)

Spectraplex is a Rust-based indexing primitive that currently supports the Solana blockchain using the Yellowstone gRPC interface. It handles the connection management, protobuf deserialization, and filtering of slots, allowing downstream services to subscribe to low-latency chain events (transactions, account updates, and block metadata) with minimal overhead.
