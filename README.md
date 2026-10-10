# wired-be

An API-only backend built on [Axum](https://github.com/tokio-rs/axum), the Rust web framework. Its goal is to first replicate, and later expand upon, [Pleroma](https://pleroma.social/)'s backend.

## Requirements

- Rust (stable, see `rust-version` in `Cargo.toml`)
- PostgreSQL (the only supported database)

## Configuration

All configuration comes from environment variables. For local development, copy `.env.example` to `.env` and edit it; `.env` is gitignored.

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | yes | | PostgreSQL URL, e.g. `postgres://user:pass@localhost:5432/wired` |
| `BIND_ADDR` | no | `127.0.0.1:4000` | Address the HTTP server listens on |

## Development

```sh
cp .env.example .env   # then edit DATABASE_URL
cargo run              # serves on BIND_ADDR
cargo test
cargo clippy --all-targets --all-features
cargo fmt
```

Lint configuration lives in `Cargo.toml` (`[lints]`) and `clippy.toml`. See `AGENTS.md` for contribution rules.
