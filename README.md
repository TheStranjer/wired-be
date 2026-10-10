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
| `BASE_URL` | yes | | Public origin of this instance, e.g. `https://social.example` (http(s) with a host, no path or query; trailing slash stripped) |
| `BIND_ADDR` | no | `127.0.0.1:4000` | Address the HTTP server listens on |
| `INSTANCE_NAME` | no | `Wired` | Instance display name |
| `INSTANCE_DESCRIPTION` | no | `A Pleroma-compatible instance` | Short instance description |
| `CONTACT_EMAIL` | no | | Admin contact email |
| `INSTANCE_LANGUAGES` | no | `en` | Comma-separated content language codes |
| `MAX_STATUS_CHARS` | no | `5000` | Maximum characters per status |
| `MAX_MEDIA_ATTACHMENTS` | no | `4` | Maximum media attachments per status |
| `MAX_POLL_OPTIONS` | no | `20` | Maximum options per poll |
| `ACCOUNTS_EMAIL_OPTIONAL` | no | `false` | Allow registering local accounts without an email address (`true`/`false`, or `1`/`0`) |
| `MIN_PASSWORD_LENGTH` | no | `8` | Minimum length of a registration password, in characters |
| `OAUTH_TOKEN_TTL_SECS` | no | `2592000` | Lifetime granted to newly issued OAuth access tokens, in seconds (default: 30 days) |
| `UPLOAD_LIMIT_BYTES` | no | `16000000` | Maximum size of one uploaded file in a multipart request, in bytes |
| `UPLOAD_MAX_FILES` | no | `16` | Maximum number of files in one multipart request |
| `HTTP_ALLOW_PRIVATE_HOSTS` | no | `false` | Disable the outbound SSRF guard so the instance may fetch private/loopback addresses (`true`/`false`, or `1`/`0`); development and test only |
| `UPLOADS_DIR` | no | `./uploads` | Directory uploaded files are stored in (relative paths resolve against the working directory) |
| `MEDIA_BASE_URL` | no | `{BASE_URL}/media` | Public prefix of uploaded files' URLs, served by `GET /media/{key}` (no trailing slash) |

## Development

```sh
cp .env.example .env   # then edit DATABASE_URL
cargo run              # serves on BIND_ADDR
cargo test
cargo clippy --all-targets --all-features
cargo fmt
scripts/ci.sh          # full gate (same as CI): exits 0 if everything passes, 1 otherwise
```

Lint configuration lives in `Cargo.toml` (`[lints]`) and `clippy.toml`. See `AGENTS.md` for contribution rules.

## Tests

Tests need a reachable PostgreSQL, given by `DATABASE_URL` (or `.env`). They use `#[sqlx::test]`, which creates a fresh throwaway database per test, applies the migrations from `migrations/`, and drops the database afterwards; the database named in `DATABASE_URL` itself is never mutated. The connecting role must be able to create databases.
