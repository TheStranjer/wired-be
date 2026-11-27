# wired-be

Wired's API-only backend, built on [Axum](https://github.com/tokio-rs/axum), the Rust web
framework. Wired is explicitly compatible with [Pleroma](https://pleroma.social/)'s API:
its goal is to first replicate, and later expand upon, Pleroma's backend.

One binary serves it all: the Mastodon-compatible client API (`/api/v1`,
`/api/v2`), Pleroma's extensions and admin API, OAuth, `ActivityPub`
federation with the MRF moderation chain, `WebFinger`/`NodeInfo` discovery
and the streaming WebSocket — plus the Postgres-backed job worker and the
periodic scheduler that keep the instance running (see
[Running](#running)). There is no frontend and no Redis: PostgreSQL is the
database and the queue.

## API only

wired-be serves only the Mastodon/Pleroma client API, `ActivityPub`
(actor, object and collection JSON; content negotiation redirects a browser
to `FRONTEND_BASE_URL`), `WebFinger`, host-meta and `NodeInfo`. It renders no
HTML and serves no frontend. Every browser page (login and consent, MFA,
password reset, remote interaction, invites, permalinks) lives in the
frontend at `FRONTEND_BASE_URL`. The routes the backend links or redirects to
there:

| Frontend route | Linked from |
|---|---|
| `/oauth/authorize` | browser `GET /oauth/authorize` (302, query kept) |
| `/oauth/external/register` | external-provider callback, unlinked account |
| `/oauth/external/consent` | external-provider callback, untrusted app |
| `/password_reset/{token}` | password-reset letter and admin reset endpoint |
| `/registration/{token}` | invite letter |
| `/authorize_interaction?uri=` | the `WebFinger` subscribe template |
| `/notice/{id}` | browser `GET /notice/{id}` and status permalinks |
| `/{nickname}` | browser `GET /users/{nickname}` and profile links |

`tests/api/route_inventory.rs` checks that no parameterless `GET` route
answers `Accept: text/html` with `text/html`.

## Requirements

- Rust (stable, see `rust-version` in `Cargo.toml`)
- PostgreSQL (the only supported database)
- `ffmpeg` on `PATH` (optional, see `FFMPEG_PATH`): when present, a video
  upload gets a JPEG preview frame (taken at the smaller of one second and
  half the duration) and a blurhash. Without it, uploads still work: video
  and audio duration and video dimensions are read from the file's own
  headers (`MP4`/`MOV`/`M4A` and `MP3`) in pure Rust, and the attachment's
  `preview_url` is `null` instead of a thumbnail unless the client sends a
  `thumbnail` part (which needs no decoder).

## Configuration

Baseline configuration comes from environment variables. For local development, copy `.env.example` to `.env` and edit it; `.env` is gitignored.

A whitelisted subset of these settings can be overridden at runtime through
the admin config store (`GET`/`POST /api/v1/pleroma/admin/config`, backed by
the `config_entries` table); a stored override wins over its environment
value, and the instance identity, registration flags, the MRF chain and the
per-role OAuth token lifetimes (`:pleroma/:oauth_token_ttl_secs`, subkeys
`admin`, `bot` and `user` in seconds; a value that is not a positive whole
number with a representable expiry is a 422) read through it live. `GET /api/v1/pleroma/admin/config/descriptions` documents
every whitelisted key (types, descriptions and example values),
`GET /api/v1/pleroma/admin/need_reboot` says whether a restart-requiring key
changed since this boot, and `GET /api/v1/pleroma/admin/restart` asks the
process to exit gracefully so its supervisor restarts it and the pending
changes take effect. See [Moderation filters (MRF)](#moderation-filters-mrf)
below.

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | yes | | PostgreSQL URL, e.g. `postgres://user:pass@localhost:5432/wired` |
| `BASE_URL` | yes | | Public origin of this instance, e.g. `https://social.example` (http(s) with a host, no path or query; trailing slash stripped) |
| `LOCAL_DOMAIN` | no | host of `BASE_URL` (a non-default port kept) | The domain local handles show, e.g. `whatever.com` for `@alice@whatever.com`, when it differs from the host `BASE_URL` names. A bare host with an optional port: lowercased, a trailing dot stripped; a scheme, path, query, fragment, userinfo, whitespace or an empty value refuses startup. It only changes handles (the `WebFinger` subject, the `fqn` field, the instance `uri` and `domain`); every actor, object, inbox and media URL stays on `BASE_URL`. See [Split-domain deployment](#split-domain-deployment); treat it as permanent once the instance federates |
| `FRONTEND_BASE_URL` | no | `BASE_URL` | Origin the frontend pages are served from; the source of browser redirects such as `/notice/{id}` and `/oauth/authorize` (the consent screen) and of the password-reset link `{FRONTEND_BASE_URL}/password_reset/{token}` in the reset letter and the admin reset endpoint (the frontend then calls `GET`/`POST /api/v1/pleroma/password_reset`, which answer JSON only) (no trailing slash; the default-asset URLs do not use it, see "Default assets" below) |
| `BIND_ADDR` | no | `127.0.0.1:4000` | Address the HTTP server listens on |
| `INSTANCE_NAME` | no | `Wired` | Instance display name |
| `INSTANCE_DESCRIPTION` | no | `A Wired instance, compatible with Pleroma clients` | Short instance description |
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
| `OAUTH_TOKEN_TTL_SECS` | no | `2592000` | Legacy single lifetime of newly issued OAuth access tokens, in seconds (default: 30 days); now only the default of `OAUTH_TOKEN_TTL_SECS_ADMIN` and `OAUTH_TOKEN_TTL_SECS_USER` when they are unset |
| `OAUTH_TOKEN_TTL_SECS_ADMIN` | no | `OAUTH_TOKEN_TTL_SECS` (30 days) | Lifetime of an access token issued to an admin (`is_admin`; wins over the bot tier), in seconds. A value that is not a positive whole number, or whose expiry is not a valid date, refuses startup |
| `OAUTH_TOKEN_TTL_SECS_BOT` | no | `3153600000` | Lifetime of an access token issued to a bot (actor type `Service`, not an admin), in seconds (default: 100 years, Pleroma's own); it does not read `OAUTH_TOKEN_TTL_SECS` |
| `OAUTH_TOKEN_TTL_SECS_USER` | no | `OAUTH_TOKEN_TTL_SECS` (30 days) | Lifetime of an access token issued to any other account, and of an app-only `client_credentials` token, in seconds |
| `PASSWORD_RESET_TTL_SECS` | no | `86400` | Lifetime of a password-reset token, in seconds (default: 24 hours); a token is valid while unused and within this window |
| `ALLOW_ACCOUNT_DELETION` | no | `true` | Whether accounts may delete their own account: while off, `POST /api/pleroma/delete_account` answers 403 `{"error":"Account deletion is disabled"}`; `disable_account` is unaffected (`true`/`false`, or `1`/`0`) |
| `OAUTH_PROVIDERS` | no | `[]` | External OAuth login providers, as the JSON text of an array of `{"name","client_id","client_secret","authorize_url","token_url","userinfo_url","scopes","uid_field","nickname_field","email_field","name_field"}` objects; invalid JSON refuses startup and `client_secret` is never logged. The provider callback hands the browser to the frontend: `{FRONTEND_BASE_URL}/oauth/external/register?state=…` for an unlinked account, `{FRONTEND_BASE_URL}/oauth/external/consent?state=…&confirm_token=…` for a linked account of an untrusted app; those screens use the JSON-only `GET /oauth/registration_details`, `POST /oauth/register` and `GET`/`POST /oauth/external/consent` |
| `CAPTCHA_ENABLED` | no | `false` | Announce a captcha provider on `GET /api/v1/pleroma/captcha` — no captcha is implemented, so off answers `{"type":"none"}` and on answers 501 (`true`/`false`, or `1`/`0`) |
| `HEALTHCHECK_ENABLED` | no | `false` | Report pool and liveness data on `GET /api/v1/pleroma/healthcheck`; while off the path answers 404 `{"error":"Not found"}` (`true`/`false`, or `1`/`0`) |
| `REGISTRATIONS_OPEN` | no | `false` | Whether the instance accepts new local registrations; NodeInfo reports it as `openRegistrations` (`true`/`false`, or `1`/`0`) |
| `INVITES_ENABLED` | no | `false` | Whether a valid registration invite `token` substitutes for open registrations at `POST /api/v1/accounts` (closed and off: 403; closed and on without a valid invite: 400 `{"error":"Invalid token"}`); reported as `invites_enabled` on the instance endpoints and `invitesEnabled` on NodeInfo (`true`/`false`, or `1`/`0`) |
| `REGISTRATIONS_APPROVAL_REQUIRED` | no | `false` | Whether a new account waits for staff approval before it may log in: `POST /api/v1/accounts` then answers 200 `{"message":"…","identifier":"awaiting_approval"}` with no token, and every login is 403 `{"error":"Your account is awaiting approval.","identifier":"awaiting_approval"}`; an invite-token registration skips the wait. Reported as `approval_required` on the instance endpoints (`true`/`false`, or `1`/`0`) |
| `ACCOUNT_ACTIVATION_REQUIRED` | no | `false` | Whether an account must confirm its email address — a new one before it may log in, a changed one before it is carried as confirmed: `POST /api/v1/accounts` then queues the confirmation email (only while `SMTP_HOST` is set) and answers 200 `{"message":"…","identifier":"missing_confirmed_email"}` with no token, an unconfirmed login is 403 `{"error":"Your login is missing a confirmed e-mail address","identifier":"missing_confirmed_email"}`, and `POST /api/pleroma/change_email` leaves the account unconfirmed and queues a confirmation letter for the new address. Reported as `account_activation_required` on the instance endpoints and `accountActivationRequired` on NodeInfo (`true`/`false`, or `1`/`0`) |
| `SEARCH_STATUSES_UNAUTHENTICATED` | no | `false` | Whether anonymous requests search statuses at `GET /api/v1/search` and `GET /api/v2/search`; while off an unauthenticated search answers an empty `statuses` list (`true`/`false`, or `1`/`0`) |
| `RESTRICT_UNAUTHENTICATED` | no | | Comma-separated areas an anonymous request may not read, from `timelines_local`, `timelines_federated`, `profiles_local`, `profiles_remote`, `activities_local`, `activities_remote`; an unknown one refuses startup. `GET /api/v1/accounts/{id}/statuses` closes behind `profiles_local` (a local account) and `profiles_remote` (a mirror) with 403 `{"error":"This API requires an authenticated user"}`; `GET /api/v1/timelines/public` closes behind `timelines_local` (its `local=true` read) and `timelines_federated` (every other) with 403 `{"error":"Authentication is required to access this resource."}`; `activities_local` and `activities_remote` close the anonymous status reads of a local and a remote status (a reblog counts as both itself and its original): `GET /api/v1/statuses/{id}` and its `/context`, `/card`, `/history`, `/favourited_by`, `/reblogged_by` and `/quotes` reads answer 404 `{"error":"Record not found"}`, and `GET /api/v1/statuses?id[]=` and the context and quotes listings leave those statuses out; a presented user token is never refused |
| `INSTANCE_DOCS_DIR` | no | `./instance` | Directory the instance documents are stored in (relative paths resolve against the working directory): each name of `INSTANCE_DOCUMENT_NAMES` is one `<name>.html` file, written by `PATCH /api/v1/pleroma/admin/instance_document/{name}` (multipart `file`, at most 1 MiB, written atomically); the backend serves no HTML page, the `terms-of-service` document is read as JSON (`updated_at`, `content`) at `GET /api/v1/instance/terms_of_service` |
| `INSTANCE_DOCUMENT_NAMES` | no | `terms-of-service` | Comma-separated names of the instance documents the admin routes (`GET`/`PATCH`/`DELETE /api/v1/pleroma/admin/instance_document/{name}`) accept; any other name is their 404. Only `terms-of-service` has a JSON reader (`GET /api/v1/instance/terms_of_service`); the upload answers `{FRONTEND_BASE_URL}/instance/<name>.html` |
| `SIGN_FETCHES` | no | `true` | Sign outgoing actor `GET`s with the instance actor's HTTP Signature so peers in authorized-fetch mode answer them (`true`/`false`, or `1`/`0`) |
| `AUTHORIZED_FETCH` | no | `false` | Require a valid HTTP Signature (draft-cavage or RFC 9421) on `GET`s of the ActivityPub actor and collection routes; while off they answer anonymous requests (`true`/`false`, or `1`/`0`) |
| `THREAD_FETCH_DEPTH` | no | `20` | How far up a fetched thread the `federator.backfill` job walks to link a remote reply to its ancestors; a deeper thread stays linked only as far as this |
| `MAX_FETCHES_PER_ACTIVITY` | no | `20` | How many remote object dereferences one incoming activity may trigger at most; a fetch past the cap fails instead of fanning out further |
| `PROTOCOLS` | no | `activitypub` | Comma-separated ids of the protocols this instance runs; names are case-insensitive, whitespace is ignored and repeats collapse. An empty list, an unknown name, or a protocol this binary was built without (its Cargo feature, named by the id) refuses startup |
| `FEDERATION_ENABLED` | no | `true` | Whether the instance federates at all; while off every ActivityPub inbox `POST` is refused with 403 `{"error":"Federation is disabled"}` and outbound delivery queues nothing (`true`/`false`, or `1`/`0`) |
| `ACCEPT_UNSOLICITED_PUBLIC` | no | `true` | Whether public and unlisted remote statuses are stored even from authors no local account follows; while off the ingest path drops such a post unless a local user is mentioned in it or it answers a local status (`private`, `direct` and followers-addressed posts are never gated) (`true`/`false`, or `1`/`0`) |
| `UNREACHABLE_GRACE_SECS` | no | `0` | How long, in seconds, a host carrying a mark in `instances.unreachable_since` stays deliverable: outbound federation drops a host only once its mark is older than this window, so `0` makes any marked host unreachable at once (Pleroma's behaviour) |
| `MRF_POLICIES` | no | *(empty)* | Comma-separated MRF policy names (`noop`, `simple`, `anti_link_spam`, `normalize_markup`, `tag`, `keyword`, `hellthread`, `object_age`) run over every incoming and outgoing activity when the `mrf` instance setting carries no `policies` array of its own; while `MRF_TRANSPARENCY` is on the active names are advertised in the instance documents' `mrf_policies` (see "Moderation filters (MRF)") |
| `MRF_TRANSPARENCY` | no | `true` | Whether the enabled policies publish their moderation lists at all: the `mrf_policies` names beside the `mrf_simple`, `mrf_simple_info` and `mrf_<policy>` objects in the federation block of `GET /api/v1/instance`, `GET /api/v2/instance` and `NodeInfo`, and the domain list `GET /api/v1/instance/domain_blocks` renders. While off those publish nothing (`{"enabled": true, "mrf_policies": [], "exclusions": false}` and an empty domain-block array) and the filters keep running (`true`/`false`, or `1`/`0`) |
| `MRF_TRANSPARENCY_EXCLUSIONS` | no | *(empty)* | Comma-separated host patterns kept out of those published lists, matched with the same rules the lists themselves use, so `*.example.com` hides that entry and every entry below it. The hidden hosts stay blocked; using the feature is itself disclosed there as `"exclusions": true` |
| `UPLOAD_LIMIT_BYTES` | no | `16000000` | Maximum size of one uploaded file in a multipart request, in bytes |
| `UPLOAD_MAX_FILES` | no | `16` | Maximum number of files in one multipart request |
| `AVATAR_LIMIT_BYTES` | no | `2000000` | Maximum size of the `avatar` image of `PATCH /api/v1/accounts/update_credentials`, in bytes; a larger one is refused with 422 `{"error":"File is too large"}` |
| `BANNER_LIMIT_BYTES` | no | `4000000` | Maximum size of the `header` or `pleroma_background_image` image of `PATCH /api/v1/accounts/update_credentials`, in bytes; a larger one is refused with 422 `{"error":"File is too large"}` |
| `INBOX_MAX_BYTES` | no | `1048576` | Hard cap on one inbound ActivityPub inbox request body, in bytes; a larger body is answered 413 `{"error":"Payload too large"}` |
| `MAX_REMOTE_MENTION_LOOKUPS` | no | `10` | How many unknown-remote `@nick@domain` mentions one status may look up over the network at compose time; the lookups share a five-second budget and any that fail, time out, or pass the cap stay plain text |
| `MAX_IMAGE_PIXELS` | no | `40000000` | Maximum pixel count (width times height) of an uploaded image; the media upload endpoints reject a larger image with 422 `{"error":"Image too large"}` and an undecodable one with 422 `{"error":"Invalid image"}` |
| `HTTP_ALLOW_PRIVATE_HOSTS` | no | `false` | Disable the outbound SSRF guard so the instance may fetch private/loopback addresses, and finger `nick@host:port` handles over plain `http://` that authority, and let `POST /api/v1/pleroma/remote_interaction` finger handles naming a port other than 80/443 (otherwise refused as `Couldn't find user`), and accept Web Push subscriptions whose endpoint is a private/loopback address (with the guard on those are refused at registration) (`true`/`false`, or `1`/`0`); development and test only |
| `UPLOADS_DIR` | no | `./uploads` | Directory uploaded files are stored in (relative paths resolve against the working directory) |
| `FFMPEG_PATH` | no | `ffmpeg` | The `ffmpeg` program a video upload's preview frame is rendered with (looked up on `PATH` unless it is a path); `off` never runs it. Without a working `ffmpeg` a video has no generated preview and `preview_url` is `null`, unless the client sends a `thumbnail` part |
| `EMOJI_DIR` | no | `./emoji` | Directory emoji packs are stored in (one subdirectory per pack, with a `pack.json` and its images; served by `GET /emoji/{pack}/{file}`) |
| `MEDIA_BASE_URL` | no | `{BASE_URL}/media` | Public prefix of uploaded files' URLs, served by `GET /media/{key}` (no trailing slash) |
| `JOB_CONCURRENCY` | no | `10` | Job handlers a single worker runs at the same time |
| `JOB_QUEUES` | no | `default,federator_incoming,federator_outgoing,mailer,scheduled,background` | Comma-separated job queue names, in claim priority order |
| `PURGE_DELETED_STATUSES_DAYS` | no | `30` | Age in days after which soft-deleted local statuses are hard-deleted by the daily maintenance purge; `0` disables the purge (remote tombstones are never deleted, so a deleted remote post is never re-imported) |
| `BACKUP_VALIDITY_DAYS` | no | `30` | Age in days a finished user backup archive stays downloadable |
| `BACKUP_LIMIT_DAYS` | no | `7` | Minimum number of days between two backups of one user; a fresh backup asked for before the window passes is refused |
| `BACKUP_PURGE` | no | `true` | Whether the backup purge sweep runs at all; while off no backup archive is ever deleted by the sweep (`true`/`false`, or `1`/`0`) |
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
| `MEDIA_PROXY_CACHE_LIMIT` | no | `100000` | How many served URLs the media-proxy cache ledger keeps at most; recording a new one prunes the oldest rows past the cap, and `GET/POST /api/v1/pleroma/admin/media_proxy_caches…` list, delete and purge it |
| `MEDIA_PROXY_PURGE_URL` | no | | Origin `PURGE` cache-invalidation requests are sent to (without a trailing slash) when the admin deletes or purges cache entries; while unset no requests are sent and only the ledger rows go |
| `RICH_MEDIA_IGNORE_HOSTS` | no | *(empty)* | Comma-separated hosts rich media card fetching never visits; a link to one of them is never fetched and gets no card |
| `RICH_MEDIA_REMOTE` | no | `false` | Whether preview cards are fetched for the remote URLs statuses link to; while off the card cache stays empty (`true`/`false`, or `1`/`0`) |
| `TRANSLATION_PROVIDER` | no | `none` | Status translation: `none` turns it off (`POST /api/v1/statuses/{id}/translate` answers 503), `libretranslate` and `deepl` send the status content to that provider's API |
| `TRANSLATION_API_URL` | no | | The provider's API base URL (no trailing slash), e.g. `https://translate.example` or `https://api-free.deepl.com`; a selected provider without it is left unused, like `none` |
| `TRANSLATION_API_KEY` | no | | The provider's API key, redacted in logs; `deepl` has no unauthenticated mode and is left unused without one, a self-hosted `libretranslate` usually needs none |
| `VAPID_PRIVATE_KEY` | no | | The instance's Web Push signing key (ES256, PKCS#8 PEM), redacted in logs; when set it overwrites the pair generated into `instance_secrets` at startup, and the public half is derived from it |
| `VAPID_PUBLIC_KEY` | no | | The instance's Web Push public key (URL-safe base64 of the uncompressed P-256 point); the `server_key` the push endpoints and the `Application` entity advertise, and the `vapid_key` / `vapid_public_key` of the app and instance responses. Optional and only a cross-check: it requires `VAPID_PRIVATE_KEY` (alone it is a startup error) and must equal the key derived from it, or startup fails |
| `RATE_LIMITS_ENABLED` | no | `true` | Whether routes that mount a rate-limit bucket throttle their requests; while off they answer uncounted and without `x-ratelimit-*` headers (`true`/`false`, or `1`/`0`) |
| `RATE_LIMITS` | no | Pleroma's defaults | Bucket overrides as a comma-separated list of `bucket=max:window_secs` entries; each replaces its bucket's default and an unknown name joins the table, which otherwise is `authentication=20:5, app_account_creation=5:1800, relations_actions=10:10, relation_id_action=2:60, statuses_actions=10:10, status_id_action=2:60, password_reset=5:1800, account_confirmation_resend=5:8600, search=30:60, imports=1:3600, remote_interaction=5:60`; a bucket off the table is unlimited |
| `TRUST_PROXY_HEADERS` | no | `false` | Let `X-Forwarded-For` name the client a rate-limit window counts against — the entry `TRUSTED_PROXY_HOPS` places from the right (by default the rightmost, the hop your reverse proxy appended); while off the socket peer always does (`true`/`false`, or `1`/`0`) |
| `TRUSTED_PROXY_HOPS` | no | `1` | How many reverse proxies sit in front of the server; with `TRUST_PROXY_HEADERS` on, the client is the `X-Forwarded-For` entry that many places from the right. A header with fewer entries, or a non-IP entry there, falls back to the socket peer (whole number) |

### Split-domain deployment

`LOCAL_DOMAIN` lets the instance run at one host while its handles show
another, Mastodon's `LOCAL_DOMAIN`/`WEB_DOMAIN` split. With
`BASE_URL=https://fedi.whatever.com` and `LOCAL_DOMAIN=whatever.com`, accounts
are `@alice@whatever.com`, while the actor is
`https://fedi.whatever.com/users/alice`. The two domains need not be related
(`whatever-fedi.net` works as well as a parent domain), as long as the
operator controls the handle domain's `/.well-known/` paths.

Remote servers fetch the actor on the web host, `WebFinger`
`acct:alice@fedi.whatever.com` there and read the subject
`acct:alice@whatever.com`, then `WebFinger` `acct:alice@whatever.com` on the
handle domain and require the same `self` link. So the handle domain must
answer these paths with a `301` to the same path **and query string** on the
`BASE_URL` host:

| Path on the handle domain | Redirects to |
|---|---|
| `/.well-known/webfinger` | `https://fedi.whatever.com/.well-known/webfinger` |
| `/.well-known/host-meta` | `https://fedi.whatever.com/.well-known/host-meta` |
| `/.well-known/nodeinfo` | `https://fedi.whatever.com/.well-known/nodeinfo` |

Set the redirect up outside WiredBE, for example a Cloudflare redirect rule
with "preserve query string" on, or a reverse-proxy `redir` rule. WiredBE
answers `acct:nick@LOCAL_DOMAIN` and `acct:nick@<BASE_URL host>` alike (any
case) and names the handle domain in the `subject`; it never fetches from or
`WebFinger`s either domain itself, and treats a mention of either as local.

Limits to know before choosing a domain:

- A static `/.well-known/webfinger` file cannot do this: it can answer for one
  fixed account, not for every user, so the redirect is required.
- The handle domain can host only one fediverse server, because its
  `WebFinger` is delegated to this one.
- **The setting is effectively permanent once the instance federates.**
  Remote servers cache the handle domain with each account, so changing or
  removing `LOCAL_DOMAIN` later orphans every existing account's identity
  there. Choose it before the first remote follow.

### Default assets

wired-be serves the four default assets itself, from the `/media` tree
(`src/default_assets.rs`, answered by `GET /media/{key}` ahead of the
uploads directory), so no frontend files are needed. They are flat-colour
placeholders rendered in memory on first use; the URLs are built from
`MEDIA_BASE_URL` (default `{BASE_URL}/media`). If `MEDIA_BASE_URL` points at
a separate file host, that host must route `/default/*` to the API. An
uploaded file cannot shadow them: the `default/` key prefix is answered
before the uploads directory is read.

| Path under `MEDIA_BASE_URL` | Advertised as |
|---|---|
| `/default/avi.png` | Default avatar: the `avatar`/`avatar_static` fallback on accounts, and the `icon` fallback of a Web Push notification |
| `/default/banner.png` | Default banner: the `header`/`header_static` fallback on accounts |
| `/default/thumbnail.jpg` | Instance thumbnail on `GET /api/v1/instance` and `GET /api/v2/instance` |
| `/default/mascot.png` | Default mascot on `GET /api/v1/pleroma/mascot` |

Every setting in that table is a `pub const NAME_VAR` in `src/config.rs`
(the one place an environment variable name is spelled), and
`tests/config_docs.rs` is the guard: it scans the declarations and fails CI
when one is missing from `.env.example` or from the table above.

## Architecture

wired-be is one binary: `src/main.rs` parses the command line, loads the
environment configuration and either serves or runs the admin bootstrap;
everything else lives in `src/lib.rs` and its modules, so the integration
tests drive the same router in-process. The server federates over
`ActivityPub` (HTTP signatures, `WebFinger`, `NodeInfo`, relays, and the
MRF policy chain below), serves the Mastodon and Pleroma client APIs to
apps, and the admin APIs to staff — the feature families are enumerated
under [API coverage](#api-coverage).

The module layout (the binding decisions it follows are numbered in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)):

| Module | Contents |
|---|---|
| `src/api/` | One file per API area, each exposing `fn router() -> Router<AppState>` and composed in `src/api.rs` (decision 3): the Mastodon client API under `/api/v1` and `/api/v2`, Pleroma's extensions, the admin APIs, the `ActivityPub` routes, discovery, OAuth and streaming |
| `src/models/` | The domain structs and their SQL, one file per table family |
| `src/entities/` | The Mastodon-style JSON entities (`Account`, `Status`, `Notification`, …) built from the models |
| `src/federation/` | `ActivityPub`: actors, objects, collections, inbox dispatch, signed delivery, authorized fetch, and the MRF policies (`src/federation/mrf/`) |
| `src/jobs/` | The background-job handlers, the queue's claim loop (`Worker`) and the cron-like `scheduler` (decision 11: Postgres rows, no Redis) |
| `src/services/` | Cross-cutting operations: activity ingest, media processing and proxying, mention resolution, Web Push, VAPID, translation, the runtime config store |
| `src/auth/` | The OAuth2 bearer extractors `AuthUser`/`MaybeAuthUser` and the Mastodon scope rules (decision 9) |
| `src/extract/` | The shared request extractors, notably `ApiBody<T>` (decision 10) |
| `src/streaming/` | The `/api/v1/streaming` hub and its `NOTIFY` bridge between processes |
| `src/config.rs` | The environment-only configuration (decision 12), documented in the table above |
| `src/state.rs` | `AppState`: the one cheap-to-clone value every handler shares (pool, config, HTTP client, rate limiter, mailer, streaming hub) |
| `src/error.rs` | The one `AppError` and its JSON rendering (decision 8) |
| `src/http_client/`, `src/mailer/`, `src/oauth/`, `src/util/`, `src/uploads.rs`, `src/middleware.rs`, `src/pagination.rs`, `src/rate_limit.rs` | The helpers around them: the SSRF-guarded outbound client, the SMTP transport, scope parsing, formatting/sanitizing/passwords/keys, upload storage, the tower layers, UUIDv7 cursor pagination, the rate limiter |

The shape that falls out: every primary key is a `uuid` column holding a
`UUIDv7` minted in Rust, so `max_id`/`min_id` pagination is plain text
comparison (decision 4); times are `timestamptz` rendered Mastodon-style
(decision 5); statuses are relational rows, and anything that federates
carries an `ap_id` (decision 6); one `users` table holds local accounts and
remote mirrors alike (decision 7); background work is queue rows claimed
`FOR UPDATE SKIP LOCKED` (decision 11).

## Running

```sh
wired-be            # or the explicit `wired-be serve`
```

One process runs four cooperating tasks (`wired_be::serve`):

- the HTTP server on `BIND_ADDR`;
- the **job worker** ([`src/jobs.rs`](src/jobs.rs)): drains the `jobs`
  table in the `JOB_QUEUES` priority order with `JOB_CONCURRENCY` handlers
  at a time, claiming rows `FOR UPDATE SKIP LOCKED`, retrying failures with
  exponential backoff and honouring a peer's `Retry-After`. There is no
  separate worker binary and nothing external to run: enqueueing is
  inserting a row, and every serving process is also a consumer — start
  `wired-be` (or as many instances as you like) and the background work is
  running;
- the **periodic scheduler** ([`src/jobs/scheduler.rs`](src/jobs/scheduler.rs)):
  a ten-second tick that enqueues the recurring jobs — status expiry, the
  scheduled-status fires, the daily maintenance, token, jobs-table and
  backup sweeps, the media sweep, the host probes and the
  received-activity prune — as ordinary queue rows. Each period is one row
  deduplicated by a unique key, so any number of schedulers agree and the
  work runs once per period wherever a worker picks it up;
- the streaming `NOTIFY` listener, mirroring other processes' events onto
  this instance's WebSocket hub.

`Ctrl-C` shuts all of it down gracefully: the worker stops claiming and
waits for its in-flight jobs. An operator can also ask for a graceful
restart over the admin API (`GET /api/v1/pleroma/admin/need_reboot` says
whether a restart-requiring config change is pending;
`POST /api/v1/pleroma/admin/restart` exits the process for its supervisor).

### Installing on a server

[`scripts/installers/ubuntu-2604.sh`](scripts/installers/ubuntu-2604.sh)
installs and upgrades wired-be on a fresh Ubuntu 26.04 LTS server, run as
root. It refuses any other release, and another release gets its own file in
`scripts/installers/`. It builds `WIRED_REF` from source under an
unprivileged user, creates a peer-authenticated PostgreSQL database, writes a
hardened systemd unit and an API-only nginx reverse proxy, and by default
locks the origin to Cloudflare. That means 80/443 from Cloudflare's ranges
only, the client address taken from `CF-Connecting-IP`, and Let's Encrypt by
DNS-01. The settings of the first run are saved, so `wired-be-setup` with no
arguments upgrades later. `--help` lists every setting. Pin `WIRED_REF` to a
commit or tag for a repeatable install.

```sh
WIRED_BASE_URL=https://fedi.example.com WIRED_LOCAL_DOMAIN=example.com \
  CLOUDFLARE_API_TOKEN=... bash scripts/installers/ubuntu-2604.sh
```

## Creating the first admin

```sh
echo 'the-new-password' | \
  wired-be create-admin --nickname alice --email alice@example.org --password-stdin
```

The command reads the same environment as the server (`DATABASE_URL` and
`BASE_URL` required), applies any pending migrations, creates the account
through the very registration path `POST /api/v1/accounts` uses — the
nickname, email and password rules apply — and then sets `is_admin = true`
on the row, writing the account's `ActivityPub` key pair as it goes. The
password arrives on standard input only, never in the process arguments;
`wired-be create-admin --help` prints the contract, and `wired-be --help`
lists the commands.

Because whoever runs the command already holds database access, the account
starts email-confirmed and approved even on an instance with
`ACCOUNT_ACTIVATION_REQUIRED` or `REGISTRATIONS_APPROVAL_REQUIRED` on — the
first staff account of a closed or gated instance can log in immediately.
From there the admin API mints more staff: `POST /api/v1/pleroma/admin/users`
creates accounts through the same bypass, and the permission-group routes
grant or revoke `admin` and `moderator`.

## Moderation filters (MRF)

Every activity this instance receives or is about to send runs the policy
chain configured by the admin config store — the `:mrf` entry's fields plus
the dedicated `:mrf_simple`, `:mrf_keyword`, `:mrf_hellthread`,
`:mrf_object_age` and `:mrf_hashtag` entries, all in group `:pleroma`, read cached for 30 s and
editable at runtime through `POST /api/v1/pleroma/admin/config` — with
`MRF_POLICIES` as the fallback list of names. Stored as one document, the
chain looks like this:

```json
{
  "policies": ["simple"],
  "simple": {
    "accept": [["peer.example", "the only instance we talk to"]],
    "reject": [["*.evil.example", "bots"]],
    "reject_deletes": ["retired.example"],
    "federated_timeline_removal": ["noisy.example"],
    "followers_only": ["press.example"],
    "media_removal": ["leech.example"],
    "media_nsfw": ["nsfw.example"],
    "report_removal": ["spammer.example"],
    "avatar_removal": ["faceless.example"],
    "banner_removal": ["loud-banners.example"],
    "background_removal": ["busy-backgrounds.example"]
  }
}
```

`simple` is Pleroma's `SimplePolicy` (`lib/pleroma/web/activity_pub/mrf/simple_policy.ex`).
Every list holds domain patterns — an exact host, or `*.example.com` for that
domain and everything below it, compared case-insensitively — and each entry
may carry the operator's reason as a `[pattern, reason]` pair. The host a rule
compares is the host of the activity's `actor`:

| List | Effect |
|---|---|
| `accept` | When non-empty, only these hosts federate: every other host's activity is refused, this instance's own domain excepted |
| `reject` | Every activity from a matching host is refused, deletions included |
| `reject_deletes` | `Delete` activities from matching hosts are dropped, so what is already stored stays |
| `federated_timeline_removal` | A matching host's notes lose the public address in `to` and gain it in `cc`: they arrive as unlisted |
| `followers_only` | The public address goes and the author's followers collection takes it in `to`: the note arrives followers-only |
| `media_removal` | The `attachment` entries of matching hosts' notes are dropped |
| `media_nsfw` | Matching hosts' notes are marked `sensitive`, attachments or not |
| `report_removal` | `Flag` activities (reports) from matching hosts are dropped |
| `avatar_removal` / `banner_removal` / `background_removal` | `icon` / `image` / `background` are stripped from matching hosts' actor documents, on fetch and on an inbound `Update` |

`reject` and `accept` are also the answer `MrfConfig::rejected` gives the
inbox extractor, the delivery enqueue and the publisher, so a refused host is
turned away at the door and never sent anything, exactly as a `reject` row of
`instance_policy_domains` does. What the instance publishes about these lists
— and what it may keep back — is the transparency block at the end of this
section.

`anti_link_spam` is Pleroma's `AntiLinkSpamPolicy`
(`lib/pleroma/web/activity_pub/mrf/anti_link_spam_policy.ex`). It refuses a
`Create` of a note from a remote author whose `content` carries a link — an
`<a href>` whose classes name neither `mention` nor `hashtag` — while the
author has zero followers and zero statuses; one follower or one earlier
post, a note without links, or a local author and the activity passes.

`normalize_markup` is Pleroma's `NormalizeMarkupPolicy`
(`lib/pleroma/web/activity_pub/mrf/normalize_markup_policy.ex`). It replaces
the `content` of every note the chain sees (the embedded object of a
`Create` or `Update`) with the output of the instance's HTML sanitizer, so
what is stored and re-served is already in the shape this instance writes.

`tag` is Pleroma's `TagPolicy` (`lib/pleroma/web/activity_pub/mrf/tag_policy.ex`).
It acts on the `users.tags` of the actor an activity speaks about, which the
chain runner preloads only while the policy is enabled — an account's tags
switch these on for its own activities:

| Tag | Effect |
|---|---|
| `mrf_tag:media-force-nsfw` | The account's notes that carry attachments are marked `sensitive` |
| `mrf_tag:media-strip` | The `attachment` entries of those notes are dropped |
| `mrf_tag:force-unlisted` | A public note becomes unlisted: the public address moves from `to` into `cc` and the followers collection takes its place |
| `mrf_tag:sandbox` | A public note becomes followers-only: the public address goes and the author's followers collection joins `to` |

The last two switches are read off the tags of the **target** of a `Follow`
(the local account being followed):

| Tag | Effect |
|---|---|
| `mrf_tag:disable-remote-subscription` | `Follow` requests for the tagged account from remote users are refused |
| `mrf_tag:disable-any-subscription` | `Follow` requests for the tagged account are refused, whoever sends them |

`keyword` is Pleroma's `KeywordPolicy`
(`lib/pleroma/web/activity_pub/mrf/keyword_policy.ex`). Its three lists act
on the text of the note a `Create` or `Update` carries — the note's
`content`, `summary` and `name`, weighed before any replacement runs:

```json
{
  "policies": ["keyword"],
  "keyword": {
    "reject": ["free money", "/^BUY\\s+NOW\\b/i"],
    "federated_timeline_removal": ["politics"],
    "replace": [["bad word", "b*tch"], ["/\\bfoo\\b/i", "bar"]]
  }
}
```

Each pattern entry is a plain string (matched as a case-insensitive
substring) or a regex written between slashes, `"/pattern/"`, optionally
followed by the flags `i`, `m`, `s` or `x`. A pattern that does not compile,
or outgrows the compiled-size limit, is skipped with a warning; the rest of
its list keeps working.

| List | Effect |
|---|---|
| `reject` | The activity is refused |
| `federated_timeline_removal` | The public address moves from `to` into `cc`: the note arrives unlisted |
| `replace` | `[pattern, replacement]` pairs; every entry rewrites each of `content`, `summary` and `name` that holds a string, in configured order, replacing all matches. The replacement is written literally (no `$name` group expansion) |

`hellthread` is Pleroma's `HellthreadPolicy`
(`lib/pleroma/web/activity_pub/mrf/hellthread_policy.ex`). It counts the
mention targets in the activity's `to` and `cc` — the public address and
every `/followers` collection are not mentions — and weighs the count
strictly against two thresholds:

```json
{ "policies": ["hellthread"], "hellthread": { "delist_threshold": 10, "reject_threshold": 20 } }
```

| Key | Default | Effect |
|---|---|---|
| `delist_threshold` | `10` | More mentions than this and the public address moves from `to` into `cc`; `0` switches the stage off |
| `reject_threshold` | `20` | More mentions than this and the activity is refused; `0` switches the stage off |

`object_age` is Pleroma's `ObjectAgePolicy`
(`lib/pleroma/web/activity_pub/mrf/object_age_policy.ex`). It speaks about
`Create` activities whose embedded object `published` lies further in the
past than `threshold` seconds (default `604800`, one week; the comparison is
strict, and an unreadable or future timestamp is never old), and takes every
action named in `actions` (Pleroma's default when the key is absent:
`["delist", "strip_followers"]`; an explicit `[]` runs nothing):

```json
{ "policies": ["object_age"], "object_age": { "threshold": 604800, "actions": ["delist", "strip_followers"] } }
```

| Action | Effect |
|---|---|
| `delist` | The public address moves from `to` into `cc`: the note arrives unlisted |
| `strip_followers` | The author's followers collection leaves `to` and `cc`, so the post reaches no home feed by audience address |
| `reject` | The activity is refused, before anything rewrites |

The three policies publish their configured subsets as the `mrf_keyword`,
`mrf_hellthread` and `mrf_objectage` transparency objects beside their
`KeywordPolicy`, `HellthreadPolicy` and `ObjectAgePolicy` names in
`mrf_policies`.

`HashtagPolicy` (Pleroma's
`lib/pleroma/web/activity_pub/mrf/hashtag_policy.ex`) is not named in
`policies`: like Pleroma, every chain runs it last. It speaks about
`Create`/`Update` notes of remote actors and compares the names of the note's
`Hashtag` tags (lowercased, without the `#`) with three lists under the
`:mrf_hashtag` entry (`hashtag` key of the document); a list left out keeps
its default:

```json
{ "hashtag": { "sensitive": ["nsfw"], "reject": [], "federated_timeline_removal": [] } }
```

| List | Effect of a match |
|---|---|
| `sensitive` (default `["nsfw"]`) | The note's `sensitive` flag is set |
| `reject` | The activity is refused |
| `federated_timeline_removal` | The public address moves from `to` into `cc`: the note arrives unlisted |

It publishes the effective lists as the `mrf_hashtag` transparency object
beside the `HashtagPolicy` name in `mrf_policies`.

### Transparency: publishing the lists

The moderation the instance does, it also says it does. `MRF_TRANSPARENCY`
(default `true`) publishes the enabled chain in the `federation` block of
`GET /api/v1/instance`, `GET /api/v2/instance` and `NodeInfo`: the
Pleroma-style policy names under `mrf_policies`, every `simple` domain list
under `mrf_simple` — all eleven keys, the empty ones as the empty array, the
patterns verbatim, `*.evil.example` and all — and the operators' reasons in
the separate `mrf_simple_info` object, as
`{"reject": {"evil.example": {"reason": "bots"}}}`. The other policies add
their own `mrf_<policy>` objects beside these.

The same lists are `GET /api/v1/instance/domain_blocks`: a host on `reject`
is a domain block of severity `suspend`, one on `federated_timeline_removal`,
`followers_only`, `media_removal` or `media_nsfw` one of severity `silence`,
each entry `{domain, digest, severity, comment}` with `digest` the lowercase
hex `SHA-256` of the domain and `comment` the reason it was configured with.
`GET /api/v1/instance/peers` leaves out every host the `reject` list names,
the apex of a wildcard entry and everything below it.

With `MRF_TRANSPARENCY=false` nothing of the moderation is published: the
federation block is the three keys `{"enabled": true, "mrf_policies": [],
"exclusions": false}`, the domain-block listing is `[]`, and the filters keep
running exactly as before. `MRF_TRANSPARENCY_EXCLUSIONS` is the halfway
setting — a comma list of host patterns, matched with the same rules the
lists themselves use, so `*.example.com` hides that entry and every entry
below it — and an excluded host is simply absent from the published lists and
from `mrf_simple_info`, reason included. Using it is itself disclosed there
as `"exclusions": true`, whether or not anything matched, so readers know the
lists they are shown may not be the whole story. Exclusions hide what is
published, never what is enforced: an excluded host is still refused its
traffic, still absent from the domain-block listing, and still out of the
peer list.

## OAuth

Third-party clients use the authorization-code flow at `/oauth/authorize`
and `POST /oauth/token`. The consent screen lives at
`{FRONTEND_BASE_URL}/oauth/authorize`: a browser `GET` of `/oauth/authorize`
validates the request (`response_type=code`, a registered `client_id` and
`redirect_uri`, scopes inside the registration) and answers
`302 Location: {FRONTEND_BASE_URL}/oauth/authorize?<the same query>`, or the
JSON 400 error with no redirect when validation fails. The server renders no
login or consent HTML, and a form `POST` (`authorization[name]` and friends)
is a 400. The frontend logs in with the password grant and then uses the JSON
half of the path:

- `GET /oauth/authorize?response_type=code&client_id=…&redirect_uri=…&scope=…&state=…`
  with `Accept: application/json` answers
  `{"app":{"name","website"},"scopes":[…],"redirect_uri","state"}`, or a 400
  `{"error":"invalid_request"|"invalid_client"|"invalid_scope","error_description"}`.
- `POST /oauth/authorize` with `Authorization: Bearer <token>` and a JSON body
  `{client_id, redirect_uri, scope, state, approve}` mints the code for the
  token's user and answers `{"redirect_uri":"…?code=…&state=…"}` (or
  `{"code"}` for `urn:ietf:wg:oauth:2.0:oob`); `approve: false` answers the
  redirect with `error=access_denied`. A missing or dead token is a 401, and a
  token whose scopes do not cover the request a 403 `invalid_scope`.

**The approving token's app must be trusted:** the JSON `POST` answers 403
`unauthorized_client` for a token issued to any other app, so an app holding a
user token cannot mint codes for other apps. An app is trusted automatically
when every one of its `redirect_uri`s is on the origin (scheme, host and port)
of `FRONTEND_BASE_URL`, at registration (`POST /api/v1/apps`) and, for apps
registered earlier, at startup; the out-of-band URN never counts. That covers
the frontend's per-browser app. Other first-party tools are marked with
`PATCH /api/v1/pleroma/admin/oauth_app/{id}` and `trusted`. The JSON `GET`
never depends on trust.

## API coverage

Wired replicates the HTTP interface of **Pleroma 2.10**: paths, parameters,
JSON shapes, status codes and headers match what Pleroma answers, because
clients and peer instances are written against that surface and cannot be
changed. Four families make it up:

- the **Mastodon client API** under `/api/v1` and `/api/v2` — apps, accounts,
  statuses, timelines, notifications (flat and grouped), conversations, lists,
  filters (v1 and v2: grouped keywords, `warn`/`hide`/`blur` actions and
  single-status filters over one storage), markers, media, polls, search,
  domain blocks, follow requests,
  endorsements, featured hashtags (`/api/v1/featured_tags`, up to ten per
  account, federated as the actor's `featuredTags` collection), scheduled
  statuses, push subscriptions, announcements, reports, trends, the directory
  and the instance documents;
- **Pleroma's own extensions** under `/api/v1/pleroma` and `/api/pleroma` —
  emoji and emoji packs, reactions, chats, conversations, bookmarks folders,
  scrobbles, settings, the mascot, MFA, aliases, moves,
  account deletion, the CSV imports, backups, OAuth token management,
  `frontend_configurations`, `healthcheck`, `captcha` and `federation_status`;
- the **admin API** — Pleroma's `/api/v1/pleroma/admin` (users, permission
  groups, invites, statuses, chats, instances, reports, relays, rules,
  announcements, webhooks, config, media-proxy caches, the
  moderation log and stats) and Mastodon's `/api/v1/admin` and
  `/api/v2/admin` (accounts and the report queue); the reporter's own
  `/api/v0/pleroma/reports` reads (`read:reports`) are a user endpoint, not
  an admin one;
- **federation and discovery** — the `ActivityPub` actors, objects, activities,
  inboxes and outboxes (`/users/…`, `/objects/…`, `/activities/…`, `/inbox`,
  `/relay`, `/internal/fetch`), `WebFinger`, host-meta, `NodeInfo`, the media and
  emoji files, the proxy, the OAuth endpoints
  and the streaming WebSocket.

`src/api/route_inventory.rs` is the table of that surface: one row per route,
with its method, its `:name`-style path and the credential its handler demands
(`Public`, `User` or `Admin`). `tests/api/route_inventory.rs` is the gate — it
requests every row without credentials and fails if the router does not match
the path (the fallback's `x-wired-fallback: 1` marker answers) or if a `User`
or `Admin` row answers anything other than 401 or 403. A route that exists in
Pleroma but is missing here, or a route here that the router does not serve,
fails CI.

Routes Pleroma has and Wired deliberately does not:

| Route | Why it is out |
|---|---|
| `POST /api/v1/pleroma/uploader_callback/:upload_path` | The per-chunk callback of Pleroma's frontend-driven resumable uploader (`UploaderController.callback`). Wired's upload endpoints take the whole file in one request, so a callback has nothing to report. |
| Phoenix's dev and test routes — `forward /dev/mailbox`, and the `/test/api/:action` and `/test/authenticated_api/:action` reads of `AuthTestController` | The reference mounts them only when its environment is `:dev` or `:test`: a mailbox preview for sent mail and plugs that exercise its own router. Wired's tests drive the real router. |
| `/phoenix/live_dashboard/*path`, and the `/pleroma/live_dashboard` and `/pleroma/oban` pages it redirects to | `Phoenix.LiveDashboard` and `Oban.Web.Router`: HTML UIs over BEAM process and job metrics, with no meaning outside that runtime. |
| The SPA fallbacks `GET /:maybe_nickname_or_id` and `GET /*path` (and the `OPTIONS /*path` pair) | They hand an unknown path to the frontend's JavaScript router, or answer its preflight. Wired is API-only (see the top of this file), so an unmatched path is the JSON 404 — under `/api` Pleroma's own `api_not_implemented`, elsewhere its plain "Not found". |
| `GET /users/:nickname/feed` (and its `.atom`/`.rss` forms) and `GET /tags/:tag` | Atom/RSS syndication, not API. Pleroma serves them from its HTML/XML pipeline; Wired is API-only, so the frontend owns HTML and feeds and these paths fall through to the JSON 404. `GET /users/:nickname` still negotiates the `ActivityPub` actor JSON. |
| `GET /api/v1/pleroma/admin/frontends` and `POST /api/v1/pleroma/admin/frontends/install` | Pleroma's `FrontendController`: list and install web UI builds into a directory Pleroma then serves. Wired serves no frontend, so an installed build would never be reachable and the installer would only add an outbound download and archive extraction; both routes fall through to the JSON 404. |
| `GET/POST /frontend_switcher` | Pleroma's `FrontendSwitcherController`: an HTML radio-list of installed frontends whose POST sets a year-long `preferred_frontend` cookie and redirects to `/`. Wired is API-only and serves no frontend at `/`, so the cookie would select nothing; both methods fall through to the JSON 404. |
| `GET /api/v1/pleroma/preferred_frontend/available` and `PUT /api/v1/pleroma/preferred_frontend` | Pleroma's `FrontendSettingsController`: list the frontends the operator made pickable and store the account's pick in a year-long `preferred_frontend` cookie. Wired serves no frontend, so nothing would read the choice; both routes fall through to the JSON 404 (`x-wired-fallback: 1`). |
| `GET /embed/:id` and `GET /notice/:id/embed_player` | Pleroma's `EmbedController.show` and `OStatus.notice_player`: server-rendered HTML pages (a status card and a one-attachment media player) served without `X-Frame-Options` so other sites can iframe them. Wired is API-only and serves no framable page; both paths fall through to the JSON 404 (`x-wired-fallback: 1`). A client that wants an embed renders its own from `GET /api/v1/statuses/:id`. `GET /notice/:id` still negotiates the `ActivityPub` Note JSON. |
| `GET /registration/:token` | Pleroma's invite-link page, which redirects to its own frontend's registration page. Wired is API-only: the invite letter links straight to `{FRONTEND_BASE_URL}/registration/{token}` (the frontend registers through `POST /api/v1/accounts` with the `token` field), so this path falls through to the JSON 404. |
| `GET/POST /ostatus_subscribe` | Pleroma's remote-follow login page: it takes a username and password from an anonymous client, with no rate limit and without the second factor the OAuth password grant demands, so it is a credential oracle and an MFA bypass. Wired serves no password form outside the throttled `POST /oauth/token`; remote follow lands on the frontend's `/authorize_interaction` page and follows through `POST /api/v1/accounts/:id/follow` with the user's OAuth token. Both methods fall through to the JSON 404 (`x-wired-fallback: 1`). |
| `GET/POST /main/ostatus` | Pleroma's server-rendered remote-subscribe form (`router.ex` `/main/ostatus`): an HTML page that takes a local nickname and a remote handle and redirects to the remote instance's subscribe template. Wired is API-only; the same lookup is `POST /api/v1/pleroma/remote_interaction` (JSON) and the page flow lives on the frontend's `/authorize_interaction`. The `pleroma:get:main/ostatus` feature flag is no longer advertised, and both methods fall through to the JSON 404 (`x-wired-fallback: 1`). |
| `GET /oauth/mfa` and `POST /oauth/mfa/verify` | Pleroma's server-rendered second-factor page and its form post (`MFAController.show`/`verify`), which finished an authorization-code flow parked behind MFA. Wired is API-only: the frontend prompts in-app when the password grant answers `mfa_required` and posts the code to the JSON `POST /oauth/mfa/challenge`, which stays. `POST /oauth/mfa/verify` falls through to the JSON 404 (`x-wired-fallback: 1`), and `GET /oauth/mfa` is read as the external-provider entry `/oauth/:provider` and answers its JSON 404 "Unknown provider". An MFA-enabled account that signs in through an external provider's callback is refused with a 403, since there is no page to detour to. |
| `GET /manifest.json` | The PWA manifest (`ManifestController`). It is the frontend's asset and the backend serves no HTML that would link it, so the path falls through to the JSON 404; a frontend that wants to be installable ships its own manifest. |
| `GET /user_exists` and `GET /check_password` | Pleroma's XMPP-server (`extauth` http) integration, off by default behind a flag. `check_password` takes `user` and `pass` in the query string (so passwords land in proxy and access logs) and checks the password alone: no rate limit and no second factor, so it is a brute-force oracle and an MFA bypass, and `user_exists` enumerates accounts. Outside the client API and `ActivityPub` scope, so both paths fall through to the JSON 404 (`x-wired-fallback: 1`) whatever the configuration. |
| `GET /authorize_interaction` | Pleroma's remote-interaction landing page: given a `uri` (actor URL, status URL or handle) it fetches and stores the remote actor or status and redirects the browser to it. Served anonymously that is an unthrottled amplifier that lets any caller make the server fetch and persist remote objects of their choosing (Mastodon's equivalent requires a logged-in user). The frontend serves `/authorize_interaction` (the `WebFinger` subscribe template points at it); the backend path falls through to the JSON 404 (`x-wired-fallback: 1`). |
| `GET /mailer/unsubscribe/:token` | Pleroma's `SubscriptionController`: an HTML page that clears a user's `email_notifications` flag from an HMAC-signed link. Wired's mailer puts no unsubscribe link in any letter, so nothing mints the tokens, and a state-changing GET on non-expiring tokens is a liability. Wired is API-only: the path falls through to the JSON 404 (`x-wired-fallback: 1`), and a client toggles the flag through the account settings API (`email_notifications`). |
| The Shout channels socket (`GET /socket`) | Pleroma's browser-notification WebSocket. Wired delivers the same events on `/api/v1/streaming` and over Web Push. |

## Development

```sh
cp .env.example .env   # then edit DATABASE_URL
cargo run              # wired-be (= `wired-be serve`): HTTP server + job worker + scheduler on BIND_ADDR
cargo run -- create-admin --nickname alice --email alice@example.org --password-stdin
cargo test             # the whole suite; `--test config_docs` alone is the config guard
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

### Models used

So far, these LLMs have been used:

* Opus 5.5
* Sonnet 5.5
* Qwen3.8 Flash next
* DeepSeek V4 Pro

