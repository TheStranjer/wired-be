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
| `MAX_CHAT_MESSAGE_CHARS` | no | `5000` | Maximum characters per chat message; longer posted content is refused with 400 |
| `MAX_MEDIA_DESCRIPTION` | no | `1500` | Maximum characters in a media attachment's description (alt text); `PUT /api/v1/media/{id}` refuses longer values with 422 |
| `MAX_DISPLAY_NAME_CHARS` | no | `50` | Maximum characters in an account display name; `PATCH /api/v1/accounts/update_credentials` refuses a longer `display_name` with 422 |
| `MAX_BIO_CHARS` | no | `5000` | Maximum characters in a profile note (bio); `PATCH /api/v1/accounts/update_credentials` refuses a longer `note` with 422 |
| `MAX_PROFILE_FIELDS` | no | `4` | Maximum number of profile metadata entries (`fields_attributes`) per account; `PATCH /api/v1/accounts/update_credentials` refuses more with 422 `{"error":"Too many fields"}` |
| `ACCOUNTS_EMAIL_OPTIONAL` | no | `false` | Allow registering local accounts without an email address (`true`/`false`, or `1`/`0`) |
| `MIN_PASSWORD_LENGTH` | no | `8` | Minimum length of a registration password, in characters |
| `OAUTH_TOKEN_TTL_SECS` | no | `2592000` | Lifetime granted to newly issued OAuth access tokens, in seconds (default: 30 days) |
| `OAUTH_PROVIDERS` | no | `[]` | External OAuth login providers, as the JSON text of an array of `{"name","client_id","client_secret","authorize_url","token_url","userinfo_url","scopes","uid_field","nickname_field","email_field","name_field"}` objects; invalid JSON refuses startup and `client_secret` is never logged |
| `MONGOOSEIM_ENABLED` | no | `false` | Serve the MongooseIM auth endpoints `GET /user_exists` and `GET /check_password`; while off both answer 404 `{"error":"Not found"}` (`true`/`false`, or `1`/`0`) |
| `CAPTCHA_ENABLED` | no | `false` | Announce a captcha provider on `GET /api/v1/pleroma/captcha` — no captcha is implemented, so off answers `{"type":"none"}` and on answers 501 (`true`/`false`, or `1`/`0`) |
| `HEALTHCHECK_ENABLED` | no | `false` | Report pool and liveness data on `GET /api/v1/pleroma/healthcheck`; while off the path answers 404 `{"error":"Not found"}` (`true`/`false`, or `1`/`0`) |
| `REGISTRATIONS_OPEN` | no | `true` | Whether the instance accepts new local registrations; NodeInfo reports it as `openRegistrations` (`true`/`false`, or `1`/`0`) |
| `INVITES_ENABLED` | no | `false` | Whether a valid registration invite `token` substitutes for open registrations at `POST /api/v1/accounts` (closed and off: 403; closed and on without a valid invite: 400 `{"error":"Invalid token"}`); reported as `invites_enabled` on the instance endpoints and `invitesEnabled` on NodeInfo (`true`/`false`, or `1`/`0`) |
| `AVAILABLE_FRONTENDS` | no | | Comma-separated frontends clients may pick at `PUT /api/v1/pleroma/preferred_frontend`; while empty, the available list is `[]` and every pick is refused with 422 |
| `SIGN_FETCHES` | no | `true` | Sign outgoing actor `GET`s with the instance actor's HTTP Signature so peers in authorized-fetch mode answer them (`true`/`false`, or `1`/`0`) |
| `AUTHORIZED_FETCH` | no | `false` | Require a valid HTTP Signature on `GET`s of the ActivityPub actor routes; while off they answer anonymous requests (`true`/`false`, or `1`/`0`) |
| `FEDERATION_ENABLED` | no | `true` | Whether the instance federates at all; while off every ActivityPub inbox `POST` is refused with 403 `{"error":"Federation is disabled"}` and outbound delivery queues nothing (`true`/`false`, or `1`/`0`) |
| `UPLOAD_LIMIT_BYTES` | no | `16000000` | Maximum size of one uploaded file in a multipart request, in bytes |
| `UPLOAD_MAX_FILES` | no | `16` | Maximum number of files in one multipart request |
| `AVATAR_LIMIT_BYTES` | no | `2000000` | Maximum size of the `avatar` image of `PATCH /api/v1/accounts/update_credentials`, in bytes; a larger one is refused with 422 `{"error":"File is too large"}` |
| `BANNER_LIMIT_BYTES` | no | `4000000` | Maximum size of the `header` or `pleroma_background_image` image of `PATCH /api/v1/accounts/update_credentials`, in bytes; a larger one is refused with 422 `{"error":"File is too large"}` |
| `INBOX_MAX_BYTES` | no | `1048576` | Hard cap on one inbound ActivityPub inbox request body, in bytes; a larger body is answered 413 `{"error":"Payload too large"}` |
| `MAX_IMAGE_PIXELS` | no | `40000000` | Maximum pixel count (width times height) of an uploaded image; the media upload endpoints reject a larger image with 422 `{"error":"Image too large"}` and an undecodable one with 422 `{"error":"Invalid image"}` |
| `HTTP_ALLOW_PRIVATE_HOSTS` | no | `false` | Disable the outbound SSRF guard so the instance may fetch private/loopback addresses (`true`/`false`, or `1`/`0`); development and test only |
| `UPLOADS_DIR` | no | `./uploads` | Directory uploaded files are stored in (relative paths resolve against the working directory) |
| `EMOJI_DIR` | no | `./emoji` | Directory emoji packs are stored in (one subdirectory per pack, with a `pack.json` and its images; served by `GET /emoji/{pack}/{file}`) |
| `MEDIA_BASE_URL` | no | `{BASE_URL}/media` | Public prefix of uploaded files' URLs, served by `GET /media/{key}` (no trailing slash) |
| `JOB_CONCURRENCY` | no | `10` | Job handlers a single worker runs at the same time |
| `JOB_QUEUES` | no | `default,federator_incoming,federator_outgoing,mailer,scheduled,background` | Comma-separated job queue names, in claim priority order |
| `STREAMING_PING_INTERVAL_SECS` | no | `30` | Seconds between the heartbeat pings one `/api/v1/streaming` WebSocket receives; a socket that never answers a ping is closed on the next tick |
| `SMTP_HOST` | no | | SMTP relay host for outbound email; while unset, mail is disabled and emails are dropped at queue time (logged at debug), never sent |
| `SMTP_PORT` | no | `587` | SMTP relay port (the submission port; usually `465` with `SMTP_TLS=tls`) |
| `SMTP_USERNAME` | no | | SMTP relay login name; authenticated only when it and `SMTP_PASSWORD` are both set |
| `SMTP_PASSWORD` | no | | SMTP relay login secret; never logged |
| `SMTP_TLS` | no | `starttls` | Relay encryption: `starttls` (upgrade required), `tls` (SMTPS) or `none` (plaintext, trusted local relay only) |
| `MAIL_FROM` | no | `noreply@<instance domain>` | The `From:` address every sent email carries |
| `MEDIA_PROXY_ENABLED` | no | `false` | Rewrite remote media URLs into signed proxy links served under `/proxy` (`true`/`false`, or `1`/`0`) |
| `MEDIA_PROXY_SECRET` | when the proxy is enabled | | HMAC-SHA256 signing secret for proxy links; at least 32 characters, redacted in logs |
| `MEDIA_PROXY_BASE_URL` | no | `BASE_URL` | Base URL proxy links are built from (no trailing slash), e.g. a CDN fronting `/proxy` |
| `MEDIA_PROXY_MAX_BYTES` | no | `50000000` | Hard cap on the bytes served for one proxied media file; the fetch aborts beyond it |
| `MEDIA_PROXY_BLACKLIST` | no | *(empty)* | Comma-separated hosts the media proxy never proxies; their URLs pass through unchanged |

## Development

```sh
cp .env.example .env   # then edit DATABASE_URL
cargo run              # serves on BIND_ADDR
cargo test
cargo clippy --all-targets --all-features
cargo fmt
scripts/ci.sh          # full gate (same as CI): exits 0 if everything passes, 1 otherwise
```

The database tests read `DATABASE_URL`; `.cargo/config.toml`
supplies the throwaway CI default (`postgres://postgres:postgres@localhost:5432/postgres`)
when the environment does not set one, and an exported `DATABASE_URL` always wins.

Lint configuration lives in `Cargo.toml` (`[lints]`) and `clippy.toml`. See `AGENTS.md` for contribution rules.

## Tests

Tests need a reachable PostgreSQL, given by `DATABASE_URL` (or `.env`). The connecting role must be able to create databases (`CREATEDB`, or a superuser), because the fixture keeps a pool of test databases beside the one the URL names.

Database tests do not create a database each. They call the fixture in `src/test_db.rs` (`common::test_db()` in the integration tests, `crate::test_db::test_db()` in the unit tests), which:

- migrates a **template** database once per migration set — the databases are named `wired_test_{fingerprint}_{slot}`, where the fingerprint hashes every migration's version and checksum plus the connecting role, so editing `migrations/` starts a fresh template and drops the old databases;
- **leases a slot** database (a copy of the template, made on a slot's first use) with a session advisory lock, held for as long as the test's `TestDb` is alive — a crashed test releases its slot with its connection, so nothing leaks and the pool grows only as wide as the number of test threads;
- **resets** the slot on lease, with one `TRUNCATE … RESTART IDENTITY CASCADE` over every table in `schema public` (the migration bookkeeping table aside), and rebuilds the copy from the template if a test changed the schema.

The database named in `DATABASE_URL` is only used for the `CREATE`/`DROP DATABASE` statements and the catalog lookups above; it is never a test target, and the environment is never mutated. What a test pays for is one connect and one `TRUNCATE` (tens of milliseconds) instead of a `CREATE DATABASE`, 38 migrations and a `DROP DATABASE` (half a second), which is what `#[sqlx::test]` did per test.

### Making a local test database fast

Nearly all of the fixture's remaining cost is `CREATE DATABASE` (copying the template) and the migrations' and tests' fsyncs. For a **development or CI database only**, turn durability down — the databases hold nothing but test rows, and every slot is reset before reuse:

```sql
ALTER SYSTEM SET fsync = off;
ALTER SYSTEM SET full_page_writes = off;
ALTER SYSTEM SET synchronous_commit = off;
SELECT pg_reload_conf();
```

`fsync`, `full_page_writes` and `synchronous_commit` are all reloadable, so no restart is needed. Never point `DATABASE_URL` at a database you care about with these settings: `fsync = off` means the server does not flush to disk at all. `.github/workflows/ci.yml` applies the same three settings to its throwaway Postgres service in a setup step (GitHub service containers cannot be started with a command).

