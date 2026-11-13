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
| `MAX_STATUS_CHARS` | no | `5000` | Maximum characters per status; `POST /api/v1/statuses` counts them the way clients do (each bare URL is worth 23) and refuses a longer post with 422 `{"error":"The status is over the character limit"}` |
| `MAX_MEDIA_ATTACHMENTS` | no | `4` | Maximum media attachments per status; `POST /api/v1/statuses` refuses a body whose `media_ids` array is longer with 422 `{"error":"Too many attachments"}` |
| `MAX_POLL_OPTIONS` | no | `20` | Maximum options per poll |
| `MAX_POLL_OPTION_CHARS` | no | `200` | Maximum characters in one poll option; `POST /api/v1/statuses` refuses a poll carrying an empty option or a longer one with 422 `{"error":"Poll options cannot be longer than 200 characters"}` (the message names this value) |
| `MIN_POLL_EXPIRATION` | no | `300` | Shortest poll expiration (seconds); `POST /api/v1/statuses` refuses a poll whose `poll[expires_in]` is shorter with 422 `{"error":"Expiration date is too soon"}` |
| `MAX_POLL_EXPIRATION` | no | `31536000` | Longest poll expiration (seconds, one year); `POST /api/v1/statuses` refuses a poll whose `poll[expires_in]` is longer with 422 `{"error":"Expiration date is too far in the future"}` |
| `MAX_CHAT_MESSAGE_CHARS` | no | `5000` | Maximum characters per chat message; longer posted content is refused with 400 |
| `MAX_MEDIA_DESCRIPTION` | no | `1500` | Maximum characters in a media attachment's description (alt text); `PUT /api/v1/media/{id}` refuses longer values with 422 |
| `MAX_DISPLAY_NAME_CHARS` | no | `50` | Maximum characters in an account display name; `PATCH /api/v1/accounts/update_credentials` refuses a longer `display_name` with 422 |
| `MAX_BIO_CHARS` | no | `5000` | Maximum characters in a profile note (bio); `PATCH /api/v1/accounts/update_credentials` refuses a longer `note` with 422 |
| `MAX_PROFILE_FIELDS` | no | `4` | Maximum number of profile metadata entries (`fields_attributes`) per account; `PATCH /api/v1/accounts/update_credentials` refuses more with 422 `{"error":"Too many fields"}` |
| `MAX_REPORT_COMMENT` | no | `1000` | Maximum characters in a report's `comment`; `POST /api/v1/reports` refuses a longer one with 422 `{"error":"Validation failed: Comment is too long (maximum is 1000 characters)"}` |
| `MAX_PINNED_STATUSES` | no | `1` | Maximum number of statuses one account may pin to its profile; `POST /api/v1/statuses/{id}/pin` refuses one past the limit with 422 `{"error":"You have already pinned the maximum number of statuses"}` (`.env.example` suggests `5`, the number Mastodon clients ship) |
| `MIN_EXPIRATION_SECS` | no | `600` | Shortest `expires_in` (seconds) a `POST /api/v1/statuses` body may ask for; a shorter one is refused with 422 `{"error":"expires_in must be at least 600 seconds"}` (the message names this value), and an accepted one becomes the status's `expires_at`, deleted by the `statuses.expire` job at that time or by the five-minute `statuses.expire_sweep` safety pass |
| `MIN_SCHEDULED_SECS` | no | `300` | How far in the future (seconds) a `POST /api/v1/statuses` `scheduled_at` must be; the scheduled-statuses window check reads it to refuse a `scheduled_at` less than this many seconds ahead |
| `MAX_SCHEDULED_PER_USER` | no | `300` | How many pending scheduled rows one account may hold; the per-user scheduled-status limit check reads it to refuse a `POST /api/v1/statuses` `scheduled_at` past this count |
| `MAX_SCHEDULED_PER_USER_PER_DAY` | no | `25` | How many pending scheduled rows one account may hold for one UTC day; the per-user-per-day scheduled-status limit check reads it to refuse a `scheduled_at` on a day that already holds this many |
| `ACCOUNTS_EMAIL_OPTIONAL` | no | `false` | Allow registering local accounts without an email address (`true`/`false`, or `1`/`0`) |
| `MIN_PASSWORD_LENGTH` | no | `8` | Minimum length of a registration or reset password, in characters |
| `OAUTH_TOKEN_TTL_SECS` | no | `2592000` | Lifetime granted to newly issued OAuth access tokens, in seconds (default: 30 days) |
| `PASSWORD_RESET_TTL_SECS` | no | `86400` | Lifetime of a password-reset token, in seconds (default: 24 hours); a token is valid while unused and within this window |
| `ALLOW_ACCOUNT_DELETION` | no | `true` | Whether accounts may delete their own account: while off, `POST /api/pleroma/delete_account` answers 403 `{"error":"Account deletion is disabled"}`; `disable_account` is unaffected (`true`/`false`, or `1`/`0`) |
| `SECRET_KEY_BASE` | no | a fresh random secret per boot | Application-wide HMAC signing secret (Pleroma's `secret_key_base`): verifies `GET /mailer/unsubscribe/{token}` tokens and signs the mailer's unsubscribe links; unset or blank generates a new secret at every boot (with a warning), so links minted earlier stop validating after a restart; never logged |
| `OAUTH_PROVIDERS` | no | `[]` | External OAuth login providers, as the JSON text of an array of `{"name","client_id","client_secret","authorize_url","token_url","userinfo_url","scopes","uid_field","nickname_field","email_field","name_field"}` objects; invalid JSON refuses startup and `client_secret` is never logged |
| `MONGOOSEIM_ENABLED` | no | `false` | Serve the MongooseIM auth endpoints `GET /user_exists` and `GET /check_password`; while off both answer 404 `{"error":"Not found"}` (`true`/`false`, or `1`/`0`) |
| `CAPTCHA_ENABLED` | no | `false` | Announce a captcha provider on `GET /api/v1/pleroma/captcha` — no captcha is implemented, so off answers `{"type":"none"}` and on answers 501 (`true`/`false`, or `1`/`0`) |
| `HEALTHCHECK_ENABLED` | no | `false` | Report pool and liveness data on `GET /api/v1/pleroma/healthcheck`; while off the path answers 404 `{"error":"Not found"}` (`true`/`false`, or `1`/`0`) |
| `REGISTRATIONS_OPEN` | no | `true` | Whether the instance accepts new local registrations; NodeInfo reports it as `openRegistrations` (`true`/`false`, or `1`/`0`) |
| `INVITES_ENABLED` | no | `false` | Whether a valid registration invite `token` substitutes for open registrations at `POST /api/v1/accounts` (closed and off: 403; closed and on without a valid invite: 400 `{"error":"Invalid token"}`); reported as `invites_enabled` on the instance endpoints and `invitesEnabled` on NodeInfo (`true`/`false`, or `1`/`0`) |
| `REGISTRATIONS_APPROVAL_REQUIRED` | no | `false` | Whether a new account waits for staff approval before it may log in: `POST /api/v1/accounts` then answers 200 `{"message":"…","identifier":"awaiting_approval"}` with no token, and every login is 403 `{"error":"Your account is awaiting approval.","identifier":"awaiting_approval"}`; an invite-token registration skips the wait. Reported as `approval_required` on the instance endpoints (`true`/`false`, or `1`/`0`) |
| `ACCOUNT_ACTIVATION_REQUIRED` | no | `false` | Whether an account must confirm its email address — a new one before it may log in, a changed one before it is carried as confirmed: `POST /api/v1/accounts` then queues the confirmation email (only while `SMTP_HOST` is set) and answers 200 `{"message":"…","identifier":"missing_confirmed_email"}` with no token, an unconfirmed login is 403 `{"error":"Your login is missing a confirmed e-mail address","identifier":"missing_confirmed_email"}`, and `POST /api/pleroma/change_email` leaves the account unconfirmed and queues a confirmation letter for the new address. Reported as `account_activation_required` on the instance endpoints and `accountActivationRequired` on NodeInfo (`true`/`false`, or `1`/`0`) |
| `SEARCH_STATUSES_UNAUTHENTICATED` | no | `false` | Whether anonymous requests search statuses at `GET /api/v1/search` and `GET /api/v2/search`; while off an unauthenticated search answers an empty `statuses` list (`true`/`false`, or `1`/`0`) |
| `RESTRICT_UNAUTHENTICATED` | no | | Comma-separated areas an anonymous request may not read, from `timelines_local`, `timelines_federated`, `profiles_local`, `profiles_remote`, `activities_local`, `activities_remote`; an unknown one refuses startup. `GET /api/v1/accounts/{id}/statuses` closes behind `profiles_local` (a local account) and `profiles_remote` (a mirror) with 403 `{"error":"This API requires an authenticated user"}`; `GET /api/v1/timelines/public` closes behind `timelines_local` (its `local=true` read) and `timelines_federated` (every other) with 403 `{"error":"Authentication is required to access this resource."}`; `activities_local` and `activities_remote` name the status reads the later tickets gate |
| `AVAILABLE_FRONTENDS` | no | | Comma-separated frontends clients may pick at `PUT /api/v1/pleroma/preferred_frontend`; while empty, the available list is `[]` and every pick is refused with 422 |
| `SIGN_FETCHES` | no | `true` | Sign outgoing actor `GET`s with the instance actor's HTTP Signature so peers in authorized-fetch mode answer them (`true`/`false`, or `1`/`0`) |
| `AUTHORIZED_FETCH` | no | `false` | Require a valid HTTP Signature on `GET`s of the ActivityPub actor and collection routes; while off they answer anonymous requests (`true`/`false`, or `1`/`0`) |
| `FEDERATION_ENABLED` | no | `true` | Whether the instance federates at all; while off every ActivityPub inbox `POST` is refused with 403 `{"error":"Federation is disabled"}` and outbound delivery queues nothing (`true`/`false`, or `1`/`0`) |
| `ACCEPT_UNSOLICITED_PUBLIC` | no | `true` | Whether public and unlisted remote statuses are stored even from authors no local account follows; while off the ingest path drops such a post unless a local user is mentioned in it or it answers a local status (`private`, `direct` and followers-addressed posts are never gated) (`true`/`false`, or `1`/`0`) |
| `UNREACHABLE_GRACE_SECS` | no | `0` | How long, in seconds, a host carrying a mark in `instances.unreachable_since` stays deliverable: outbound federation drops a host only once its mark is older than this window, so `0` makes any marked host unreachable at once (Pleroma's behaviour) |
| `MRF_POLICIES` | no | *(empty)* | Comma-separated MRF policy names (`noop` today; policy tickets add more) run over every incoming and outgoing activity when the `mrf` instance setting carries no `policies` array of its own; the active names are advertised in the instance documents' `mrf_policies` |
| `UPLOAD_LIMIT_BYTES` | no | `16000000` | Maximum size of one uploaded file in a multipart request, in bytes |
| `UPLOAD_MAX_FILES` | no | `16` | Maximum number of files in one multipart request |
| `AVATAR_LIMIT_BYTES` | no | `2000000` | Maximum size of the `avatar` image of `PATCH /api/v1/accounts/update_credentials`, in bytes; a larger one is refused with 422 `{"error":"File is too large"}` |
| `BANNER_LIMIT_BYTES` | no | `4000000` | Maximum size of the `header` or `pleroma_background_image` image of `PATCH /api/v1/accounts/update_credentials`, in bytes; a larger one is refused with 422 `{"error":"File is too large"}` |
| `INBOX_MAX_BYTES` | no | `1048576` | Hard cap on one inbound ActivityPub inbox request body, in bytes; a larger body is answered 413 `{"error":"Payload too large"}` |
| `MAX_REMOTE_MENTION_LOOKUPS` | no | `10` | How many unknown-remote `@nick@domain` mentions one status may look up over the network at compose time; the lookups share a five-second budget and any that fail, time out, or pass the cap stay plain text |
| `MAX_IMAGE_PIXELS` | no | `40000000` | Maximum pixel count (width times height) of an uploaded image; the media upload endpoints reject a larger image with 422 `{"error":"Image too large"}` and an undecodable one with 422 `{"error":"Invalid image"}` |
| `HTTP_ALLOW_PRIVATE_HOSTS` | no | `false` | Disable the outbound SSRF guard so the instance may fetch private/loopback addresses (`true`/`false`, or `1`/`0`); development and test only |
| `UPLOADS_DIR` | no | `./uploads` | Directory uploaded files are stored in (relative paths resolve against the working directory) |
| `EMOJI_DIR` | no | `./emoji` | Directory emoji packs are stored in (one subdirectory per pack, with a `pack.json` and its images; served by `GET /emoji/{pack}/{file}`) |
| `MEDIA_BASE_URL` | no | `{BASE_URL}/media` | Public prefix of uploaded files' URLs, served by `GET /media/{key}` (no trailing slash) |
| `JOB_CONCURRENCY` | no | `10` | Job handlers a single worker runs at the same time |
| `JOB_QUEUES` | no | `default,federator_incoming,federator_outgoing,mailer,scheduled,background` | Comma-separated job queue names, in claim priority order |
| `PURGE_DELETED_STATUSES_DAYS` | no | `30` | Age in days after which soft-deleted local statuses are hard-deleted by the daily maintenance purge; `0` disables the purge (remote tombstones are never deleted, so a deleted remote post is never re-imported) |
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
| `RICH_MEDIA_IGNORE_HOSTS` | no | *(empty)* | Comma-separated hosts rich media card fetching never visits; a link to one of them is never fetched and gets no card |
| `RICH_MEDIA_REMOTE` | no | `false` | Whether preview cards are fetched for the remote URLs statuses link to; while off the card cache stays empty (`true`/`false`, or `1`/`0`) |
| `TRANSLATION_PROVIDER` | no | `none` | Status translation: `none` turns it off (`POST /api/v1/statuses/{id}/translate` answers 503), `libretranslate` and `deepl` send the status content to that provider's API |
| `TRANSLATION_API_URL` | no | | The provider's API base URL (no trailing slash), e.g. `https://translate.example` or `https://api-free.deepl.com`; a selected provider without it is left unused, like `none` |
| `TRANSLATION_API_KEY` | no | | The provider's API key, redacted in logs; `deepl` has no unauthenticated mode and is left unused without one, a self-hosted `libretranslate` usually needs none |
| `VAPID_PRIVATE_KEY` | no | | The instance's Web Push signing key (ES256, PKCS#8 PEM), redacted in logs; when set it overwrites the pair generated into `instance_secrets` at startup, and the public half is derived from it unless `VAPID_PUBLIC_KEY` pins one |
| `VAPID_PUBLIC_KEY` | no | | The instance's Web Push public key (URL-safe base64 of the uncompressed P-256 point); the `server_key` the push endpoints and the `Application` entity advertise, and the `vapid_key` / `vapid_public_key` of the app and instance responses |
| `RATE_LIMITS_ENABLED` | no | `true` | Whether routes that mount a rate-limit bucket throttle their requests; while off they answer uncounted and without `x-ratelimit-*` headers (`true`/`false`, or `1`/`0`) |
| `RATE_LIMITS` | no | Pleroma's defaults | Bucket overrides as a comma-separated list of `bucket=max:window_secs` entries; each replaces its bucket's default and an unknown name joins the table, which otherwise is `authentication=20:5, app_account_creation=5:1800, relations_actions=10:10, relation_id_action=2:60, statuses_actions=10:10, status_id_action=2:60, password_reset=5:1800, account_confirmation_resend=5:8600, search=30:60`; a bucket off the table is unlimited |
| `TRUST_PROXY_HEADERS` | no | `false` | Let the leftmost `X-Forwarded-For` entry name the client a rate-limit window counts against; while off the socket peer always does (`true`/`false`, or `1`/`0`) |

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

