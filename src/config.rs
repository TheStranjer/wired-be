//! Runtime configuration, read from environment variables.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    net::{AddrParseError, SocketAddr},
    path::PathBuf,
};

use url::Url;

use crate::{
    extract::multipart::UploadLimits,
    jobs::DEFAULT_JOB_QUEUES,
    rate_limit::{self, RateLimit},
};

/// Environment variable holding the PostgreSQL connection URL.
pub const DATABASE_URL_VAR: &str = "DATABASE_URL";
/// Environment variable holding the address to listen on.
pub const BIND_ADDR_VAR: &str = "BIND_ADDR";
/// Environment variable holding the public origin of this instance.
pub const BASE_URL_VAR: &str = "BASE_URL";
/// Environment variable holding the origin the frontend pages are served from.
pub const FRONTEND_BASE_URL_VAR: &str = "FRONTEND_BASE_URL";
/// Environment variable holding the instance display name.
pub const INSTANCE_NAME_VAR: &str = "INSTANCE_NAME";
/// Environment variable holding the instance description.
pub const INSTANCE_DESCRIPTION_VAR: &str = "INSTANCE_DESCRIPTION";
/// Environment variable holding the admin contact email.
pub const CONTACT_EMAIL_VAR: &str = "CONTACT_EMAIL";
/// Environment variable holding the comma-separated content languages.
pub const INSTANCE_LANGUAGES_VAR: &str = "INSTANCE_LANGUAGES";
/// Environment variable holding the maximum status length in characters.
pub const MAX_STATUS_CHARS_VAR: &str = "MAX_STATUS_CHARS";
/// Environment variable holding the maximum media attachments per status.
pub const MAX_MEDIA_ATTACHMENTS_VAR: &str = "MAX_MEDIA_ATTACHMENTS";
/// Environment variable holding the maximum options per poll.
pub const MAX_POLL_OPTIONS_VAR: &str = "MAX_POLL_OPTIONS";
/// Environment variable holding the maximum length of one poll option in
/// characters.
pub const MAX_POLL_OPTION_CHARS_VAR: &str = "MAX_POLL_OPTION_CHARS";
/// Environment variable holding the shortest poll `poll[expires_in]`, in
/// seconds.
pub const MIN_POLL_EXPIRATION_VAR: &str = "MIN_POLL_EXPIRATION";
/// Environment variable holding the longest poll `poll[expires_in]`, in
/// seconds.
pub const MAX_POLL_EXPIRATION_VAR: &str = "MAX_POLL_EXPIRATION";
/// Environment variable holding the maximum chat message length in
/// characters.
pub const MAX_CHAT_MESSAGE_CHARS_VAR: &str = "MAX_CHAT_MESSAGE_CHARS";
/// Environment variable holding the maximum media description length in
/// characters.
pub const MAX_MEDIA_DESCRIPTION_VAR: &str = "MAX_MEDIA_DESCRIPTION";
/// Environment variable holding the maximum display-name length in
/// characters.
pub const MAX_DISPLAY_NAME_CHARS_VAR: &str = "MAX_DISPLAY_NAME_CHARS";
/// Environment variable holding the maximum profile note (bio) length in
/// characters.
pub const MAX_BIO_CHARS_VAR: &str = "MAX_BIO_CHARS";
/// Environment variable holding the maximum number of profile metadata
/// fields (`fields_attributes`).
pub const MAX_PROFILE_FIELDS_VAR: &str = "MAX_PROFILE_FIELDS";
/// Environment variable holding the maximum length of a report's comment
/// in characters.
pub const MAX_REPORT_COMMENT_VAR: &str = "MAX_REPORT_COMMENT";
/// Environment variable holding the number of statuses one account may pin
/// to its profile.
pub const MAX_PINNED_STATUSES_VAR: &str = "MAX_PINNED_STATUSES";
/// Environment variable holding the shortest `expires_in` a status may ask
/// for, in seconds.
pub const MIN_EXPIRATION_SECS_VAR: &str = "MIN_EXPIRATION_SECS";
/// Environment variable holding how far in the future a
/// `POST /api/v1/statuses` `scheduled_at` must be, in seconds.
pub const MIN_SCHEDULED_SECS_VAR: &str = "MIN_SCHEDULED_SECS";
/// Environment variable holding how many pending scheduled rows one account
/// may hold.
pub const MAX_SCHEDULED_PER_USER_VAR: &str = "MAX_SCHEDULED_PER_USER";
/// Environment variable holding how many pending scheduled rows one account
/// may hold for one UTC day.
pub const MAX_SCHEDULED_PER_USER_PER_DAY_VAR: &str = "MAX_SCHEDULED_PER_USER_PER_DAY";
/// Environment variable holding the maximum size of one uploaded file.
pub const UPLOAD_LIMIT_BYTES_VAR: &str = "UPLOAD_LIMIT_BYTES";
/// Environment variable holding the maximum pixel count of an uploaded
/// image.
pub const MAX_IMAGE_PIXELS_VAR: &str = "MAX_IMAGE_PIXELS";
/// Environment variable holding the maximum number of files per request.
pub const UPLOAD_MAX_FILES_VAR: &str = "UPLOAD_MAX_FILES";
/// Environment variable holding the maximum size of a profile avatar image,
/// in bytes.
pub const AVATAR_LIMIT_BYTES_VAR: &str = "AVATAR_LIMIT_BYTES";
/// Environment variable holding the maximum size of a profile banner (or
/// background) image, in bytes.
pub const BANNER_LIMIT_BYTES_VAR: &str = "BANNER_LIMIT_BYTES";
/// Environment variable holding the maximum size of one inbound
/// `ActivityPub` inbox request body, in bytes.
pub const INBOX_MAX_BYTES_VAR: &str = "INBOX_MAX_BYTES";
/// Environment variable holding how many unknown-remote `@nick@domain`
/// mentions one status may look up over the network.
pub const MAX_REMOTE_MENTION_LOOKUPS_VAR: &str = "MAX_REMOTE_MENTION_LOOKUPS";
/// Environment variable holding whether outbound HTTP may reach private
/// hosts (disabling the SSRF guard).
pub const HTTP_ALLOW_PRIVATE_HOSTS_VAR: &str = "HTTP_ALLOW_PRIVATE_HOSTS";
/// Environment variable holding the directory uploaded files are stored in.
pub const UPLOADS_DIR_VAR: &str = "UPLOADS_DIR";
/// Environment variable holding the directory emoji packs are stored in.
pub const EMOJI_DIR_VAR: &str = "EMOJI_DIR";
/// Environment variable holding the public prefix of uploaded files' URLs.
pub const MEDIA_BASE_URL_VAR: &str = "MEDIA_BASE_URL";
/// Environment variable holding whether registration may omit the email
/// address.
pub const ACCOUNTS_EMAIL_OPTIONAL_VAR: &str = "ACCOUNTS_EMAIL_OPTIONAL";
/// Environment variable holding the minimum length of a registration
/// password.
pub const MIN_PASSWORD_LENGTH_VAR: &str = "MIN_PASSWORD_LENGTH";
/// Environment variable holding the lifetime of issued OAuth access
/// tokens, in seconds.
pub const OAUTH_TOKEN_TTL_SECS_VAR: &str = "OAUTH_TOKEN_TTL_SECS";
/// Environment variable holding the lifetime of a password-reset token,
/// in seconds.
pub const PASSWORD_RESET_TTL_SECS_VAR: &str = "PASSWORD_RESET_TTL_SECS";
/// Environment variable holding whether accounts must confirm their email
/// address: a new account before it may log in, and a changed address before
/// it is carried as confirmed.
///
/// The instance endpoints report it as `account_activation_required` and
/// `NodeInfo` as `accountActivationRequired`.
pub const ACCOUNT_ACTIVATION_REQUIRED_VAR: &str = "ACCOUNT_ACTIVATION_REQUIRED";
/// Environment variable holding whether accounts may delete their own
/// account through `POST /api/pleroma/delete_account`.
pub const ALLOW_ACCOUNT_DELETION_VAR: &str = "ALLOW_ACCOUNT_DELETION";
/// Environment variable holding whether the `MongooseIM` auth endpoints
/// (`GET /user_exists`, `GET /check_password`) are served.
pub const MONGOOSEIM_ENABLED_VAR: &str = "MONGOOSEIM_ENABLED";
/// Environment variable holding whether a captcha provider is configured
/// for `GET /api/v1/pleroma/captcha`.
pub const CAPTCHA_ENABLED_VAR: &str = "CAPTCHA_ENABLED";
/// Environment variable holding whether `GET /api/v1/pleroma/healthcheck`
/// reports pool and liveness data.
pub const HEALTHCHECK_ENABLED_VAR: &str = "HEALTHCHECK_ENABLED";
/// Environment variable holding whether the instance accepts new local
/// registrations; `NodeInfo` reports it as `openRegistrations`.
pub const REGISTRATIONS_OPEN_VAR: &str = "REGISTRATIONS_OPEN";
/// Environment variable holding whether registration invite tokens
/// substitute for open registrations on `POST /api/v1/accounts`; the
/// instance endpoints report it as `invites_enabled`.
pub const INVITES_ENABLED_VAR: &str = "INVITES_ENABLED";
/// Environment variable holding whether a new account waits for staff
/// approval before it may log in; the instance endpoints report it as
/// `approval_required`.
pub const REGISTRATIONS_APPROVAL_REQUIRED_VAR: &str = "REGISTRATIONS_APPROVAL_REQUIRED";
/// Environment variable holding whether anonymous requests may search
/// statuses at `GET /api/v1/search` and `GET /api/v2/search`.
pub const SEARCH_STATUSES_UNAUTHENTICATED_VAR: &str = "SEARCH_STATUSES_UNAUTHENTICATED";
/// Environment variable holding the comma-separated areas of
/// [`RESTRICT_UNAUTHENTICATED_AREAS`] an anonymous request may not read.
pub const RESTRICT_UNAUTHENTICATED_VAR: &str = "RESTRICT_UNAUTHENTICATED";
/// The areas [`RESTRICT_UNAUTHENTICATED_VAR`] accepts.
///
/// Pleroma's `:restrict_unauthenticated` groups (`timelines`, `profiles`,
/// `activities`), each joined to its half of the world with an underscore —
/// the flat spelling of Pleroma's nested keyword list in a single
/// environment variable.
pub const RESTRICT_UNAUTHENTICATED_AREAS: [&str; 6] = [
    "timelines_local",
    "timelines_federated",
    "profiles_local",
    "profiles_remote",
    "activities_local",
    "activities_remote",
];
/// Environment variable holding the application-wide HMAC signing secret
/// (`SECRET_KEY_BASE` in Pleroma's config).
///
/// When unset or blank, [`serve`](crate::serve) generates a random one per
/// boot and warns, so links minted earlier stop validating after a restart.
pub const SECRET_KEY_BASE_VAR: &str = "SECRET_KEY_BASE";
/// Environment variable holding the external OAuth login providers, as the
/// JSON text of an [`OauthProvider`] array.
pub const OAUTH_PROVIDERS_VAR: &str = "OAUTH_PROVIDERS";
/// Environment variable holding the comma-separated list of frontends
/// clients may pick at `PUT /api/v1/pleroma/preferred_frontend`.
pub const AVAILABLE_FRONTENDS_VAR: &str = "AVAILABLE_FRONTENDS";
/// Environment variable holding the job worker's concurrent handler count.
pub const JOB_CONCURRENCY_VAR: &str = "JOB_CONCURRENCY";
/// Environment variable holding the comma-separated job queue names.
pub const JOB_QUEUES_VAR: &str = "JOB_QUEUES";
/// Environment variable holding the age in days after which soft-deleted
/// local statuses are hard-deleted by the daily maintenance purge; `0`
/// disables the purge.
pub const PURGE_DELETED_STATUSES_DAYS_VAR: &str = "PURGE_DELETED_STATUSES_DAYS";
/// Environment variable holding the age in days a finished user backup
/// archive stays downloadable.
pub const BACKUP_VALIDITY_DAYS_VAR: &str = "BACKUP_VALIDITY_DAYS";
/// Environment variable holding the minimum number of days between two
/// backups of one user.
pub const BACKUP_LIMIT_DAYS_VAR: &str = "BACKUP_LIMIT_DAYS";
/// Environment variable holding whether the backup purge sweep runs at all.
pub const BACKUP_PURGE_VAR: &str = "BACKUP_PURGE";
/// Environment variable holding how many seconds the streaming WebSocket
/// waits between two heartbeat pings of one socket.
pub const STREAMING_PING_INTERVAL_SECS_VAR: &str = "STREAMING_PING_INTERVAL_SECS";
/// Environment variable holding the SMTP relay host; mail is disabled while
/// it is unset.
pub const SMTP_HOST_VAR: &str = "SMTP_HOST";
/// Environment variable holding the SMTP relay port.
pub const SMTP_PORT_VAR: &str = "SMTP_PORT";
/// Environment variable holding the SMTP relay login name.
pub const SMTP_USERNAME_VAR: &str = "SMTP_USERNAME";
/// Environment variable holding the SMTP relay login secret.
pub const SMTP_PASSWORD_VAR: &str = "SMTP_PASSWORD";
/// Environment variable holding the SMTP encryption mode.
pub const SMTP_TLS_VAR: &str = "SMTP_TLS";
/// Environment variable holding the `From:` address of every sent email.
pub const MAIL_FROM_VAR: &str = "MAIL_FROM";
/// Environment variable holding whether the media proxy rewrites remote
/// media URLs (see [`crate::services::media_proxy`]).
pub const MEDIA_PROXY_ENABLED_VAR: &str = "MEDIA_PROXY_ENABLED";
/// Environment variable holding the HMAC-SHA256 secret the media proxy
/// signs its URLs with; required, at least
/// [`MIN_MEDIA_PROXY_SECRET_CHARS`] characters, whenever the proxy is
/// enabled.
pub const MEDIA_PROXY_SECRET_VAR: &str = "MEDIA_PROXY_SECRET";
/// Environment variable overriding the base URL media proxy links are
/// built from (e.g. a CDN in front of this instance's `/proxy` routes).
pub const MEDIA_PROXY_BASE_URL_VAR: &str = "MEDIA_PROXY_BASE_URL";
/// Environment variable holding the hard byte cap for one proxied media
/// fetch.
pub const MEDIA_PROXY_MAX_BYTES_VAR: &str = "MEDIA_PROXY_MAX_BYTES";
/// Environment variable holding the comma-separated list of hosts the
/// media proxy never proxies.
pub const MEDIA_PROXY_BLACKLIST_VAR: &str = "MEDIA_PROXY_BLACKLIST";
/// Environment variable holding the comma-separated list of hosts rich
/// media card fetching never visits.
pub const RICH_MEDIA_IGNORE_HOSTS_VAR: &str = "RICH_MEDIA_IGNORE_HOSTS";
/// Environment variable holding whether preview cards are fetched for the
/// remote URLs statuses link to.
pub const RICH_MEDIA_REMOTE_VAR: &str = "RICH_MEDIA_REMOTE";
/// Environment variable holding the instance's VAPID private key for Web
/// Push, the ES256 signing key as PKCS#8 PEM.
///
/// When set it takes the pair stored in `instance_secrets` over at
/// startup. Never logged; redacted in `Debug` output.
pub const VAPID_PRIVATE_KEY_VAR: &str = "VAPID_PRIVATE_KEY";
/// Environment variable holding the instance's VAPID public key, the
/// URL-safe base64 of the uncompressed P-256 point.
///
/// The push endpoints advertise it as `server_key`; with no
/// [`VAPID_PRIVATE_KEY_VAR`] beside it only the advertised half is
/// overridden. Not secret: the instance endpoints publish it.
pub const VAPID_PUBLIC_KEY_VAR: &str = "VAPID_PUBLIC_KEY";
/// Environment variable holding which status-translation provider the
/// instance uses: `none` (the default), `libretranslate` or `deepl`.
pub const TRANSLATION_PROVIDER_VAR: &str = "TRANSLATION_PROVIDER";
/// Environment variable holding the base URL of the configured translation
/// provider's API (no trailing slash); a provider without it stays unused.
pub const TRANSLATION_API_URL_VAR: &str = "TRANSLATION_API_URL";
/// Environment variable holding the translation provider's API key (never
/// logged; redacted in `Debug` output). `deepl` needs it, a
/// keyless `libretranslate` server does not.
pub const TRANSLATION_API_KEY_VAR: &str = "TRANSLATION_API_KEY";
/// Environment variable holding whether outgoing actor `GET`s are signed
/// with the instance actor's key (authorized-fetch mode on the peers).
pub const SIGN_FETCHES_VAR: &str = "SIGN_FETCHES";
/// Environment variable holding whether incoming `GET`s to the
/// `ActivityPub` actor routes must carry a valid HTTP signature.
pub const AUTHORIZED_FETCH_VAR: &str = "AUTHORIZED_FETCH";
/// Environment variable holding how far up a fetched thread
/// `federator.backfill` walks to link a remote reply to its ancestors
/// (see [`crate::federation::fetch_object`]).
pub const THREAD_FETCH_DEPTH_VAR: &str = "THREAD_FETCH_DEPTH";
/// Environment variable holding how many remote object fetches one
/// incoming activity may trigger at most
/// (see [`crate::federation::fetch_object`]).
pub const MAX_FETCHES_PER_ACTIVITY_VAR: &str = "MAX_FETCHES_PER_ACTIVITY";
/// Environment variable holding whether this instance federates at all:
/// while off the `ActivityPub` inbox routes answer 403 and outbound
/// delivery queues nothing.
pub const FEDERATION_ENABLED_VAR: &str = "FEDERATION_ENABLED";
/// Environment variable holding whether uninvited public remote posts are
/// stored, as `ACCEPT_UNSOLICITED_PUBLIC`.
///
/// While off, public and unlisted statuses from authors no local account
/// follows are dropped at ingestion, unless a local user is mentioned or
/// the status replies to a local one.
pub const ACCEPT_UNSOLICITED_PUBLIC_VAR: &str = "ACCEPT_UNSOLICITED_PUBLIC";
/// Environment variable holding how long a host whose delivery failed stays
/// treated as reachable, in seconds, as `UNREACHABLE_GRACE_SECS`.
///
/// [`crate::models::instance::is_host_unreachable`] reads a host unreachable
/// only when its `unreachable_since` mark is older than this span, so `0` —
/// the default — makes any marked host unreachable at once.
pub const UNREACHABLE_GRACE_SECS_VAR: &str = "UNREACHABLE_GRACE_SECS";
/// Environment variable holding the comma-separated list of MRF policy
/// names enabled without any `mrf` instance setting: the fallback of
/// [`crate::federation::mrf`]'s configuration load.
pub const MRF_POLICIES_VAR: &str = "MRF_POLICIES";
/// Environment variable holding whether the moderation lists of the
/// enabled policies are published at all.
///
/// The `mrf_policies` names beside every policy's transparency objects in
/// the federation block of `GET /api/v1/instance`, `GET /api/v2/instance`
/// and `NodeInfo`, and the list
/// `GET /api/v1/instance/domain_blocks` renders, both come from this
/// switch (see [`crate::federation::mrf`]).
pub const MRF_TRANSPARENCY_VAR: &str = "MRF_TRANSPARENCY";
/// Environment variable holding the comma-separated list of host patterns
/// kept out of those published lists; using it is disclosed there as
/// `"exclusions": true`.
pub const MRF_TRANSPARENCY_EXCLUSIONS_VAR: &str = "MRF_TRANSPARENCY_EXCLUSIONS";
/// Environment variable holding whether routes that mount a rate-limit
/// bucket throttle their requests.
pub const RATE_LIMITS_ENABLED_VAR: &str = "RATE_LIMITS_ENABLED";
/// Environment variable holding the rate limiter's bucket overrides.
pub const RATE_LIMITS_VAR: &str = "RATE_LIMITS";
/// Environment variable holding whether the leftmost `X-Forwarded-For`
/// entry may name the client for rate limiting.
pub const TRUST_PROXY_HEADERS_VAR: &str = "TRUST_PROXY_HEADERS";
/// Listen address used when `BIND_ADDR` is unset.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:4000";
/// Instance name used when `INSTANCE_NAME` is unset.
pub const DEFAULT_INSTANCE_NAME: &str = "Wired";
/// Description used when `INSTANCE_DESCRIPTION` is unset.
pub const DEFAULT_INSTANCE_DESCRIPTION: &str = "A Pleroma-compatible instance";
/// Language list used when `INSTANCE_LANGUAGES` is unset.
pub const DEFAULT_INSTANCE_LANGUAGES: &str = "en";
/// Status length used when `MAX_STATUS_CHARS` is unset.
pub const DEFAULT_MAX_STATUS_CHARS: usize = 5000;
/// Attachment limit used when `MAX_MEDIA_ATTACHMENTS` is unset.
pub const DEFAULT_MAX_MEDIA_ATTACHMENTS: usize = 4;
/// Poll option limit used when `MAX_POLL_OPTIONS` is unset.
pub const DEFAULT_MAX_POLL_OPTIONS: usize = 20;
/// Poll option length used when `MAX_POLL_OPTION_CHARS` is unset.
///
/// Pleroma's `[:instance, :poll_limits, :max_option_chars]` ships as 200;
/// `POST /api/v1/statuses` refuses a poll carrying an empty option or an
/// option longer than this with 422 `{"error":"Poll options cannot be longer
/// than 200 characters"}` (the message names this value).
pub const DEFAULT_MAX_POLL_OPTION_CHARS: usize = 200;
/// Shortest poll expiration used when `MIN_POLL_EXPIRATION` is unset.
///
/// The 300 seconds the parent ticket fixes for
/// `[:instance, :poll_limits, :min_expiration]` (Pleroma's own shipped
/// literal is `0`); `POST /api/v1/statuses` refuses a shorter
/// `poll[expires_in]` with 422 `{"error":"Expiration date is too soon"}`.
pub const DEFAULT_MIN_POLL_EXPIRATION: usize = 300;
/// Longest poll expiration used when `MAX_POLL_EXPIRATION` is unset.
///
/// The `31_536_000` seconds (one year) the parent ticket fixes over Pleroma's
/// `[:instance, :poll_limits, :max_expiration]` literal `365 * 24 * 60 * 60`,
/// which is the same number; `POST /api/v1/statuses` refuses a longer
/// `poll[expires_in]` with 422 `{"error":"Expiration date is too far in the
/// future"}`.
pub const DEFAULT_MAX_POLL_EXPIRATION: usize = 31_536_000;
/// Chat message length used when `MAX_CHAT_MESSAGE_CHARS` is unset.
///
/// The 5000 characters of Pleroma's `[:instance, :chat_limit]` default —
/// the same number the chat model's storage backstop keeps, pinned together
/// by a test in this module.
pub const DEFAULT_MAX_CHAT_MESSAGE_CHARS: usize = 5_000;
/// Media description length used when `MAX_MEDIA_DESCRIPTION` is unset.
///
/// It is the 1500 characters Mastodon caps an attachment's alt text at
/// (`MAX_ALTERNATIVE_TEXT_SIZE` in `config/settings.yml`);
/// `PUT /api/v1/media/{id}` refuses a longer `description`.
pub const DEFAULT_MAX_MEDIA_DESCRIPTION: usize = 1_500;
/// Display-name length used when `MAX_DISPLAY_NAME_CHARS` is unset.
///
/// Mastodon caps `display_name` at 50 characters
/// (`Account#display_name`); `PATCH /api/v1/accounts/update_credentials`
/// refuses a longer one with 422.
pub const DEFAULT_MAX_DISPLAY_NAME_CHARS: usize = 50;
/// Bio length used when `MAX_BIO_CHARS` is unset.
///
/// The 5000 characters of Pleroma's `[:limiter, :user_bio_length]`;
/// `PATCH /api/v1/accounts/update_credentials` refuses a longer `note`
/// with 422.
pub const DEFAULT_MAX_BIO_CHARS: usize = 5_000;
/// Profile metadata field count used when `MAX_PROFILE_FIELDS` is unset.
///
/// Mastodon caps `fields_attributes` at 4 entries per account
/// (`account_fields` in `config/settings.yml`);
/// `PATCH /api/v1/accounts/update_credentials` refuses more with 422
/// `{"error":"Too many fields"}`.
pub const DEFAULT_MAX_PROFILE_FIELDS: usize = 4;
/// Report comment length used when `MAX_REPORT_COMMENT` is unset.
///
/// Mastodon caps a report's `comment` at 1000 characters
/// (`Report`'s length validation); `POST /api/v1/reports` refuses a longer
/// one with 422.
pub const DEFAULT_MAX_REPORT_COMMENT: usize = 1_000;
/// Pinned statuses per account used when `MAX_PINNED_STATUSES` is unset.
///
/// Pleroma's `[:instance, :max_pinned_statuses]` ships as one
/// (`Pleroma.User.add_pinned_object_id/2` validates against it and
/// `Pleroma.Web.CommonAPI.pin` refuses the pin past it), so a second pin is
/// a 422 on an instance that never raised the limit.
pub const DEFAULT_MAX_PINNED_STATUSES: usize = 1;
/// Shortest status expiration used when `MIN_EXPIRATION_SECS` is unset.
///
/// Pleroma's `:expiration, minimum_expiration_interval` ships as 600:
/// `POST /api/v1/statuses` refuses a shorter `expires_in` with 422, so a
/// client cannot schedule a purge faster than ten minutes out.
pub const DEFAULT_MIN_EXPIRATION_SECS: usize = 600;
/// Scheduled-statuses window used when `MIN_SCHEDULED_SECS` is unset.
///
/// Pleroma's `Pleroma.ScheduledActivity` `@min_offset` ships as
/// `:timer.minutes(5)`: the scheduled-statuses window check will refuse a
/// `POST /api/v1/statuses` `scheduled_at` less than five minutes ahead.
pub const DEFAULT_MIN_SCHEDULED_SECS: usize = 300;
/// Pending scheduled rows per account used when `MAX_SCHEDULED_PER_USER` is
/// unset.
///
/// Pleroma's `[:Pleroma.ScheduledActivity, :total_user_limit]` ships as 300:
/// the per-user scheduled-status limit check will refuse a `POST
/// /api/v1/statuses` `scheduled_at` that would take one account past this
/// many pending rows.
pub const DEFAULT_MAX_SCHEDULED_PER_USER: usize = 300;
/// Pending scheduled rows per account per day used when
/// `MAX_SCHEDULED_PER_USER_PER_DAY` is unset.
///
/// Pleroma's `[:Pleroma.ScheduledActivity, :daily_user_limit]` ships as 25:
/// the per-user-per-day scheduled-status limit check will refuse a `POST
/// /api/v1/statuses` `scheduled_at` falling on a UTC day that already holds
/// this many pending rows for the account.
pub const DEFAULT_MAX_SCHEDULED_PER_USER_PER_DAY: usize = 25;
/// Upload file size used when `UPLOAD_LIMIT_BYTES` is unset: the 16 MB of
/// Pleroma's `:upload, max_size` (decimal, so just under the middleware's
/// 16 MiB body cap).
pub const DEFAULT_UPLOAD_LIMIT_BYTES: usize = 16_000_000;
/// Upload file count used when `UPLOAD_MAX_FILES` is unset: Pleroma's
/// `:upload, max_number_of_files`.
pub const DEFAULT_UPLOAD_MAX_FILES: usize = 16;
/// The two upload defaults together as an [`UploadLimits`], spelled once
/// for [`Config::test_defaults`]'s literal; the real read of the same
/// pair is the `upload_settings` function.
#[cfg(any(test, feature = "integration-tests"))]
const TEST_UPLOAD_LIMITS: UploadLimits = UploadLimits {
    max_file_bytes: DEFAULT_UPLOAD_LIMIT_BYTES,
    max_files: DEFAULT_UPLOAD_MAX_FILES,
};
/// Avatar image size used when `AVATAR_LIMIT_BYTES` is unset.
///
/// Pleroma's `[:avatar, :limit]` `[size: 2_000_000]`:
/// `PATCH /api/v1/accounts/update_credentials` refuses a larger `avatar`
/// file with 422 `{"error":"File is too large"}`.
pub const DEFAULT_AVATAR_LIMIT_BYTES: usize = 2_000_000;
/// Banner image size used when `BANNER_LIMIT_BYTES` is unset.
///
/// Pleroma's `[:banner, :limit]` (and `[:background, :limit]`)
/// `[size: 4_000_000]`, the one number this server caps both the `header`
/// and the `pleroma_background_image` file at with 422
/// `{"error":"File is too large"}`.
pub const DEFAULT_BANNER_LIMIT_BYTES: usize = 4_000_000;
/// Inbox body size used when `INBOX_MAX_BYTES` is unset: the 1 MiB the
/// inbox authentication extractor caps one inbound `ActivityPub` `POST`
/// body at (see [`crate::federation::inbox_auth`]).
pub const DEFAULT_INBOX_MAX_BYTES: usize = 1_048_576;
/// Remote mention lookups used when `MAX_REMOTE_MENTION_LOOKUPS` is unset:
/// the 10 unknown-remote mentions one status may resolve over the network
/// (see [`crate::services::mentions`]).
pub const DEFAULT_MAX_REMOTE_MENTION_LOOKUPS: usize = 10;
/// Image pixel count used when `MAX_IMAGE_PIXELS` is unset.
///
/// The 40 megapixels the media ticket fixes. `POST /api/v1/media` and
/// `POST /api/v2/media` reject an image whose decoded dimensions multiply
/// past it with 422 `{"error":"Image too large"}` (see
/// [`crate::services::media_processing::analyze_image`]).
pub const DEFAULT_MAX_IMAGE_PIXELS: usize = 40_000_000;
/// Whether the outbound SSRF guard is disabled when
/// `HTTP_ALLOW_PRIVATE_HOSTS` is unset: it is on by default, so private
/// and loopback targets are blocked.
pub const DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS: bool = false;
/// Directory uploaded files are stored in when `UPLOADS_DIR` is unset,
/// relative to the working directory (Pleroma's `Pleroma.Uploaders.Local`
/// default is a `public/uploads` directory in the same spirit).
pub const DEFAULT_UPLOADS_DIR: &str = "./uploads";
/// Directory emoji packs live in when `EMOJI_DIR` is unset, relative to
/// the working directory.
///
/// Pleroma keeps them in an `emoji` subdirectory of its static dir
/// (`lib/pleroma/emoji/pack.ex`), one directory per pack.
pub const DEFAULT_EMOJI_DIR: &str = "./emoji";
/// Path appended to `BASE_URL` for the media URL prefix when
/// `MEDIA_BASE_URL` is unset: the route in [`crate::api::media_files`].
pub const DEFAULT_MEDIA_URL_PATH: &str = "/media";
/// Email requirement used when `ACCOUNTS_EMAIL_OPTIONAL` is unset: an email
/// address is required at registration.
pub const DEFAULT_ACCOUNTS_EMAIL_OPTIONAL: bool = false;
/// Password length used when `MIN_PASSWORD_LENGTH` is unset: the 8
/// characters of Pleroma-era Mastodon password minimums.
pub const DEFAULT_MIN_PASSWORD_LENGTH: usize = 8;
/// Access-token lifetime used when `OAUTH_TOKEN_TTL_SECS` is unset:
/// `2_592_000` seconds, 30 days.
///
/// Pleroma's own `[:oauth2, :token_expires_in]` ships far longer; the
/// ticket default rules here. [`DEFAULT_TOKEN_TTL`] is the same span as
/// a [`std::time::Duration`], kept consistent by a test in
/// [`crate::models::oauth_token`].
///
/// [`DEFAULT_TOKEN_TTL`]: crate::models::oauth_token::DEFAULT_TOKEN_TTL
pub const DEFAULT_OAUTH_TOKEN_TTL_SECS: usize = 2_592_000;
/// Password-reset token lifetime used when `PASSWORD_RESET_TTL_SECS` is
/// unset: `86_400` seconds, 24 hours — Pleroma's
/// `[:instance, :password_reset_token_validity]` default.
pub const DEFAULT_PASSWORD_RESET_TTL_SECS: usize = 86_400;
/// Email-confirmation state used when `ACCOUNT_ACTIVATION_REQUIRED` is
/// unset: no confirmation required.
///
/// New accounts log in without confirming their address and changing the
/// email stays confirmed — making confirmation a condition of logging in is
/// an explicit operator decision, as it is in Pleroma's
/// `[:instance, :account_activation_required]` `false` default.
pub const DEFAULT_ACCOUNT_ACTIVATION_REQUIRED: bool = false;
/// Account-deletion state used when `ALLOW_ACCOUNT_DELETION` is unset:
/// `POST /api/pleroma/delete_account` is offered, as Pleroma offers
/// `UtilController.delete_account`; closing it is the operator's call.
pub const DEFAULT_ALLOW_ACCOUNT_DELETION: bool = true;
/// `MongooseIM` auth endpoint state used when `MONGOOSEIM_ENABLED` is
/// unset: the endpoints answer the standard fallback 404 until an operator
/// turns them on for their `extauth` backend.
pub const DEFAULT_MONGOOSEIM_ENABLED: bool = false;
/// Captcha state used when `CAPTCHA_ENABLED` is unset: no captcha provider
/// is configured, so `GET /api/v1/pleroma/captcha` reports `{"type":"none"}`
/// like Pleroma with no captcha plugin.
pub const DEFAULT_CAPTCHA_ENABLED: bool = false;
/// Healthcheck state used when `HEALTHCHECK_ENABLED` is unset: the
/// endpoint answers the standard fallback 404 until it is turned on.
pub const DEFAULT_HEALTHCHECK_ENABLED: bool = false;
/// Registration state used when `REGISTRATIONS_OPEN` is unset: Pleroma's
/// `[:instance, :registrations_open]` default is open, so closing
/// registration is always an explicit operator decision.
pub const DEFAULT_REGISTRATIONS_OPEN: bool = true;
/// Invite state used when `INVITES_ENABLED` is unset: invites are off, so
/// while registrations are closed every registration attempt is refused —
/// accepting invite tokens is an explicit operator decision.
pub const DEFAULT_INVITES_ENABLED: bool = false;
/// Unauthenticated status-search state used when
/// `SEARCH_STATUSES_UNAUTHENTICATED` is unset: false.
///
/// While off, the combined search answers a request without a viewer with
/// an empty `statuses` list: letting anonymous requests read the stored
/// text is an explicit operator decision.
pub const DEFAULT_SEARCH_STATUSES_UNAUTHENTICATED: bool = false;
/// Restricted areas used when `RESTRICT_UNAUTHENTICATED` is unset: nothing.
///
/// So every read an anonymous request may make is open — a public instance,
/// as Pleroma's own `:if_instance_is_private` default resolves on one.
pub const DEFAULT_RESTRICT_UNAUTHENTICATED: &str = "";
/// Approval state used when `REGISTRATIONS_APPROVAL_REQUIRED` is unset:
/// nobody waits for staff.
///
/// Matches Pleroma's `[:instance, :account_approval_required]` `false`
/// default — requiring approval is an explicit operator decision.
pub const DEFAULT_REGISTRATIONS_APPROVAL_REQUIRED: bool = false;
/// The provider list used when `OAUTH_PROVIDERS` is unset: no external
/// login provider is configured, so `GET /oauth/prepare_request` and the
/// provider endpoints answer their errors for every name.
pub const DEFAULT_OAUTH_PROVIDERS: &str = "[]";
/// Frontend list used when `AVAILABLE_FRONTENDS` is unset: nothing is pickable.
///
/// So `GET /api/v1/pleroma/preferred_frontend/available` answers `[]` and
/// `PUT /api/v1/pleroma/preferred_frontend` refuses every name.
pub const DEFAULT_AVAILABLE_FRONTENDS: &str = "";
/// Worker concurrency used when `JOB_CONCURRENCY` is unset.
pub const DEFAULT_JOB_CONCURRENCY: usize = 10;
/// Deleted-status purge age used when `PURGE_DELETED_STATUSES_DAYS` is
/// unset: thirty days, the window the earlier maintenance sweeps give
/// their rows before reclaiming them.
pub const DEFAULT_PURGE_DELETED_STATUSES_DAYS: usize = 30;
/// Backup archive validity used when `BACKUP_VALIDITY_DAYS` is unset: thirty
/// days, the window a finished user backup archive stays downloadable.
pub const DEFAULT_BACKUP_VALIDITY_DAYS: usize = 30;
/// Backup interval floor used when `BACKUP_LIMIT_DAYS` is unset: seven days,
/// the minimum spacing between two backups of one user.
pub const DEFAULT_BACKUP_LIMIT_DAYS: usize = 7;
/// Backup purge state used when `BACKUP_PURGE` is unset: the sweep runs;
/// switching it off is the operator's explicit decision.
pub const DEFAULT_BACKUP_PURGE: bool = true;
/// Streaming heartbeat period used when `STREAMING_PING_INTERVAL_SECS` is
/// unset: the 30 seconds Pleroma's `WebsocketHandler` pings a socket on
/// (`lib/pleroma/web/mastodon_api/websocket_handler.ex`, `@tick`).
pub const DEFAULT_STREAMING_PING_INTERVAL_SECS: usize = 30;
/// SMTP relay port used when `SMTP_PORT` is unset: the `SUBMISSION` port,
/// where a relay accepts authenticated client connections.
pub const DEFAULT_SMTP_PORT: u16 = 587;
/// Sender user part used when `MAIL_FROM` is unset: the `noreply` of the
/// default address, attached to the instance domain, so an unset `MAIL_FROM`
/// on `social.example` sends as `noreply@social.example`.
pub const DEFAULT_MAIL_FROM_USER: &str = "noreply";
/// Media proxy enablement used when `MEDIA_PROXY_ENABLED` is unset: the
/// proxy is off, so entity URLs pass through untouched.
pub const DEFAULT_MEDIA_PROXY_ENABLED: bool = false;
/// Proxied media byte cap used when `MEDIA_PROXY_MAX_BYTES` is unset:
/// 50 MB, the order of magnitude Pleroma's `max_body_length` media
/// proxying allows.
pub const DEFAULT_MEDIA_PROXY_MAX_BYTES: usize = 50_000_000;
/// The media-proxy defaults together as a [`MediaProxyConfig`], spelled
/// once for [`Config::test_defaults`]'s literal; the real read of the same
/// group is the `media_proxy_settings` function.
#[cfg(any(test, feature = "integration-tests"))]
const TEST_MEDIA_PROXY: MediaProxyConfig = MediaProxyConfig {
    enabled: DEFAULT_MEDIA_PROXY_ENABLED,
    secret: None,
    base_url: None,
    max_bytes: DEFAULT_MEDIA_PROXY_MAX_BYTES,
    blacklist: Vec::new(),
};
/// The rate-limiter test defaults as a [`RateLimitConfig`], spelled once
/// for [`Config::test_defaults`]'s literal like [`TEST_UPLOAD_LIMITS`] and
/// [`TEST_MEDIA_PROXY`]; a function rather than a `const`, because
/// [`default_limits`](crate::rate_limit::default_limits) builds its
/// [`std::collections::BTreeMap`] at runtime.
#[cfg(any(test, feature = "integration-tests"))]
fn test_rate_limit() -> RateLimitConfig {
    RateLimitConfig {
        enabled: Toggle::new(false),
        limits: rate_limit::default_limits(),
        trust_proxy_headers: Toggle::new(DEFAULT_TRUST_PROXY_HEADERS),
    }
}
/// Minimum length, in characters, of `MEDIA_PROXY_SECRET` when the media
/// proxy is enabled: 32 bytes of key material for HMAC-SHA256.
pub const MIN_MEDIA_PROXY_SECRET_CHARS: usize = 32;
/// Ignore list used when `RICH_MEDIA_IGNORE_HOSTS` is unset: nothing, so
/// card fetching visits every host the rest of the settings allow.
pub const DEFAULT_RICH_MEDIA_IGNORE_HOSTS: &str = "";
/// Remote-card state used when `RICH_MEDIA_REMOTE` is unset: off, so no
/// preview card is ever fetched for a status's remote link.
pub const DEFAULT_RICH_MEDIA_REMOTE: bool = false;
/// Signed-fetch state used when `SIGN_FETCHES` is unset: actor `GET`s are
/// signed.
///
/// Pleroma signs dereferences with the instance actor key by default
/// (`Pleroma.User.get_or_create_service_actor_by_ap_id` plus
/// `Signature.sign`), so peers with authorized fetch answer our fetches.
pub const DEFAULT_SIGN_FETCHES: bool = true;
/// Authorized-fetch state used when `AUTHORIZED_FETCH` is unset: this
/// instance serves `ActivityPub` `GET`s to unsigned requests too; Pleroma's
/// `[:federator, :authorized_fetch]`-style gate is an explicit opt-in.
pub const DEFAULT_AUTHORIZED_FETCH: bool = false;
/// Thread-backfill depth used when `THREAD_FETCH_DEPTH` is unset: the
/// job walks up to 20 ancestors of a fetched reply, Pleroma's
/// `fix_in_reply_to` recursion limit spelled as one number.
pub const DEFAULT_THREAD_FETCH_DEPTH: usize = 20;
/// Remote object fetches used when `MAX_FETCHES_PER_ACTIVITY` is unset:
/// one incoming activity may trigger at most 20 object dereferences before
/// the fetch code refuses to make more.
pub const DEFAULT_MAX_FETCHES_PER_ACTIVITY: usize = 20;
/// Federation state used when `FEDERATION_ENABLED` is unset: this instance
/// federates, the way Pleroma's `[:federator, :enabled]` default does;
/// going quiet is an explicit operator decision.
pub const DEFAULT_FEDERATION_ENABLED: bool = true;
/// Unsolicited-public state used when `ACCEPT_UNSOLICITED_PUBLIC` is unset:
/// public and unlisted remote statuses are stored even from authors nobody
/// follows.
///
/// This is Mastodon's and Pleroma's behaviour — a remote public post
/// arrives because *something* (a relay, a follower of a follower, a
/// quote) delivered it, and refusing to store it hides content local users
/// can legitimately reach through a thread. Turning it off is the explicit
/// anti-spam decision, Pleroma's
/// `[:instance, :limit]`-style gate spelled as one switch.
pub const DEFAULT_ACCEPT_UNSOLICITED_PUBLIC: bool = true;
/// Unreachable-host grace period used when `UNREACHABLE_GRACE_SECS` is
/// unset: `0`, so a host the delivery path has marked counts as unreachable
/// with the mark itself.
///
/// Pleroma has no such window — its `Instances.filter_reachable/1` drops
/// every host carrying `unreachable_since` — so `0` is the compatible
/// reading; an operator raising it gives a host that refused one `POST` the
/// grace of a retry cycle before the whole instance stops talking to it.
pub const DEFAULT_UNREACHABLE_GRACE_SECS: usize = 0;
/// MRF transparency used when `MRF_TRANSPARENCY` is unset: the enabled
/// policies publish their lists.
///
/// This is Pleroma's `[:mrf, :transparency]` default — an instance that
/// moderates by domain tells its users so.
pub const DEFAULT_MRF_TRANSPARENCY: bool = true;
/// Rate limiting used when `RATE_LIMITS_ENABLED` is unset: routes that
/// mount a bucket throttle, like Pleroma's `RateLimiterPlug`.
pub const DEFAULT_RATE_LIMITS_ENABLED: bool = true;
/// Proxy trust used when `TRUST_PROXY_HEADERS` is unset: the socket peer
/// is always the client, so a plain `X-Forwarded-For` header cannot name
/// another one.
pub const DEFAULT_TRUST_PROXY_HEADERS: bool = false;
/// `DATABASE_URL` used by [`Config::for_tests`]: a placeholder URL that
/// points at nothing; integration tests get their real pool from the fixture
/// in [`crate::test_db`].
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_DATABASE_URL: &str = "postgres://localhost/wired_test";
/// `BASE_URL` used by [`Config::for_tests`].
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_BASE_URL: &str = "https://test.example";

/// A boolean feature toggle held by [`Config`].
///
/// [`Config`] already carries the two registration/SSRF-guard booleans, and
/// Clippy's denied `struct_excessive_bools` (threshold 2) wants more
/// booleans modelled, not piled up — the same reason
/// [`Flag`](crate::models::user::Flag) exists for `users`' columns. A
/// `Toggle` is a plain named wrapper: parsed from the environment by the
/// `boolean` reader below, read by the gated handlers with
/// [`get`](Self::get). `Serialize` writes it as its inner `bool`, so the
/// JSON documents that read a toggle (`NodeInfo`'s `openRegistrations`)
/// see a plain `true`/`false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Toggle(bool);

impl Toggle {
    /// Wraps a raw boolean as a toggle.
    #[must_use]
    pub const fn new(value: bool) -> Self {
        Self(value)
    }

    /// The raw boolean behind the toggle.
    #[must_use]
    pub const fn get(self) -> bool {
        self.0
    }
}

/// A `bool` is already a valid toggle value; `true.into()` sets one.
impl From<bool> for Toggle {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

/// The moderation-filter (MRF) settings: which policies run over every
/// activity, and how much of what they decide the instance tells the world
/// (see [`crate::federation::mrf`]).
///
/// The three settings ride together because the [`Config`] readers and the
/// [`Debug`] impl below each have an 80-line budget (AGENTS.md) that one
/// field per line would not fit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MrfSettings {
    /// The policy names from `MRF_POLICIES`, in chain order: the fallback
    /// chain configuration, used whenever the `mrf` instance setting
    /// carries no `policies` array of its own.
    pub policies: Vec<String>,
    /// Whether the policies publish their lists at all, from
    /// `MRF_TRANSPARENCY` (Pleroma's `[:mrf, :transparency]`).
    pub transparency: Toggle,
    /// The host patterns kept out of what is published, from
    /// `MRF_TRANSPARENCY_EXCLUSIONS` (comma list, Pleroma's
    /// `[:mrf, :transparency_exclusions]`).
    pub transparency_exclusions: Vec<String>,
}

impl MrfSettings {
    /// The settings the test configuration writes: no policy without a
    /// setting row, and the moderation published whole.
    #[cfg(any(test, feature = "integration-tests"))]
    const fn test_defaults() -> Self {
        Self {
            policies: Vec::new(),
            transparency: Toggle::new(DEFAULT_MRF_TRANSPARENCY),
            transparency_exclusions: Vec::new(),
        }
    }
}

/// One external OAuth login provider, as one element of the
/// `OAUTH_PROVIDERS` JSON array (Pleroma's `config :ueberauth, Ueberauth,
/// providers:` list, expressed as data instead of compiled-in strategies).
///
/// The four `*_field` names say where the provider's userinfo document
/// carries each fact; a dotted path digs into nested objects (`"data.id"`),
/// and an empty name means the provider does not tell us that much.
/// `client_secret` is secret material: the manual `Debug` impl below
/// redacts it, so a configuration written to the log never carries it.
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthProvider {
    /// Provider name: the `{provider}` path segment and the value of the
    /// `provider` parameter, e.g. `github`.
    pub name: String,
    /// The id this instance registered with the provider, sent as
    /// `client_id`.
    pub client_id: String,
    /// The provider's client secret, sent to the token endpoint. Never in
    /// `Debug` output.
    pub client_secret: String,
    /// Where the browser is redirected to authorize us, sent the provider
    /// `client_id`, our callback URL and our state.
    pub authorize_url: String,
    /// Where the returned code is exchanged for an `access_token`.
    pub token_url: String,
    /// Where the `access_token` fetches the account document from.
    pub userinfo_url: String,
    /// The `scope` string sent to the provider (its own vocabulary, not
    /// Mastodon scopes); empty sends none.
    #[serde(default)]
    pub scopes: String,
    /// Dotted path of the stable provider account id in the userinfo
    /// document, the key `user_identities.uid` stores.
    #[serde(default = "default_uid_field")]
    pub uid_field: String,
    /// Dotted path of the login name, offered to the registration form.
    /// Empty means the document carries no such name.
    #[serde(default)]
    pub nickname_field: String,
    /// Dotted path of the email address, offered to the registration form.
    #[serde(default)]
    pub email_field: String,
    /// Dotted path of the display name, offered to the registration form.
    #[serde(default)]
    pub name_field: String,
}

/// [`OauthProvider::uid_field`] when a provider object omits it: the
/// top-level `id` key, the spelling the majority of providers use for the
/// stable account id (GitHub's `id`; a provider that names it otherwise,
/// like Keycloak's `sub`, says so in its own object).
fn default_uid_field() -> String {
    "id".to_owned()
}

/// [`client_secret`](Self::client_secret) is secret material and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for OauthProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OauthProvider")
            .field("name", &self.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("userinfo_url", &self.userinfo_url)
            .field("scopes", &self.scopes)
            .field("uid_field", &self.uid_field)
            .field("nickname_field", &self.nickname_field)
            .field("email_field", &self.email_field)
            .field("name_field", &self.name_field)
            .finish()
    }
}

/// Media proxy settings, from the five `MEDIA_PROXY_*` variables.
///
/// When [`enabled`](Self::enabled) is set, remote media URLs in API
/// responses are rewritten to signed links this instance serves itself
/// (see [`crate::services::media_proxy`]).
#[derive(Clone, PartialEq, Eq)]
pub struct MediaProxyConfig {
    /// Whether remote media URLs are rewritten to proxy links, from
    /// `MEDIA_PROXY_ENABLED`.
    pub enabled: bool,
    /// HMAC-SHA256 signing secret, from `MEDIA_PROXY_SECRET`; required
    /// (and validated to be at least [`MIN_MEDIA_PROXY_SECRET_CHARS`]
    /// characters) whenever [`enabled`](Self::enabled) is set. Redacted
    /// in `Debug` output like `Config`'s `database_url`.
    pub secret: Option<String>,
    /// Base URL proxy links are built from, from `MEDIA_PROXY_BASE_URL`;
    /// `None` means [`Config::base_url`]. Never ends in a slash.
    pub base_url: Option<String>,
    /// Hard cap on the bytes served for one proxied media file, from
    /// `MEDIA_PROXY_MAX_BYTES`.
    pub max_bytes: usize,
    /// Hosts that are never proxied, from `MEDIA_PROXY_BLACKLIST`;
    /// compared case-insensitively against the URL's host.
    pub blacklist: Vec<String>,
}

/// Redacts [`secret`](Self::secret), which is signing key material.
impl fmt::Debug for MediaProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaProxyConfig")
            .field("enabled", &self.enabled)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("max_bytes", &self.max_bytes)
            .field("blacklist", &self.blacklist)
            .finish()
    }
}

/// Which service translates statuses, the value of
/// `TRANSLATION_PROVIDER` (Pleroma's `[:language, :translation, :provider]`).
///
/// The two remote providers are the ones
/// [`crate::services::translate`] speaks; anything else is
/// [`None`](Self::None), which is also the default, so an instance
/// translates nothing until an operator points it at a server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TranslationProvider {
    /// No provider: `POST /api/v1/statuses/{id}/translate` answers 503 and
    /// the language matrix stays empty.
    #[default]
    None,
    /// A `LibreTranslate` server (self-hosted or a hosted instance of it).
    LibreTranslate,
    /// The `DeepL` API.
    DeepL,
}

impl TranslationProvider {
    /// The provider a `TRANSLATION_PROVIDER` value names, case-insensitively
    /// and trimmed; `None` for any other spelling.
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "libretranslate" => Some(Self::LibreTranslate),
            "deepl" => Some(Self::DeepL),
            _ => None,
        }
    }

    /// Whether this name selects a provider at all
    /// ([`None`](Self::None) selects none).
    #[must_use]
    pub const fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// VAPID key material for Web Push, from `VAPID_PRIVATE_KEY` and
/// `VAPID_PUBLIC_KEY`.
///
/// The instance's ES256 key pair normally lives in the `instance_secrets`
/// table, generated on first startup (see [`crate::services::vapid`]);
/// these settings override that pair with the operator's own material
/// instead. [`private_key`](Self::private_key) is secret: the manual
/// `Debug` impl below redacts it. [`public_key`](Self::public_key) is the
/// half every push client is handed as `server_key`, so it is no secret.
///
/// [`crate::services::vapid`]: crate::services::vapid
#[derive(Clone, Default, PartialEq, Eq)]
pub struct VapidConfig {
    /// The signing key as PKCS#8 PEM, from `VAPID_PRIVATE_KEY`; `None`
    /// keeps the generated pair in `instance_secrets`. Redacted in
    /// `Debug` output like `Config`'s `database_url`.
    pub private_key: Option<String>,
    /// The public point as URL-safe base64 (no padding), from
    /// `VAPID_PUBLIC_KEY`; `None` means the public half is read from (or
    /// derived from [`private_key`](Self::private_key)).
    pub public_key: Option<String>,
}

/// Redacts [`private_key`](Self::private_key), which is signing key
/// material; the public half shows.
impl fmt::Debug for VapidConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VapidConfig")
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "<redacted>"),
            )
            .field("public_key", &self.public_key)
            .finish()
    }
}

/// Status-translation settings, from the three `TRANSLATION_*` variables.
///
/// [`crate::services::translate`] turns one of these into the translator the
/// endpoints use. [`api_key`](Self::api_key) is secret material: the manual
/// `Debug` impl below redacts it. The [`Default`] is the unset state: no
/// provider, no URL, no key.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TranslationConfig {
    /// The provider to translate with, from `TRANSLATION_PROVIDER`;
    /// [`None`](TranslationProvider::None) by default.
    pub provider: TranslationProvider,
    /// The provider's API base URL, from `TRANSLATION_API_URL` with any
    /// trailing slash trimmed; `None` when unset.
    pub api_url: Option<String>,
    /// The provider's API key, from `TRANSLATION_API_KEY`; `None` when
    /// unset. Redacted in `Debug` output like `Config`'s `database_url`.
    pub api_key: Option<String>,
}

impl TranslationConfig {
    /// Whether these settings name a provider the instance can actually
    /// talk to: a provider other than [`None`](TranslationProvider::None),
    /// an [`api_url`](Self::api_url) to reach it at, and — for
    /// [`DeepL`](TranslationProvider::DeepL), whose API has no unauthenticated
    /// mode — an [`api_key`](Self::api_key). This is what
    /// `configuration.translation.enabled` of the v2 instance document
    /// reports (Pleroma's `Translation.configured?()`).
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        if self.provider.is_none() || self.api_url.is_none() {
            return false;
        }
        !matches!(self.provider, TranslationProvider::DeepL) || self.api_key.is_some()
    }
}

/// Redacts [`api_key`](Self::api_key), which is provider credentials.
impl fmt::Debug for TranslationConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranslationConfig")
            .field("provider", &self.provider)
            .field("api_url", &self.api_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Rate limiter settings, from `RATE_LIMITS_ENABLED`, `RATE_LIMITS` and
/// `TRUST_PROXY_HEADERS`.
///
/// While [`enabled`](Self::enabled) is set — the default — routes that
/// mount a bucket throttle their requests against the windows of
/// [`crate::rate_limit`]; the rest of the struct fixes what they throttle
/// with and whom they count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Whether limited routes throttle, from `RATE_LIMITS_ENABLED`: while
    /// off they answer uncounted and without `x-ratelimit-*` headers.
    pub enabled: Toggle,
    /// The bucket table, from `RATE_LIMITS`: Pleroma's defaults
    /// (see [`DEFAULT_LIMITS`](crate::rate_limit::DEFAULT_LIMITS)) with the
    /// variable's `bucket=max:window_secs` entries overriding them; a
    /// bucket the table does not name is unlimited.
    pub limits: BTreeMap<String, RateLimit>,
    /// Whether the leftmost `X-Forwarded-For` entry may name the client a
    /// window counts against, from `TRUST_PROXY_HEADERS`: while off (the
    /// default) the socket peer always does.
    pub trust_proxy_headers: Toggle,
}

/// The two numbers that bound how far federation fetching may go, from
/// `THREAD_FETCH_DEPTH` and `MAX_FETCHES_PER_ACTIVITY` (see
/// [`crate::federation::fetch_object`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchLimits {
    /// How far up a fetched thread [`federator.backfill`] walks to link a
    /// remote reply to its ancestors; a deeper thread stays linked only as
    /// far as this.
    ///
    /// [`federator.backfill`]: crate::federation::fetch_object::KIND_BACKFILL
    pub thread_fetch_depth: usize,
    /// The most remote objects one incoming activity (or one resolver run)
    /// may dereference; a fetch past the cap fails instead of fanning out
    /// further.
    pub max_fetches_per_activity: usize,
}

/// The settings that govern how this instance fetches and is fetched by
/// peers.
///
/// Grouped because [`Config::from_lookup`] has a size budget
/// (AGENTS.md) and all four describe one outbound `GET`: the
/// `HTTP_ALLOW_PRIVATE_HOSTS`, `SIGN_FETCHES` and `AUTHORIZED_FETCH`
/// toggles and the two numbers of [`FetchLimits`] (`THREAD_FETCH_DEPTH`,
/// `MAX_FETCHES_PER_ACTIVITY`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchSettings {
    /// Whether outbound HTTP may connect to private, loopback and other
    /// reserved addresses, i.e. the SSRF guard is disabled, from
    /// `HTTP_ALLOW_PRIVATE_HOSTS`. Development and tests only.
    pub http_allow_private_hosts: bool,
    /// Whether outgoing actor `GET`s are HTTP-signed with the instance
    /// actor's key, from `SIGN_FETCHES` (see
    /// [`crate::federation::fetch_actor`]).
    pub sign_fetches: Toggle,
    /// Whether incoming `GET`s to the `ActivityPub` actor routes must carry
    /// a valid HTTP signature, from `AUTHORIZED_FETCH` (see
    /// [`crate::federation::authorized_fetch`]).
    pub authorized_fetch: Toggle,
    /// How far federation fetching may go, from `THREAD_FETCH_DEPTH` and
    /// `MAX_FETCHES_PER_ACTIVITY` (see [`crate::federation::fetch_object`]).
    pub limits: FetchLimits,
}

/// The unset-everything shape: each variable's documented default.
impl Default for FetchSettings {
    fn default() -> Self {
        Self {
            http_allow_private_hosts: DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS,
            sign_fetches: DEFAULT_SIGN_FETCHES.into(),
            authorized_fetch: DEFAULT_AUTHORIZED_FETCH.into(),
            limits: FetchLimits::default(),
        }
    }
}

/// The unset-everything shape of the two fetch bounds: the documented
/// defaults of `THREAD_FETCH_DEPTH` and `MAX_FETCHES_PER_ACTIVITY`.
impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            thread_fetch_depth: DEFAULT_THREAD_FETCH_DEPTH,
            max_fetches_per_activity: DEFAULT_MAX_FETCHES_PER_ACTIVITY,
        }
    }
}

/// How the SMTP relay connection is encrypted, the value of `SMTP_TLS`.
///
/// Parsed by the `SMTP_TLS` reader in `mailer_settings`; the unset or blank
/// default is [`Starttls`](Self::Starttls), the mode submission-port relays
/// speak.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SmtpTls {
    /// Plain connection upgraded with STARTTLS, required before any
    /// credential is sent.
    #[default]
    Starttls,
    /// Implicit TLS for the whole connection (SMTPS, traditionally port
    /// 465).
    Tls,
    /// No encryption at all, for a trusted local relay.
    None,
}

impl SmtpTls {
    /// The mode an `SMTP_TLS` value names, case-insensitively and trimmed;
    /// `None` for any other spelling.
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "starttls" => Some(Self::Starttls),
            "tls" => Some(Self::Tls),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Application configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// PostgreSQL connection URL, e.g. `postgres://user:pass@localhost/wired`.
    pub database_url: String,
    /// Address the HTTP server binds to.
    pub bind_addr: SocketAddr,
    /// Public origin of this instance, normalized to e.g.
    /// `https://social.example` (no path, query or trailing slash).
    pub base_url: String,
    /// Origin the frontend pages are served from, from `FRONTEND_BASE_URL`;
    /// [`base_url`](Self::base_url) when unset or blank. A set value is
    /// trimmed and loses its trailing slashes, so a browser redirect such as
    /// `/notice/{id}` (a later slice) can hang its path straight onto it.
    pub frontend_base_url: String,
    /// Display name of the instance.
    pub instance_name: String,
    /// Short description of the instance.
    pub instance_description: String,
    /// Admin contact email, if configured.
    pub contact_email: Option<String>,
    /// Content language codes advertised by the instance.
    pub instance_languages: Vec<String>,
    /// Maximum length of a status in characters.
    pub max_status_chars: usize,
    /// Maximum number of media attachments per status.
    pub max_media_attachments: usize,
    /// Maximum number of options per poll.
    pub max_poll_options: usize,
    /// Maximum length of one poll option in characters, from
    /// `MAX_POLL_OPTION_CHARS`; `POST /api/v1/statuses` refuses a poll
    /// carrying an empty option or an option longer than this with 422
    /// `{"error":"Poll options cannot be longer than <n> characters"}` (the
    /// message names this value).
    pub max_poll_option_chars: usize,
    /// Shortest poll expiration, in seconds, from `MIN_POLL_EXPIRATION`;
    /// `POST /api/v1/statuses` refuses a poll whose `poll[expires_in]` is
    /// shorter with 422 `{"error":"Expiration date is too soon"}`.
    pub min_poll_expiration_secs: usize,
    /// Longest poll expiration, in seconds, from `MAX_POLL_EXPIRATION`;
    /// `POST /api/v1/statuses` refuses a poll whose `poll[expires_in]` is
    /// longer with 422 `{"error":"Expiration date is too far in the
    /// future"}`.
    pub max_poll_expiration_secs: usize,
    /// Maximum length of a chat message in characters, from
    /// `MAX_CHAT_MESSAGE_CHARS`; `POST /api/v1/pleroma/chats/{id}/messages`
    /// refuses longer content.
    pub max_chat_message_chars: usize,
    /// Maximum length of a media attachment's `description` (alt text) in
    /// characters, from `MAX_MEDIA_DESCRIPTION`; `PUT /api/v1/media/{id}`
    /// refuses longer values.
    pub max_media_description: usize,
    /// Maximum length of an account's `display_name` in characters, from
    /// `MAX_DISPLAY_NAME_CHARS`; `PATCH /api/v1/accounts/update_credentials`
    /// refuses longer values with 422.
    pub max_display_name_chars: usize,
    /// Maximum length of a profile note (bio) in characters, from
    /// `MAX_BIO_CHARS`; `PATCH /api/v1/accounts/update_credentials` refuses
    /// a longer `note` with 422.
    pub max_bio_chars: usize,
    /// Maximum number of profile metadata entries (`fields_attributes`) an
    /// account may store, from `MAX_PROFILE_FIELDS`;
    /// `PATCH /api/v1/accounts/update_credentials` refuses more with 422
    /// `{"error":"Too many fields"}`.
    pub max_profile_fields: usize,
    /// Maximum length of a report's `comment` in characters, from
    /// `MAX_REPORT_COMMENT`; `POST /api/v1/reports` refuses a longer one
    /// with 422.
    pub max_report_comment: usize,
    /// Maximum pixel count (width times height) of an uploaded image, from
    /// `MAX_IMAGE_PIXELS`; the media upload endpoints reject an image that
    /// decodes larger with 422 "Image too large" (see
    /// [`crate::services::media_processing::analyze_image`]).
    pub max_image_pixels: usize,
    /// How many statuses one account may pin to its profile, from
    /// `MAX_PINNED_STATUSES`; `POST /api/v1/statuses/{id}/pin` refuses one
    /// past it with 422
    /// `{"error":"You have already pinned the maximum number of statuses"}`
    /// (see [`crate::models::pinned_status`]).
    pub max_pinned_statuses: usize,
    /// Shortest `expires_in` a status create may ask for, in seconds, from
    /// `MIN_EXPIRATION_SECS`; `POST /api/v1/statuses` refuses a shorter one
    /// with 422 `{"error":"expires_in must be at least <n> seconds"}` and
    /// stores the accepted span as the status's `expires_at` (see
    /// [`crate::jobs::statuses`]).
    pub min_expiration_secs: usize,
    /// How far in the future a `POST /api/v1/statuses` `scheduled_at` must
    /// be, in seconds, from `MIN_SCHEDULED_SECS`; the scheduled-statuses
    /// window check will refuse a `scheduled_at` less than this many seconds
    /// ahead.
    pub min_scheduled_secs: usize,
    /// How many pending scheduled rows one account may hold, from
    /// `MAX_SCHEDULED_PER_USER`; the per-user scheduled-status limit check
    /// will refuse a `POST /api/v1/statuses` `scheduled_at` past this count.
    pub max_scheduled_per_user: usize,
    /// How many pending scheduled rows one account may hold for one UTC day,
    /// from `MAX_SCHEDULED_PER_USER_PER_DAY`; the per-user-per-day
    /// scheduled-status limit check will refuse a `scheduled_at` on a day
    /// that already holds this many.
    pub max_scheduled_per_user_per_day: usize,
    /// Per-request limits for multipart uploads, from `UPLOAD_LIMIT_BYTES`
    /// and `UPLOAD_MAX_FILES`.
    pub upload_limits: UploadLimits,
    /// Maximum size of a profile avatar image in bytes, from
    /// `AVATAR_LIMIT_BYTES`; `PATCH /api/v1/accounts/update_credentials`
    /// refuses a larger `avatar` file with 422 `{"error":"File is too large"}`.
    pub avatar_limit_bytes: usize,
    /// Maximum size of a profile banner or background image in bytes, from
    /// `BANNER_LIMIT_BYTES`; the same endpoint refuses a larger `header` or
    /// `pleroma_background_image` file with 422 `{"error":"File is too large"}`.
    pub banner_limit_bytes: usize,
    /// Hard cap on one inbound `ActivityPub` inbox request body, in bytes,
    /// from `INBOX_MAX_BYTES`; the `SignedActivity` extractor answers a
    /// larger body 413 (see [`crate::federation::inbox_auth`]).
    pub inbox_max_bytes: usize,
    /// How many unknown-remote `@nick@domain` mentions one status may look
    /// up over the network, from `MAX_REMOTE_MENTION_LOOKUPS`; further
    /// mentions of a single status stay plain text (see
    /// [`crate::services::mentions::resolve_mentions`]).
    pub max_remote_mention_lookups: usize,
    /// Directory uploaded files are stored in, from `UPLOADS_DIR`.
    pub uploads_dir: PathBuf,
    /// Directory emoji packs are stored in, from `EMOJI_DIR`: one
    /// subdirectory per pack holding a `pack.json` and its image files
    /// (see [`crate::services::emoji_packs`]).
    pub emoji_dir: PathBuf,
    /// Public prefix uploaded files' URLs are built from, from
    /// `MEDIA_BASE_URL`; defaults to `{base_url}/media` and never ends in a
    /// slash, so `/{key}` appends cleanly.
    pub media_base_url: String,
    /// Whether registration may omit the email address, from
    /// `ACCOUNTS_EMAIL_OPTIONAL`. See
    /// [`create_local_user`](crate::models::user::register::create_local_user).
    pub accounts_email_optional: bool,
    /// Minimum length of a registration password in characters, from
    /// `MIN_PASSWORD_LENGTH`.
    pub min_password_length: usize,
    /// Lifetime granted to newly issued OAuth access tokens, in seconds,
    /// from `OAUTH_TOKEN_TTL_SECS`. The `/oauth` endpoints pass it to
    /// [`create_token`](crate::models::oauth_token::create_token) as the
    /// token's `ttl`.
    pub oauth_token_ttl_secs: usize,
    /// Lifetime of a password-reset token, in seconds, from
    /// `PASSWORD_RESET_TTL_SECS`. A token is valid while it is unused and
    /// its `inserted_at` is within this window (see
    /// [`crate::models::password_reset`]).
    pub password_reset_ttl_secs: usize,
    /// Whether accounts must confirm their email address: a new account
    /// before it may log in, and a changed address before it is carried as
    /// confirmed, from `ACCOUNT_ACTIVATION_REQUIRED` (Pleroma's
    /// `[:account_activation, :required]` and
    /// `[:instance, :account_activation_required]` in one switch). While
    /// on, `POST /api/v1/accounts` registers the account unconfirmed with a
    /// fresh
    /// [`confirmation_token`](crate::models::user::User::confirmation_token),
    /// queues the confirmation email and answers without a token, an
    /// unconfirmed login is refused (see [`crate::api::accounts`]), and
    /// `POST /api/pleroma/change_email` resets the account's
    /// `email_confirmed` flag and queues a confirmation letter for the new
    /// address (see [`crate::api::pleroma_account_settings`]). The instance
    /// endpoints report the state as `account_activation_required`,
    /// `NodeInfo` as `accountActivationRequired`.
    pub account_activation_required: Toggle,
    /// Whether accounts may delete their own account, from
    /// `ALLOW_ACCOUNT_DELETION`. While off, `POST /api/pleroma/delete_account`
    /// answers 403 `{"error":"Account deletion is disabled"}` (see
    /// [`crate::api::pleroma_account_settings`]).
    pub allow_account_deletion: Toggle,
    /// Whether the `MongooseIM` auth endpoints (`GET /user_exists`,
    /// `GET /check_password`) are served, from `MONGOOSEIM_ENABLED`. See
    /// [`crate::api::misc`].
    pub mongooseim_enabled: Toggle,
    /// Whether a captcha provider is configured, from `CAPTCHA_ENABLED`.
    /// While no captcha implementation exists, a set toggle makes
    /// `GET /api/v1/pleroma/captcha` answer 501; see [`crate::api::misc`].
    pub captcha_enabled: Toggle,
    /// Whether `GET /api/v1/pleroma/healthcheck` reports pool and
    /// liveness data, from `HEALTHCHECK_ENABLED`. See [`crate::api::misc`].
    pub healthcheck_enabled: Toggle,
    /// Whether the instance accepts new local registrations, from
    /// `REGISTRATIONS_OPEN`; `NodeInfo` reports it as `openRegistrations`
    /// (see [`crate::api::nodeinfo`]).
    pub registrations_open: Toggle,
    /// Whether registration invite tokens substitute for open
    /// registrations, from `INVITES_ENABLED`: while registrations are
    /// closed, `POST /api/v1/accounts` accepts a valid invite `token` and
    /// refuses every attempt without one (see
    /// [`crate::models::invite_token`]). The instance endpoints report the
    /// state as `invites_enabled`, `NodeInfo` as `invitesEnabled`.
    pub invites_enabled: Toggle,
    /// Whether a new local account waits for staff approval before it may
    /// log in, from `REGISTRATIONS_APPROVAL_REQUIRED`: `POST
    /// /api/v1/accounts` then registers the account with
    /// [`approval_pending`](crate::models::user::User::approval_pending) set
    /// and answers without a token (see [`crate::api::accounts`]). A
    /// registration that came in on an invite token skips the wait. The
    /// instance endpoints report the state as `approval_required`.
    pub registrations_approval_required: Toggle,
    /// Whether anonymous requests search statuses at the combined search,
    /// from `SEARCH_STATUSES_UNAUTHENTICATED`: while off, `GET
    /// /api/v1/search` and `GET /api/v2/search` answer a request without a
    /// viewer with an empty `statuses` list, and the `statuses` and
    /// `statuses`-side `account_id` parameters search nothing (see
    /// [`crate::api::search`]). The account and hashtag searches are never
    /// gated by this.
    pub search_statuses_unauthenticated: Toggle,
    /// The areas of [`RESTRICT_UNAUTHENTICATED_AREAS`] an anonymous request
    /// may not read, from `RESTRICT_UNAUTHENTICATED` (a comma-separated
    /// list, empty by default): Pleroma's `:restrict_unauthenticated`
    /// nested config, spelled as one flat set of `group_half` area names.
    /// [`Config::restricts`] answers it; [`crate::api::accounts`]' profile
    /// reads are the endpoints that ask.
    pub restrict_unauthenticated: BTreeSet<String>,
    /// The external OAuth login providers, from `OAUTH_PROVIDERS`; empty
    /// when none is configured. Each one answers the `/oauth` provider
    /// endpoints under its own name (see [`crate::api::oauth_external`]).
    pub oauth_providers: Vec<OauthProvider>,
    /// The frontends clients may pick, from `AVAILABLE_FRONTENDS` (a
    /// comma-separated list, empty by default): the answer of
    /// `GET /api/v1/pleroma/preferred_frontend/available` and the whitelist
    /// `PUT /api/v1/pleroma/preferred_frontend` validates against.
    pub available_frontends: Vec<String>,
    /// Job handlers a single worker runs at the same time.
    pub job_concurrency: usize,
    /// Job queue names the worker drains, in claim priority order.
    pub job_queues: Vec<String>,
    /// Age in days after which soft-deleted LOCAL statuses are
    /// hard-deleted by the daily maintenance purge
    /// (see [`crate::jobs::maintenance::handle_purge_deleted_statuses`]),
    /// from `PURGE_DELETED_STATUSES_DAYS`; `0` disables the purge. Remote
    /// tombstones are never deleted — their rows keep a deleted remote
    /// post from being re-imported.
    pub purge_deleted_statuses_days: usize,
    /// Age in days a finished user backup archive stays downloadable, from
    /// `BACKUP_VALIDITY_DAYS`.
    pub backup_validity_days: usize,
    /// Minimum number of days between two backups of one user, from
    /// `BACKUP_LIMIT_DAYS`.
    pub backup_limit_days: usize,
    /// Whether the backup purge sweep runs at all, from `BACKUP_PURGE`:
    /// while off the sweep never deletes anything, whatever
    /// [`backup_validity_days`](Self::backup_validity_days) says.
    pub backup_purge: Toggle,
    /// Seconds between the heartbeat pings of one streaming socket, from
    /// `STREAMING_PING_INTERVAL_SECS`: [`PING_INTERVAL`] spells the same span
    /// (30, Pleroma's `@tick`), and a test keeps the two agreeing.
    ///
    /// [`PING_INTERVAL`]: crate::api::streaming::PING_INTERVAL
    pub streaming_ping_interval_secs: usize,
    /// SMTP relay host the mailer connects to, from `SMTP_HOST`; `None`
    /// (the default) disables mail entirely, so nothing is even queued.
    /// See [`crate::mailer`].
    pub smtp_host: Option<String>,
    /// SMTP relay port, from `SMTP_PORT`; 587, the submission port, by
    /// default. With [`SmtpTls::Tls`] an operator usually sets 465.
    pub smtp_port: u16,
    /// SMTP relay login name, from `SMTP_USERNAME`; `None` sends
    /// unauthenticated.
    pub smtp_username: Option<String>,
    /// SMTP relay login secret, from `SMTP_PASSWORD`. Secret material: the
    /// manual `Debug` impl below redacts it.
    pub smtp_password: Option<String>,
    /// How the relay connection is encrypted, from `SMTP_TLS`
    /// ([`SmtpTls::Starttls`] by default).
    pub smtp_tls: SmtpTls,
    /// The `From:` address every sent email carries, from `MAIL_FROM`;
    /// `noreply@<instance domain>` by default.
    pub mail_from: String,
    /// The HMAC-SHA256 secret this instance signs its mail-unsubscribe
    /// links with, from `SECRET_KEY_BASE` (Pleroma's `:pleroma,
    /// Pleroma.Web.Endpoint` `secret_key_base`); `None` when the variable
    /// is unset or blank, in which case [`serve`](crate::serve) generates
    /// a random one per boot and warns, so links minted earlier stop
    /// validating after a restart. Secret material: the manual `Debug`
    /// impl below redacts it.
    pub secret_key_base: Option<String>,
    /// The media proxy settings, from the `MEDIA_PROXY_*` variables
    /// (see [`crate::services::media_proxy`]).
    pub media_proxy: MediaProxyConfig,
    /// Hosts rich media card fetching never visits, from
    /// `RICH_MEDIA_IGNORE_HOSTS` (a comma-separated list, empty by
    /// default): a link to one of them is never fetched and gets no card.
    pub rich_media_ignore_hosts: Vec<String>,
    /// Whether preview cards are fetched for the remote URLs statuses link
    /// to, from `RICH_MEDIA_REMOTE`: off by default, so the card cache
    /// stays empty until an operator turns fetching on.
    pub rich_media_remote: Toggle,
    /// The VAPID key overrides, from `VAPID_PRIVATE_KEY` and
    /// `VAPID_PUBLIC_KEY` (see [`crate::services::vapid`]).
    pub vapid: VapidConfig,
    /// The status-translation settings, from the `TRANSLATION_*` variables
    /// (see [`crate::services::translate`]).
    pub translation: TranslationConfig,
    /// The settings that govern fetching, from the `HTTP_ALLOW_PRIVATE_HOSTS`,
    /// `SIGN_FETCHES`, `AUTHORIZED_FETCH`, `THREAD_FETCH_DEPTH` and
    /// `MAX_FETCHES_PER_ACTIVITY` variables (see
    /// [`crate::federation::fetch_object`] and [`crate::http_client`]).
    pub fetch: FetchSettings,
    /// Whether this instance federates at all, from `FEDERATION_ENABLED`:
    /// while off the `ActivityPub` inbox routes answer 403
    /// `{"error":"Federation is disabled"}` (see [`crate::api::activitypub`])
    /// and [`enqueue_delivery`](crate::federation::delivery::enqueue_delivery)
    /// queues nothing (see [`crate::federation::delivery`]).
    pub federation_enabled: Toggle,
    /// Whether uninvited public remote posts are stored, from
    /// `ACCEPT_UNSOLICITED_PUBLIC`.
    ///
    /// While off, [`crate::services::ingest`] drops a `public` or
    /// `unlisted` status from an author no local account follows, unless a
    /// local user is mentioned in it or it replies to a local status
    /// (`private`, `direct` and the author's followers-addressed posts are
    /// never gated — they were aimed at somebody here).
    pub accept_unsolicited_public: Toggle,
    /// How long after a host was marked unreachable it still counts as
    /// reachable for outbound federation, in seconds, from
    /// `UNREACHABLE_GRACE_SECS`; `0` (the default) makes any marked host
    /// unreachable at once.
    ///
    /// [`crate::federation::publisher`] turns it into a span and passes it to
    /// [`is_host_unreachable`](crate::models::instance::is_host_unreachable),
    /// which keeps a host deliverable while its `unreachable_since` mark is
    /// younger than this window — the back-off floor a single failed `POST`
    /// should not be able to defederate a peer over.
    pub unreachable_grace_secs: usize,
    /// The moderation-filter (MRF) settings, from `MRF_POLICIES`,
    /// `MRF_TRANSPARENCY` and `MRF_TRANSPARENCY_EXCLUSIONS`: the fallback
    /// chain of [`crate::federation::mrf`] and how much of what the chain
    /// decides the instance publishes (see [`MrfSettings`]).
    pub mrf: MrfSettings,
    /// The rate limiter settings, from `RATE_LIMITS_ENABLED`, `RATE_LIMITS`
    /// and `TRUST_PROXY_HEADERS` (see [`crate::rate_limit`]).
    pub rate_limit: RateLimitConfig,
}

impl Config {
    /// Reads configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a required variable is missing or a value is invalid.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Reads configuration through `lookup`, which maps a variable name to its
    /// value. Tests pass values in here instead of mutating the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a required variable is missing or a value is invalid.
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(lookup: F) -> Result<Self, ConfigError> {
        let (database_url, bind_addr, base_url, media_base_url) = endpoints(&lookup)?;
        let (profile, limits, text, auth, exposure, mail) = wording_and_mail(&lookup, &base_url)?;
        let rest = rest_settings(&lookup)?;
        Ok(Self {
            database_url,
            bind_addr,
            // Read before `base_url` is moved below: it defaults onto it.
            frontend_base_url: frontend_base_url(&lookup, &base_url),
            base_url,
            instance_name: profile.name,
            instance_description: profile.description,
            contact_email: profile.contact_email,
            instance_languages: profile.languages,
            max_status_chars: limits.status_chars,
            max_media_attachments: limits.media_attachments,
            max_poll_options: limits.poll_options,
            max_poll_option_chars: limits.poll_option_chars,
            min_poll_expiration_secs: limits.min_poll_expiration_secs,
            max_poll_expiration_secs: limits.max_poll_expiration_secs,
            max_chat_message_chars: limits.chat_message_chars,
            max_media_description: text.media_description,
            max_display_name_chars: text.display_name_chars,
            max_bio_chars: text.bio_chars,
            max_profile_fields: text.profile_fields,
            max_report_comment: limits.report_comment,
            max_image_pixels: limits.image_pixels,
            max_pinned_statuses: limits.pinned_statuses,
            min_expiration_secs: limits.min_expiration_secs,
            min_scheduled_secs: limits.min_scheduled_secs,
            max_scheduled_per_user: limits.max_scheduled_per_user,
            max_scheduled_per_user_per_day: limits.max_scheduled_per_user_per_day,
            upload_limits: rest.upload_limits,
            avatar_limit_bytes: rest.avatar_limit_bytes,
            banner_limit_bytes: rest.banner_limit_bytes,
            inbox_max_bytes: limits.inbox_max_bytes,
            max_remote_mention_lookups: limits.max_remote_mention_lookups,
            uploads_dir: directory(&lookup, UPLOADS_DIR_VAR, DEFAULT_UPLOADS_DIR),
            emoji_dir: directory(&lookup, EMOJI_DIR_VAR, DEFAULT_EMOJI_DIR),
            media_base_url,
            accounts_email_optional: auth.accounts_email_optional,
            min_password_length: auth.min_password_length,
            oauth_token_ttl_secs: auth.oauth_token_ttl_secs,
            password_reset_ttl_secs: auth.password_reset_ttl_secs,
            allow_account_deletion: auth.allow_account_deletion,
            mongooseim_enabled: rest.mongooseim_enabled,
            captcha_enabled: rest.captcha_enabled,
            healthcheck_enabled: rest.healthcheck_enabled,
            registrations_open: exposure.registrations_open,
            invites_enabled: exposure.invites_enabled,
            registrations_approval_required: exposure.registrations_approval_required,
            account_activation_required: exposure.account_activation_required,
            search_statuses_unauthenticated: exposure.search_statuses_unauthenticated,
            restrict_unauthenticated: exposure.restrict_unauthenticated,
            oauth_providers: auth.oauth_providers,
            available_frontends: exposure.available_frontends,
            job_concurrency: rest.job_concurrency,
            job_queues: rest.job_queues,
            purge_deleted_statuses_days: rest.purge_deleted_statuses_days,
            backup_validity_days: rest.backup_validity_days,
            backup_limit_days: rest.backup_limit_days,
            backup_purge: rest.backup_purge,
            streaming_ping_interval_secs: streaming_heartbeat(&lookup)?,
            smtp_host: mail.host,
            smtp_port: mail.port,
            smtp_username: mail.username,
            smtp_password: mail.password,
            smtp_tls: mail.tls,
            mail_from: mail.from,
            secret_key_base: secret_key_base(&lookup),
            media_proxy: media_proxy_settings(&lookup)?,
            rich_media_ignore_hosts: rich_media_ignore_hosts(&lookup),
            rich_media_remote: rich_media_remote(&lookup)?,
            vapid: vapid_settings(&lookup),
            translation: translation_settings(&lookup)?,
            fetch: rest.fetch,
            federation_enabled: federation_enabled(&lookup)?,
            accept_unsolicited_public: accept_unsolicited_public(&lookup)?,
            unreachable_grace_secs: unreachable_grace_secs(&lookup)?,
            mrf: mrf_settings(&lookup)?,
            rate_limit: rate_limit_settings(&lookup)?,
        })
    }

    /// The [`base_url`](Self::base_url) without its scheme: `social.example`,
    /// or `localhost:4000` when a non-default port is part of the origin.
    #[must_use]
    pub fn domain(&self) -> &str {
        domain_of(&self.base_url)
    }

    /// Whether [`base_url`](Self::base_url) uses the `https` scheme.
    #[must_use]
    pub fn is_https(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// Whether this instance keeps `area` from anonymous readers.
    ///
    /// `area` is one of the [`RESTRICT_UNAUTHENTICATED_AREAS`] names, and
    /// the refusal is Pleroma's 403 `{"error":"This API requires an
    /// authenticated user"}` (see [`crate::api::accounts`]' account status
    /// route). A request that presents a live user token is never refused
    /// by this: the restriction is about the anonymous reader, not about
    /// the read.
    ///
    /// An `area` outside [`RESTRICT_UNAUTHENTICATED_AREAS`] restricts
    /// nothing — naming one is a startup failure
    /// ([`ConfigError::InvalidRestrictUnauthenticated`]), so reaching this
    /// with one can only be a caller's mistake, never a silent hole.
    #[must_use]
    pub fn restricts(&self, area: &str) -> bool {
        self.restrict_unauthenticated.contains(area)
    }

    /// The [`OAUTH_PROVIDERS`](OAUTH_PROVIDERS_VAR) entry named `name`,
    /// matched case-sensitively against [`OauthProvider::name`]; `None` is
    /// the endpoints' "Unknown provider" answer.
    #[must_use]
    pub fn oauth_provider(&self, name: &str) -> Option<&OauthProvider> {
        self.oauth_providers
            .iter()
            .find(|provider| provider.name == name)
    }

    /// Configuration for tests: `BASE_URL` is `https://test.example`,
    /// `HTTP_ALLOW_PRIVATE_HOSTS` is `true` (the suites talk to local
    /// mini-servers), `RATE_LIMITS_ENABLED` is `false` (so a suite can
    /// exercise an endpoint's own budget without tripping the limiter —
    /// tests that want throttling read the var through `from_lookup`),
    /// every other setting is at its default, and
    /// `DATABASE_URL` is an inert placeholder (tests get their real pool
    /// from the fixture in [`crate::test_db`]). Reads no environment
    /// variables, so it is safe in parallel tests.
    ///
    /// Available to unit tests through `cfg(test)` and to the integration
    /// tests through the `integration-tests` feature.
    #[cfg(any(test, feature = "integration-tests"))]
    #[must_use]
    pub fn for_tests() -> Self {
        // The inputs are fixed and valid, so `from_lookup` cannot fail; the
        // fallback arm builds the same config without a panic, keeping this
        // constructor infallible.
        Self::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            HTTP_ALLOW_PRIVATE_HOSTS_VAR => Some("true".to_owned()),
            RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
            RICH_MEDIA_IGNORE_HOSTS_VAR => Some(DEFAULT_RICH_MEDIA_IGNORE_HOSTS.to_owned()),
            RICH_MEDIA_REMOTE_VAR => Some(DEFAULT_RICH_MEDIA_REMOTE.to_string()),
            _ => None,
        })
        .unwrap_or_else(|_| Self::test_defaults())
    }

    /// The [`for_tests`](Self::for_tests) configuration built directly from
    /// the constants, without any parsing. Only reachable if parsing the
    /// fixed test values ever failed, which they cannot.
    #[cfg(any(test, feature = "integration-tests"))]
    fn test_defaults() -> Self {
        Self {
            database_url: TEST_DATABASE_URL.to_owned(),
            bind_addr: SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 4000)),
            base_url: TEST_BASE_URL.to_owned(),
            frontend_base_url: TEST_BASE_URL.to_owned(),
            instance_name: DEFAULT_INSTANCE_NAME.to_owned(),
            instance_description: DEFAULT_INSTANCE_DESCRIPTION.to_owned(),
            contact_email: None,
            instance_languages: comma_list(DEFAULT_INSTANCE_LANGUAGES),
            max_status_chars: DEFAULT_MAX_STATUS_CHARS,
            max_media_attachments: DEFAULT_MAX_MEDIA_ATTACHMENTS,
            max_poll_options: DEFAULT_MAX_POLL_OPTIONS,
            max_poll_option_chars: DEFAULT_MAX_POLL_OPTION_CHARS,
            min_poll_expiration_secs: DEFAULT_MIN_POLL_EXPIRATION,
            max_poll_expiration_secs: DEFAULT_MAX_POLL_EXPIRATION,
            max_chat_message_chars: DEFAULT_MAX_CHAT_MESSAGE_CHARS,
            max_media_description: DEFAULT_MAX_MEDIA_DESCRIPTION,
            max_display_name_chars: DEFAULT_MAX_DISPLAY_NAME_CHARS,
            max_bio_chars: DEFAULT_MAX_BIO_CHARS,
            max_profile_fields: DEFAULT_MAX_PROFILE_FIELDS,
            max_report_comment: DEFAULT_MAX_REPORT_COMMENT,
            max_image_pixels: DEFAULT_MAX_IMAGE_PIXELS,
            max_pinned_statuses: DEFAULT_MAX_PINNED_STATUSES,
            min_expiration_secs: DEFAULT_MIN_EXPIRATION_SECS,
            min_scheduled_secs: DEFAULT_MIN_SCHEDULED_SECS,
            max_scheduled_per_user: DEFAULT_MAX_SCHEDULED_PER_USER,
            max_scheduled_per_user_per_day: DEFAULT_MAX_SCHEDULED_PER_USER_PER_DAY,
            upload_limits: TEST_UPLOAD_LIMITS,
            avatar_limit_bytes: DEFAULT_AVATAR_LIMIT_BYTES,
            banner_limit_bytes: DEFAULT_BANNER_LIMIT_BYTES,
            inbox_max_bytes: DEFAULT_INBOX_MAX_BYTES,
            max_remote_mention_lookups: DEFAULT_MAX_REMOTE_MENTION_LOOKUPS,
            uploads_dir: PathBuf::from(DEFAULT_UPLOADS_DIR),
            emoji_dir: PathBuf::from(DEFAULT_EMOJI_DIR),
            media_base_url: format!("{TEST_BASE_URL}{DEFAULT_MEDIA_URL_PATH}"),
            accounts_email_optional: DEFAULT_ACCOUNTS_EMAIL_OPTIONAL,
            min_password_length: DEFAULT_MIN_PASSWORD_LENGTH,
            oauth_token_ttl_secs: DEFAULT_OAUTH_TOKEN_TTL_SECS,
            password_reset_ttl_secs: DEFAULT_PASSWORD_RESET_TTL_SECS,
            account_activation_required: Toggle::new(DEFAULT_ACCOUNT_ACTIVATION_REQUIRED),
            allow_account_deletion: Toggle::new(DEFAULT_ALLOW_ACCOUNT_DELETION),
            mongooseim_enabled: Toggle::new(DEFAULT_MONGOOSEIM_ENABLED),
            captcha_enabled: Toggle::new(DEFAULT_CAPTCHA_ENABLED),
            healthcheck_enabled: Toggle::new(DEFAULT_HEALTHCHECK_ENABLED),
            registrations_open: Toggle::new(DEFAULT_REGISTRATIONS_OPEN),
            invites_enabled: Toggle::new(DEFAULT_INVITES_ENABLED),
            registrations_approval_required: Toggle::new(DEFAULT_REGISTRATIONS_APPROVAL_REQUIRED),
            search_statuses_unauthenticated: Toggle::new(DEFAULT_SEARCH_STATUSES_UNAUTHENTICATED),
            restrict_unauthenticated: BTreeSet::new(),
            oauth_providers: Vec::new(),
            available_frontends: comma_list(DEFAULT_AVAILABLE_FRONTENDS),
            job_concurrency: DEFAULT_JOB_CONCURRENCY,
            job_queues: comma_list(DEFAULT_JOB_QUEUES),
            purge_deleted_statuses_days: DEFAULT_PURGE_DELETED_STATUSES_DAYS,
            backup_validity_days: DEFAULT_BACKUP_VALIDITY_DAYS,
            backup_limit_days: DEFAULT_BACKUP_LIMIT_DAYS,
            backup_purge: Toggle::new(DEFAULT_BACKUP_PURGE),
            streaming_ping_interval_secs: DEFAULT_STREAMING_PING_INTERVAL_SECS,
            smtp_host: None,
            smtp_port: DEFAULT_SMTP_PORT,
            smtp_username: None,
            smtp_password: None,
            smtp_tls: SmtpTls::default(),
            mail_from: default_mail_from(domain_of(TEST_BASE_URL)),
            secret_key_base: None,
            media_proxy: TEST_MEDIA_PROXY,
            rich_media_ignore_hosts: comma_list(DEFAULT_RICH_MEDIA_IGNORE_HOSTS),
            rich_media_remote: Toggle::new(DEFAULT_RICH_MEDIA_REMOTE),
            vapid: VapidConfig::default(),
            translation: TranslationConfig::default(),
            fetch: FetchSettings {
                http_allow_private_hosts: true,
                ..FetchSettings::default()
            },
            federation_enabled: Toggle::new(DEFAULT_FEDERATION_ENABLED),
            accept_unsolicited_public: Toggle::new(DEFAULT_ACCEPT_UNSOLICITED_PUBLIC),
            unreachable_grace_secs: DEFAULT_UNREACHABLE_GRACE_SECS,
            mrf: MrfSettings::test_defaults(),
            rate_limit: test_rate_limit(),
        }
    }
}

/// The shape [`endpoints`] answers: the database URL, the listen socket,
/// the public origin and the media URL prefix, in [`Config`] field order;
/// factored out because a four-value tuple is a very complex type to
/// spell inline.
type Endpoints = (String, SocketAddr, String, String);

/// Reads the four settings that say where the process and its media are
/// reached: the database URL it connects to, the socket address it listens
/// on, the public origin it is known by, and the media URL prefix that
/// defaults onto that origin. Grouped because each is a few lines of its
/// own and [`Config::from_lookup`] has a size budget (AGENTS.md); the tuple
/// comes back in [`Config`] field order. `media_base_url` is read last, off
/// the normalized origin it defaults onto.
///
/// # Errors
///
/// Returns [`ConfigError::Missing`] when `DATABASE_URL` or `BASE_URL` is
/// unset or blank, [`ConfigError::InvalidBindAddr`] when `BIND_ADDR` holds
/// something that is not a socket address, and
/// [`ConfigError::InvalidBaseUrl`] when `BASE_URL` is not an http(s)
/// origin without path or query.
fn endpoints<F>(lookup: &F) -> Result<Endpoints, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let database_url = required_var(lookup, DATABASE_URL_VAR)?;
    let bind_addr = listen_addr(lookup)?;
    let base_url = normalize_base_url(&required_var(lookup, BASE_URL_VAR)?)?;
    let media_base_url = media_base_url(lookup, &base_url);
    Ok((database_url, bind_addr, base_url, media_base_url))
}

/// Validates a `BASE_URL` value and normalizes it to the bare origin with no
/// trailing slash, e.g. `https://social.example`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBaseUrl`] unless the value is an absolute
/// `http`/`https` URL with a host and no path, query or trailing junk.
fn normalize_base_url(raw: &str) -> Result<String, ConfigError> {
    let Ok(url) = Url::parse(raw) else {
        return Err(ConfigError::InvalidBaseUrl);
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::InvalidBaseUrl);
    }
    let Some(host) = url.host_str() else {
        return Err(ConfigError::InvalidBaseUrl);
    };
    if !matches!(url.path(), "" | "/") || url.query().is_some() {
        return Err(ConfigError::InvalidBaseUrl);
    }
    let domain = url
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
    Ok(format!("{}://{domain}", url.scheme()))
}

/// What [`instance_profile`] reads: the four settings that describe the
/// instance to the world, named by what they are and hung on the
/// [`Config`] fields in [`Config::from_lookup`] (`INSTANCE_NAME`,
/// `INSTANCE_DESCRIPTION`, `CONTACT_EMAIL`, `INSTANCE_LANGUAGES`).
#[derive(Debug)]
struct InstanceProfile {
    /// The instance display name (`INSTANCE_NAME`).
    name: String,
    /// The short description (`INSTANCE_DESCRIPTION`).
    description: String,
    /// The admin contact email (`CONTACT_EMAIL`).
    contact_email: Option<String>,
    /// The content language codes (`INSTANCE_LANGUAGES`).
    languages: Vec<String>,
}

/// Reads the four settings that describe the instance to the world.
/// Grouped because each is a plain default-or-list and
/// [`Config::from_lookup`] has a size budget (AGENTS.md).
fn instance_profile<F>(lookup: &F) -> InstanceProfile
where
    F: Fn(&str) -> Option<String>,
{
    let name = lookup(INSTANCE_NAME_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_NAME.to_owned());
    let description =
        lookup(INSTANCE_DESCRIPTION_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_DESCRIPTION.to_owned());
    let contact_email = lookup(CONTACT_EMAIL_VAR).filter(|email| !email.is_empty());
    let languages = comma_list(
        &lookup(INSTANCE_LANGUAGES_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_LANGUAGES.to_owned()),
    );
    InstanceProfile {
        name,
        description,
        contact_email,
        languages,
    }
}

/// The shape [`job_worker`] answers: the worker's concurrent handler count,
/// its queue names in claim order, and the age in days after which the
/// daily sweep hard-deletes soft-deleted local statuses; factored out
/// because a mixed tuple of queues and numbers reads better by name (and a
/// bare `Result` of it trips `type_complexity`).
type JobWorkerSettings = (usize, Vec<String>, usize);

/// Reads the three settings that shape the job worker and its maintenance
/// purge: how many handlers a single worker runs at the same time, the
/// queue names it drains, in claim priority order, and the age in days
/// after which the daily sweep hard-deletes soft-deleted local statuses
/// ([`PURGE_DELETED_STATUSES_DAYS_VAR`], `0` disabling the purge). Grouped
/// because each is a plain default-or-parse and [`Config::from_lookup`]
/// has a size budget (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when `JOB_CONCURRENCY` or
/// `PURGE_DELETED_STATUSES_DAYS` holds a value that is not a whole number.
fn job_worker<F>(lookup: &F) -> Result<JobWorkerSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let job_concurrency = number(lookup, JOB_CONCURRENCY_VAR, DEFAULT_JOB_CONCURRENCY)?;
    let job_queues =
        comma_list(&lookup(JOB_QUEUES_VAR).unwrap_or_else(|| DEFAULT_JOB_QUEUES.to_owned()));
    let purge_deleted_statuses_days = number(
        lookup,
        PURGE_DELETED_STATUSES_DAYS_VAR,
        DEFAULT_PURGE_DELETED_STATUSES_DAYS,
    )?;
    Ok((job_concurrency, job_queues, purge_deleted_statuses_days))
}

/// Reads the three settings that shape the user-backup feature: the age in
/// days a finished backup archive stays downloadable
/// ([`BACKUP_VALIDITY_DAYS_VAR`]), the minimum number of days between two
/// backups of one user ([`BACKUP_LIMIT_DAYS_VAR`]) and whether the backup
/// purge sweep runs at all ([`BACKUP_PURGE_VAR`]). Grouped because each is a
/// plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when one of the two day settings
/// holds a value that is not a whole number, and
/// [`ConfigError::InvalidBoolean`] when `BACKUP_PURGE` holds a value that is
/// neither spelling.
fn backup_settings<F>(lookup: &F) -> Result<(usize, usize, Toggle), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let validity_days = number(
        lookup,
        BACKUP_VALIDITY_DAYS_VAR,
        DEFAULT_BACKUP_VALIDITY_DAYS,
    )?;
    let limit_days = number(lookup, BACKUP_LIMIT_DAYS_VAR, DEFAULT_BACKUP_LIMIT_DAYS)?;
    let purge = boolean(lookup, BACKUP_PURGE_VAR, DEFAULT_BACKUP_PURGE)?.into();
    Ok((validity_days, limit_days, purge))
}

/// The reader groups that share no theme — [`upload_settings`],
/// [`misc_toggles`], [`job_worker`], [`backup_settings`] and
/// [`fetch_settings`] — flattened into one named bundle so
/// [`Config::from_lookup`] binds all thirteen of their fields with a single
/// `let` and stays inside Clippy's line budget, the same move [`Groups`]
/// spells for the wording and signup trios. The field names mirror the
/// [`Config`] fields they hang on.
#[derive(Debug)]
struct RestSettings {
    /// Per-request limits for multipart uploads (`UPLOAD_LIMIT_BYTES` and
    /// `UPLOAD_MAX_FILES`).
    upload_limits: UploadLimits,
    /// Maximum avatar image size in bytes (`AVATAR_LIMIT_BYTES`).
    avatar_limit_bytes: usize,
    /// Maximum banner/background image size in bytes (`BANNER_LIMIT_BYTES`).
    banner_limit_bytes: usize,
    /// Whether the `MongooseIM` auth endpoints are served
    /// (`MONGOOSEIM_ENABLED`).
    mongooseim_enabled: Toggle,
    /// Whether a captcha provider is configured (`CAPTCHA_ENABLED`).
    captcha_enabled: Toggle,
    /// Whether the healthcheck endpoint reports pool data
    /// (`HEALTHCHECK_ENABLED`).
    healthcheck_enabled: Toggle,
    /// The job worker's concurrent handler count (`JOB_CONCURRENCY`).
    job_concurrency: usize,
    /// The job queue names the worker drains (`JOB_QUEUES`).
    job_queues: Vec<String>,
    /// The deleted-status purge age (`PURGE_DELETED_STATUSES_DAYS`).
    purge_deleted_statuses_days: usize,
    /// The days a backup archive stays downloadable (`BACKUP_VALIDITY_DAYS`).
    backup_validity_days: usize,
    /// The minimum days between two backups of one user
    /// (`BACKUP_LIMIT_DAYS`).
    backup_limit_days: usize,
    /// Whether the backup purge sweep runs at all (`BACKUP_PURGE`).
    backup_purge: Toggle,
    /// The settings that govern how this instance fetches and is fetched by
    /// peers, from [`fetch_settings`].
    fetch: FetchSettings,
}

/// Reads the five groups [`RestSettings`] bundles.
///
/// # Errors
///
/// Those of [`upload_settings`], [`misc_toggles`], [`job_worker`],
/// [`backup_settings`] and [`fetch_settings`].
fn rest_settings<F>(lookup: &F) -> Result<RestSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let (upload_limits, avatar_limit_bytes, banner_limit_bytes) = upload_settings(lookup)?;
    let (mongooseim_enabled, captcha_enabled, healthcheck_enabled) = misc_toggles(lookup)?;
    let (job_concurrency, job_queues, purge_deleted_statuses_days) = job_worker(lookup)?;
    let (backup_validity_days, backup_limit_days, backup_purge) = backup_settings(lookup)?;
    let fetch = fetch_settings(lookup)?;
    Ok(RestSettings {
        upload_limits,
        avatar_limit_bytes,
        banner_limit_bytes,
        mongooseim_enabled,
        captcha_enabled,
        healthcheck_enabled,
        job_concurrency,
        job_queues,
        purge_deleted_statuses_days,
        backup_validity_days,
        backup_limit_days,
        backup_purge,
        fetch,
    })
}

/// Reads the one setting that shapes the streaming WebSocket: how many
/// seconds pass between two heartbeat pings of one socket
/// (`STREAMING_PING_INTERVAL_SECS`, 30 — Pleroma's `@tick` — when unset).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when the variable holds a value
/// that is not a whole number.
fn streaming_heartbeat<F>(lookup: &F) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    number(
        lookup,
        STREAMING_PING_INTERVAL_SECS_VAR,
        DEFAULT_STREAMING_PING_INTERVAL_SECS,
    )
}

/// Reads `BIND_ADDR`: the address the HTTP server listens on,
/// [`DEFAULT_BIND_ADDR`] when unset. Factored out of
/// [`Config::from_lookup`], which has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBindAddr`] when the value is not a socket
/// address.
fn listen_addr<F>(lookup: &F) -> Result<SocketAddr, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(BIND_ADDR_VAR)
        .as_deref()
        .unwrap_or(DEFAULT_BIND_ADDR)
        .parse()
        .map_err(ConfigError::InvalidBindAddr)
}

/// What [`mailer_settings`] reads: the six settings that configure
/// outbound mail, named by what they are and hung on the [`Config`] fields
/// in [`Config::from_lookup`] (`SMTP_HOST`, `SMTP_PORT`, `SMTP_USERNAME`,
/// `SMTP_PASSWORD`, `SMTP_TLS`, `MAIL_FROM`).
#[derive(Debug)]
struct MailerSettings {
    /// The relay host; `None` means mail is disabled.
    host: Option<String>,
    /// The relay port.
    port: u16,
    /// The login name.
    username: Option<String>,
    /// The login secret.
    password: Option<String>,
    /// The encryption mode.
    tls: SmtpTls,
    /// The sender address.
    from: String,
}

/// Reads the six settings that configure outbound mail: the relay host
/// (unset means mail is disabled), the relay port, the login name and
/// secret, the encryption mode and the sender address. Grouped because each
/// is a plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md). `domain` is the [`domain_of`] the default sender address is
/// built from.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when `SMTP_PORT` is not a whole
/// number in the `u16` range, and [`ConfigError::InvalidSmtpTls`] when
/// `SMTP_TLS` names no known mode.
fn mailer_settings<F>(lookup: &F, domain: &str) -> Result<MailerSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let smtp_host = lookup(SMTP_HOST_VAR)
        .map(|host| host.trim().to_owned())
        .filter(|host| !host.is_empty());
    let smtp_port = smtp_port(lookup)?;
    let smtp_username = lookup(SMTP_USERNAME_VAR)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    let smtp_password = lookup(SMTP_PASSWORD_VAR).filter(|secret| !secret.is_empty());
    let smtp_tls = smtp_tls(lookup)?;
    let mail_from = lookup(MAIL_FROM_VAR)
        .map(|from| from.trim().to_owned())
        .filter(|from| !from.is_empty())
        .unwrap_or_else(|| default_mail_from(domain));
    Ok(MailerSettings {
        host: smtp_host,
        port: smtp_port,
        username: smtp_username,
        password: smtp_password,
        tls: smtp_tls,
        from: mail_from,
    })
}

/// Reads `SMTP_PORT`: a whole number in the `u16` range,
/// [`DEFAULT_SMTP_PORT`] when unset.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] for a non-number or a value
/// outside the port range.
fn smtp_port<F>(lookup: &F) -> Result<u16, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let port = number(lookup, SMTP_PORT_VAR, usize::from(DEFAULT_SMTP_PORT))?;
    u16::try_from(port).map_err(|_out_of_range| ConfigError::InvalidNumber(SMTP_PORT_VAR))
}

/// Reads `SMTP_TLS`: one of the [`SmtpTls`] spellings, case-insensitive,
/// [`SmtpTls::default`] when unset or blank.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidSmtpTls`] for any other spelling.
fn smtp_tls<F>(lookup: &F) -> Result<SmtpTls, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(SMTP_TLS_VAR).filter(|value| !value.trim().is_empty()) else {
        return Ok(SmtpTls::default());
    };
    SmtpTls::parse(&raw).ok_or(ConfigError::InvalidSmtpTls(SMTP_TLS_VAR))
}

/// The [`Config::domain`] of a normalized base URL: everything after the
/// `://`.
fn domain_of(base_url: &str) -> &str {
    base_url
        .split_once("://")
        .map_or(base_url, |(_, domain)| domain)
}

/// The `MAIL_FROM` value used when the variable is unset:
/// `noreply@<instance domain>`. A port is not part of an email domain, so
/// `localhost:4000` contributes only `localhost`.
fn default_mail_from(domain: &str) -> String {
    let host = domain.split_once(':').map_or(domain, |(host, _)| host);
    format!("{DEFAULT_MAIL_FROM_USER}@{host}")
}

/// Reads `SECRET_KEY_BASE`: the trimmed signing secret, `None` when the
/// variable is unset or blank, in which case [`serve`](crate::serve)
/// fills the field with a secret generated for the boot. Read through its
/// own fn because [`Config::from_lookup`] has a size budget (AGENTS.md).
fn secret_key_base<F>(lookup: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(SECRET_KEY_BASE_VAR)
        .map(|secret| secret.trim().to_owned())
        .filter(|secret| !secret.is_empty())
}

/// What [`auth_settings`] reads: the six settings that gate
/// registration, token issuance, password resets, the external logins and
/// account deletion, in [`Config`] field order.
#[derive(Debug)]
struct AuthSettings {
    /// Whether registration may omit the email address
    /// (`ACCOUNTS_EMAIL_OPTIONAL`).
    accounts_email_optional: bool,
    /// The registration password minimum (`MIN_PASSWORD_LENGTH`).
    min_password_length: usize,
    /// The access-token lifetime (`OAUTH_TOKEN_TTL_SECS`).
    oauth_token_ttl_secs: usize,
    /// The password-reset token lifetime (`PASSWORD_RESET_TTL_SECS`).
    password_reset_ttl_secs: usize,
    /// Whether accounts may delete their own account through
    /// `POST /api/pleroma/delete_account` (`ALLOW_ACCOUNT_DELETION`).
    allow_account_deletion: Toggle,
    /// The external login providers (`OAUTH_PROVIDERS`).
    oauth_providers: Vec<OauthProvider>,
}

/// Reads the six settings that gate registration, token issuance,
/// password resets, the external logins and account deletion. Email
/// confirmation lives with [`registration_modes`] instead. Grouped
/// because each is a plain default-or-parse and [`Config::from_lookup`]
/// has a size budget (AGENTS.md).
fn auth_settings<F>(lookup: &F) -> Result<AuthSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let accounts_email_optional = boolean(
        lookup,
        ACCOUNTS_EMAIL_OPTIONAL_VAR,
        DEFAULT_ACCOUNTS_EMAIL_OPTIONAL,
    )?;
    let min_password_length = number(lookup, MIN_PASSWORD_LENGTH_VAR, DEFAULT_MIN_PASSWORD_LENGTH)?;
    let oauth_token_ttl_secs = number(
        lookup,
        OAUTH_TOKEN_TTL_SECS_VAR,
        DEFAULT_OAUTH_TOKEN_TTL_SECS,
    )?;
    let password_reset_ttl_secs = number(
        lookup,
        PASSWORD_RESET_TTL_SECS_VAR,
        DEFAULT_PASSWORD_RESET_TTL_SECS,
    )?;
    let allow_account_deletion = boolean(
        lookup,
        ALLOW_ACCOUNT_DELETION_VAR,
        DEFAULT_ALLOW_ACCOUNT_DELETION,
    )?
    .into();
    Ok(AuthSettings {
        accounts_email_optional,
        min_password_length,
        oauth_token_ttl_secs,
        password_reset_ttl_secs,
        allow_account_deletion,
        oauth_providers: oauth_providers(lookup)?,
    })
}

/// Reads one directory-location variable (`UPLOADS_DIR`, `EMOJI_DIR`):
/// the configured value trimmed, or `default` when it is unset or blank.
/// Factored out of [`Config::from_lookup`], which has a size budget
/// (AGENTS.md), because each path keeps its own field there.
fn directory<F>(lookup: &F, var: &'static str, default: &str) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    PathBuf::from(
        lookup(var)
            .map(|dir| dir.trim().to_owned())
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| default.to_owned()),
    )
}

/// Reads `MEDIA_BASE_URL`: the URL prefix media is served under, trimmed
/// and with its trailing slash dropped, or `base_url` — the normalized
/// origin — with the default media path hung onto it when unset or blank.
/// Factored out of [`endpoints`], which reads it last, off the origin it
/// defaults onto.
fn media_base_url<F>(lookup: &F, base_url: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    lookup(MEDIA_BASE_URL_VAR)
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| format!("{base_url}{DEFAULT_MEDIA_URL_PATH}"))
}

/// Reads `FRONTEND_BASE_URL`: the origin the frontend pages are served
/// from, trimmed and with its trailing slashes dropped, or `base_url` — the
/// normalized origin — when unset or blank. The `/notice/{id}` browser
/// redirect (a later slice) hangs its path onto this value, so it gets the
/// same trimmed, slash-stripped reading as the media base URL and the media
/// proxy's base URL: nothing unusable ever reaches a `Location` header.
/// Factored out of [`Config::from_lookup`], which has a size budget
/// (AGENTS.md), because it defaults onto the origin `endpoints` normalizes.
fn frontend_base_url<F>(lookup: &F, base_url: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    lookup(FRONTEND_BASE_URL_VAR)
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| base_url.to_owned())
}

/// Reads the three feature toggles that gate the endpoints of
/// [`crate::api::misc`]: the `MongooseIM` auth endpoints, the captcha
/// provider and the healthcheck. Grouped because each is a plain
/// default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] for whichever variable holds a
/// value that is neither spelling.
fn misc_toggles<F>(lookup: &F) -> Result<(Toggle, Toggle, Toggle), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        boolean(lookup, MONGOOSEIM_ENABLED_VAR, DEFAULT_MONGOOSEIM_ENABLED)?.into(),
        boolean(lookup, CAPTCHA_ENABLED_VAR, DEFAULT_CAPTCHA_ENABLED)?.into(),
        boolean(lookup, HEALTHCHECK_ENABLED_VAR, DEFAULT_HEALTHCHECK_ENABLED)?.into(),
    ))
}

/// What [`content_limits`] reads: the eleven plain numeric content limits,
/// named by what they cap and hung on the [`Config`] fields in
/// [`Config::from_lookup`] (`MAX_STATUS_CHARS`, `MAX_MEDIA_ATTACHMENTS`,
/// `MAX_POLL_OPTIONS`, `MAX_POLL_OPTION_CHARS`, `MIN_POLL_EXPIRATION`,
/// `MAX_POLL_EXPIRATION`, `MAX_CHAT_MESSAGE_CHARS`, `MAX_IMAGE_PIXELS`,
/// `MAX_REPORT_COMMENT`, `MAX_PINNED_STATUSES`, `MIN_EXPIRATION_SECS`).
#[derive(Debug)]
struct ContentLimits {
    /// The maximum status length (`MAX_STATUS_CHARS`).
    status_chars: usize,
    /// The media attachment count (`MAX_MEDIA_ATTACHMENTS`).
    media_attachments: usize,
    /// The poll option count (`MAX_POLL_OPTIONS`).
    poll_options: usize,
    /// The poll option length (`MAX_POLL_OPTION_CHARS`).
    poll_option_chars: usize,
    /// The shortest poll expiration, in seconds (`MIN_POLL_EXPIRATION`).
    min_poll_expiration_secs: usize,
    /// The longest poll expiration, in seconds (`MAX_POLL_EXPIRATION`).
    max_poll_expiration_secs: usize,
    /// The chat message length (`MAX_CHAT_MESSAGE_CHARS`).
    chat_message_chars: usize,
    /// The pixel count an uploaded image may decode to (`MAX_IMAGE_PIXELS`).
    image_pixels: usize,
    /// The length of a report's `comment` (`MAX_REPORT_COMMENT`).
    report_comment: usize,
    /// The number of statuses one account may pin (`MAX_PINNED_STATUSES`).
    pinned_statuses: usize,
    /// The shortest status expiration, in seconds (`MIN_EXPIRATION_SECS`).
    min_expiration_secs: usize,
    /// The largest inbound `ActivityPub` inbox body, in bytes
    /// (`INBOX_MAX_BYTES`).
    inbox_max_bytes: usize,
    /// The unknown-remote mentions one status may look up
    /// (`MAX_REMOTE_MENTION_LOOKUPS`).
    max_remote_mention_lookups: usize,
    /// How far in the future a `scheduled_at` create must be, in seconds
    /// (`MIN_SCHEDULED_SECS`).
    min_scheduled_secs: usize,
    /// How many pending scheduled rows one account may hold
    /// (`MAX_SCHEDULED_PER_USER`).
    max_scheduled_per_user: usize,
    /// How many pending scheduled rows one account may hold for one UTC
    /// day (`MAX_SCHEDULED_PER_USER_PER_DAY`).
    max_scheduled_per_user_per_day: usize,
}

/// Reads the sixteen plain numeric content limits: the maximum status length
/// (`MAX_STATUS_CHARS`), the media attachment count
/// (`MAX_MEDIA_ATTACHMENTS`), the poll option count (`MAX_POLL_OPTIONS`),
/// the poll option length (`MAX_POLL_OPTION_CHARS`), the shortest
/// (`MIN_POLL_EXPIRATION`) and longest (`MAX_POLL_EXPIRATION`) poll
/// expiration, the chat message length (`MAX_CHAT_MESSAGE_CHARS`), the
/// pixel count an uploaded image may decode to (`MAX_IMAGE_PIXELS`), the
/// length of a report's `comment` (`MAX_REPORT_COMMENT`), the number of
/// statuses one account may pin to its profile (`MAX_PINNED_STATUSES`),
/// the shortest `expires_in` a status create may ask for
/// (`MIN_EXPIRATION_SECS`), the largest inbound inbox body
/// (`INBOX_MAX_BYTES`), the unknown-remote mentions one status may look
/// up (`MAX_REMOTE_MENTION_LOOKUPS`) and the three settings that bound
/// scheduled statuses: the `scheduled_at` window (`MIN_SCHEDULED_SECS`),
/// the pending rows one account may hold (`MAX_SCHEDULED_PER_USER`) and
/// the pending rows it may hold for one UTC day
/// (`MAX_SCHEDULED_PER_USER_PER_DAY`). Grouped because each is the same
/// `number` read and [`Config::from_lookup`] has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] for whichever variable holds a
/// value that is not a whole number.
fn content_limits<F>(lookup: &F) -> Result<ContentLimits, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(ContentLimits {
        status_chars: number(lookup, MAX_STATUS_CHARS_VAR, DEFAULT_MAX_STATUS_CHARS)?,
        media_attachments: number(
            lookup,
            MAX_MEDIA_ATTACHMENTS_VAR,
            DEFAULT_MAX_MEDIA_ATTACHMENTS,
        )?,
        poll_options: number(lookup, MAX_POLL_OPTIONS_VAR, DEFAULT_MAX_POLL_OPTIONS)?,
        poll_option_chars: number(
            lookup,
            MAX_POLL_OPTION_CHARS_VAR,
            DEFAULT_MAX_POLL_OPTION_CHARS,
        )?,
        min_poll_expiration_secs: number(
            lookup,
            MIN_POLL_EXPIRATION_VAR,
            DEFAULT_MIN_POLL_EXPIRATION,
        )?,
        max_poll_expiration_secs: number(
            lookup,
            MAX_POLL_EXPIRATION_VAR,
            DEFAULT_MAX_POLL_EXPIRATION,
        )?,
        chat_message_chars: number(
            lookup,
            MAX_CHAT_MESSAGE_CHARS_VAR,
            DEFAULT_MAX_CHAT_MESSAGE_CHARS,
        )?,
        image_pixels: number(lookup, MAX_IMAGE_PIXELS_VAR, DEFAULT_MAX_IMAGE_PIXELS)?,
        report_comment: number(lookup, MAX_REPORT_COMMENT_VAR, DEFAULT_MAX_REPORT_COMMENT)?,
        pinned_statuses: number(lookup, MAX_PINNED_STATUSES_VAR, DEFAULT_MAX_PINNED_STATUSES)?,
        min_expiration_secs: number(lookup, MIN_EXPIRATION_SECS_VAR, DEFAULT_MIN_EXPIRATION_SECS)?,
        inbox_max_bytes: number(lookup, INBOX_MAX_BYTES_VAR, DEFAULT_INBOX_MAX_BYTES)?,
        max_remote_mention_lookups: number(
            lookup,
            MAX_REMOTE_MENTION_LOOKUPS_VAR,
            DEFAULT_MAX_REMOTE_MENTION_LOOKUPS,
        )?,
        min_scheduled_secs: number(lookup, MIN_SCHEDULED_SECS_VAR, DEFAULT_MIN_SCHEDULED_SECS)?,
        max_scheduled_per_user: number(
            lookup,
            MAX_SCHEDULED_PER_USER_VAR,
            DEFAULT_MAX_SCHEDULED_PER_USER,
        )?,
        max_scheduled_per_user_per_day: number(
            lookup,
            MAX_SCHEDULED_PER_USER_PER_DAY_VAR,
            DEFAULT_MAX_SCHEDULED_PER_USER_PER_DAY,
        )?,
    })
}

/// Reads the settings that govern how this instance fetches and is
/// fetched by peers: whether the client may reach private/loopback hosts
/// (`HTTP_ALLOW_PRIVATE_HOSTS`), whether it signs its outgoing actor
/// `GET`s (`SIGN_FETCHES`), whether it demands signatures on incoming
/// `ActivityPub` `GET`s (`AUTHORIZED_FETCH`), and the two numbers that
/// bound how far fetching may go (`THREAD_FETCH_DEPTH`,
/// `MAX_FETCHES_PER_ACTIVITY`, read through [`fetch_limits`]). Grouped
/// into one [`FetchSettings`] because each is a plain default-or-parse
/// and [`Config::from_lookup`] has a size budget (AGENTS.md) — and
/// because the settings belong together on the wire: they are the shape
/// of this instance's outbound `GET`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] for whichever boolean variable
/// holds a value that is neither spelling, and
/// [`ConfigError::InvalidNumber`] for whichever fetch-limit variable holds
/// a value that is not a whole number.
fn fetch_settings<F>(lookup: &F) -> Result<FetchSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(FetchSettings {
        http_allow_private_hosts: boolean(
            lookup,
            HTTP_ALLOW_PRIVATE_HOSTS_VAR,
            DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS,
        )?,
        sign_fetches: boolean(lookup, SIGN_FETCHES_VAR, DEFAULT_SIGN_FETCHES)?.into(),
        authorized_fetch: boolean(lookup, AUTHORIZED_FETCH_VAR, DEFAULT_AUTHORIZED_FETCH)?.into(),
        limits: fetch_limits(lookup)?,
    })
}

/// Reads the two numbers that bound how far federation fetching may go:
/// how deep the [`federator.backfill`] job walks a thread
/// (`THREAD_FETCH_DEPTH`) and how many object dereferences one run may
/// trigger (`MAX_FETCHES_PER_ACTIVITY`).
///
/// [`federator.backfill`]: crate::federation::fetch_object::KIND_BACKFILL
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] for whichever variable holds a
/// value that is not a whole number.
fn fetch_limits<F>(lookup: &F) -> Result<FetchLimits, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(FetchLimits {
        thread_fetch_depth: number(lookup, THREAD_FETCH_DEPTH_VAR, DEFAULT_THREAD_FETCH_DEPTH)?,
        max_fetches_per_activity: number(
            lookup,
            MAX_FETCHES_PER_ACTIVITY_VAR,
            DEFAULT_MAX_FETCHES_PER_ACTIVITY,
        )?,
    })
}

/// Reads `FEDERATION_ENABLED`: whether this instance federates at all,
/// [`DEFAULT_FEDERATION_ENABLED`] when unset or blank. Read as its own
/// setting because the inbox routes and the delivery enqueuer both gate on
/// it (see [`crate::api::activitypub`] and
/// [`crate::federation::delivery`]).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when the variable holds a value
/// that is neither spelling.
fn federation_enabled<F>(lookup: &F) -> Result<Toggle, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(boolean(lookup, FEDERATION_ENABLED_VAR, DEFAULT_FEDERATION_ENABLED)?.into())
}

/// Reads `ACCEPT_UNSOLICITED_PUBLIC`, [`DEFAULT_ACCEPT_UNSOLICITED_PUBLIC`]
/// when unset: the ingest gate on public and unlisted posts from authors
/// nobody follows (see [`crate::services::ingest`]).
fn accept_unsolicited_public<F>(lookup: &F) -> Result<Toggle, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(boolean(
        lookup,
        ACCEPT_UNSOLICITED_PUBLIC_VAR,
        DEFAULT_ACCEPT_UNSOLICITED_PUBLIC,
    )?
    .into())
}

/// Reads `UNREACHABLE_GRACE_SECS`, [`DEFAULT_UNREACHABLE_GRACE_SECS`] when
/// unset: how long a host marked unreachable stays deliverable, so
/// [`crate::federation::publisher`] can hold a freshly marked peer in the
/// target set for one more retry cycle.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when the variable holds a value
/// that is not a whole number.
fn unreachable_grace_secs<F>(lookup: &F) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    number(
        lookup,
        UNREACHABLE_GRACE_SECS_VAR,
        DEFAULT_UNREACHABLE_GRACE_SECS,
    )
}

/// Reads the MRF group: the `MRF_POLICIES` fallback chain (empty when unset
/// or blank), the `MRF_TRANSPARENCY` switch on publishing the moderation
/// lists, and the `MRF_TRANSPARENCY_EXCLUSIONS` host patterns kept out of
/// them (see [`crate::federation::mrf`]). Grouped because every [`Config`]
/// reader works inside a line budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when `MRF_TRANSPARENCY` holds a
/// value that is neither spelling.
fn mrf_settings<F>(lookup: &F) -> Result<MrfSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(MrfSettings {
        policies: comma_list(&lookup(MRF_POLICIES_VAR).unwrap_or_default()),
        transparency: boolean(lookup, MRF_TRANSPARENCY_VAR, DEFAULT_MRF_TRANSPARENCY)?.into(),
        transparency_exclusions: comma_list(
            &lookup(MRF_TRANSPARENCY_EXCLUSIONS_VAR).unwrap_or_default(),
        ),
    })
}

/// What [`text_length_limits`] reads: the four default-or-parse profile
/// limits, named by what they cap and hung on the [`Config`] fields in
/// [`Config::from_lookup`] (`MAX_MEDIA_DESCRIPTION`,
/// `MAX_DISPLAY_NAME_CHARS`, `MAX_BIO_CHARS`, `MAX_PROFILE_FIELDS`).
#[derive(Debug)]
struct TextLengthLimits {
    /// The media attachment description length (`MAX_MEDIA_DESCRIPTION`).
    media_description: usize,
    /// The display name length (`MAX_DISPLAY_NAME_CHARS`).
    display_name_chars: usize,
    /// The profile bio length (`MAX_BIO_CHARS`).
    bio_chars: usize,
    /// The profile metadata field count (`MAX_PROFILE_FIELDS`).
    profile_fields: usize,
}

/// Reads the four per-field profile limits that live beside the status
/// ones: the media attachment description (`MAX_MEDIA_DESCRIPTION`), the
/// display name (`MAX_DISPLAY_NAME_CHARS`), the profile bio
/// (`MAX_BIO_CHARS`) and the profile metadata field count
/// (`MAX_PROFILE_FIELDS`). Grouped because each is a plain default-or-parse
/// and [`Config::from_lookup`] has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when any of the four variables
/// holds a value that is not a whole number.
fn text_length_limits<F>(lookup: &F) -> Result<TextLengthLimits, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(TextLengthLimits {
        media_description: number(
            lookup,
            MAX_MEDIA_DESCRIPTION_VAR,
            DEFAULT_MAX_MEDIA_DESCRIPTION,
        )?,
        display_name_chars: number(
            lookup,
            MAX_DISPLAY_NAME_CHARS_VAR,
            DEFAULT_MAX_DISPLAY_NAME_CHARS,
        )?,
        bio_chars: number(lookup, MAX_BIO_CHARS_VAR, DEFAULT_MAX_BIO_CHARS)?,
        profile_fields: number(lookup, MAX_PROFILE_FIELDS_VAR, DEFAULT_MAX_PROFILE_FIELDS)?,
    })
}

/// The shape [`profile_and_limits`] answers: the instance's profile, its
/// numeric content limits and its per-field text limits, in the order the
/// three groups feed the [`Config`] fields; factored out because a
/// three-value tuple of structs is a very complex type to spell inline.
type Wording = (InstanceProfile, ContentLimits, TextLengthLimits);

/// Reads the three groups that shape what the instance says about itself
/// and how much of it a client may send: the [`instance_profile`], the
/// [`content_limits`] and the [`text_length_limits`] beside it. Grouped
/// because [`Config::from_lookup`] has a size budget (AGENTS.md); see
/// [`Wording`].
///
/// # Errors
///
/// Those of [`content_limits`] and [`text_length_limits`].
fn profile_and_limits<F>(lookup: &F) -> Result<Wording, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        instance_profile(lookup),
        content_limits(lookup)?,
        text_length_limits(lookup)?,
    ))
}

/// The shape [`upload_settings`] answers: the per-request multipart limits
/// and the two profile-image byte caps, in [`Config`] field order; factored
/// out because a mixed tuple of a struct and two numbers reads better by
/// name.
type UploadSettings = (UploadLimits, usize, usize);

/// Reads the settings that bound uploads: the per-request multipart limits
/// (`UPLOAD_LIMIT_BYTES` and `UPLOAD_MAX_FILES`) and the two profile-image
/// byte caps (`AVATAR_LIMIT_BYTES` for the avatar, `BANNER_LIMIT_BYTES` for
/// the banner and the background image alike). Grouped because each is a
/// plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when any of the four variables
/// holds a value that is not a whole number.
fn upload_settings<F>(lookup: &F) -> Result<UploadSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        UploadLimits {
            max_file_bytes: number(lookup, UPLOAD_LIMIT_BYTES_VAR, DEFAULT_UPLOAD_LIMIT_BYTES)?,
            max_files: number(lookup, UPLOAD_MAX_FILES_VAR, DEFAULT_UPLOAD_MAX_FILES)?,
        },
        number(lookup, AVATAR_LIMIT_BYTES_VAR, DEFAULT_AVATAR_LIMIT_BYTES)?,
        number(lookup, BANNER_LIMIT_BYTES_VAR, DEFAULT_BANNER_LIMIT_BYTES)?,
    ))
}

/// What [`client_exposure`] reads: the toggles that gate what the instance
/// exposes to clients, the areas it keeps from anonymous readers and the
/// pickable-frontend list, named by what they are and hung on the [`Config`]
/// fields in [`Config::from_lookup`] (`REGISTRATIONS_OPEN`,
/// `INVITES_ENABLED`, `SEARCH_STATUSES_UNAUTHENTICATED`,
/// `RESTRICT_UNAUTHENTICATED`, `AVAILABLE_FRONTENDS`).
#[derive(Debug)]
struct ExposureSettings {
    /// Whether new registrations are open (`REGISTRATIONS_OPEN`).
    registrations_open: Toggle,
    /// Whether invite tokens substitute for open registrations
    /// (`INVITES_ENABLED`).
    invites_enabled: Toggle,
    /// Whether a new account waits for staff approval
    /// (`REGISTRATIONS_APPROVAL_REQUIRED`).
    registrations_approval_required: Toggle,
    /// Whether a new account must confirm its email address before it may
    /// log in (`ACCOUNT_ACTIVATION_REQUIRED`).
    account_activation_required: Toggle,
    /// Whether anonymous requests may search statuses at the combined
    /// search (`SEARCH_STATUSES_UNAUTHENTICATED`).
    search_statuses_unauthenticated: Toggle,
    /// The areas an anonymous request may not read
    /// (`RESTRICT_UNAUTHENTICATED`).
    restrict_unauthenticated: BTreeSet<String>,
    /// The frontends the settings endpoints let a user pick
    /// (`AVAILABLE_FRONTENDS`).
    available_frontends: Vec<String>,
}

/// Reads the settings that gate what the instance exposes to clients:
/// whether new registrations are open (`NodeInfo`'s `openRegistrations`),
/// whether invite tokens substitute for open registrations
/// (`invites_enabled` on the instance endpoints), what a finished
/// registration means — waiting for staff approval and confirming the
/// email ([`registration_modes`]) — whether anonymous requests may search
/// statuses at the combined search, which areas anonymous reads are
/// refused, and the frontends the settings endpoints let a user pick.
/// Grouped because each is a plain default-or-parse and
/// [`Config::from_lookup`] has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when `REGISTRATIONS_OPEN`,
/// `INVITES_ENABLED`, `REGISTRATIONS_APPROVAL_REQUIRED`,
/// `ACCOUNT_ACTIVATION_REQUIRED` or `SEARCH_STATUSES_UNAUTHENTICATED`
/// holds a value that is neither spelling, and whatever [`restrict_areas`]
/// reports.
fn client_exposure<F>(lookup: &F) -> Result<ExposureSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let registrations_open =
        boolean(lookup, REGISTRATIONS_OPEN_VAR, DEFAULT_REGISTRATIONS_OPEN)?.into();
    let invites_enabled = boolean(lookup, INVITES_ENABLED_VAR, DEFAULT_INVITES_ENABLED)?.into();
    let (registrations_approval_required, account_activation_required) =
        registration_modes(lookup)?;
    let search_statuses_unauthenticated = boolean(
        lookup,
        SEARCH_STATUSES_UNAUTHENTICATED_VAR,
        DEFAULT_SEARCH_STATUSES_UNAUTHENTICATED,
    )?
    .into();
    let restrict_unauthenticated = restrict_areas(lookup)?;
    let available_frontends = comma_list(
        &lookup(AVAILABLE_FRONTENDS_VAR).unwrap_or_else(|| DEFAULT_AVAILABLE_FRONTENDS.to_owned()),
    );
    Ok(ExposureSettings {
        registrations_open,
        invites_enabled,
        registrations_approval_required,
        account_activation_required,
        search_statuses_unauthenticated,
        restrict_unauthenticated,
        available_frontends,
    })
}

/// The shape [`signup_and_mail`] answers: the account settings, what the
/// client surface exposes and the mailer settings the confirmation mails
/// go out over, in the order the three groups feed the [`Config`] fields;
/// factored out because a three-value tuple of structs is a very complex
/// type to spell inline.
type Signup = (AuthSettings, ExposureSettings, MailerSettings);

/// What [`wording_and_mail`] answers: [`Wording`] and [`Signup`] flattened
/// into one six-value tuple, so [`Config::from_lookup`] binds all six
/// groups with a single `let` and stays inside Clippy's line budget.
type Groups = (
    InstanceProfile,
    ContentLimits,
    TextLengthLimits,
    AuthSettings,
    ExposureSettings,
    MailerSettings,
);

/// [`profile_and_limits`] and [`signup_and_mail`] in one call: the
/// instance-wording trio followed by the signup, exposure and mailer
/// trio, flattened. `base_url` is the instance origin, whose domain the
/// mailer's `From:` default is derived from (the pair the two grouped
/// readers used to take apart).
fn wording_and_mail<F>(lookup: &F, base_url: &str) -> Result<Groups, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let (profile, limits, text) = profile_and_limits(lookup)?;
    let (auth, exposure, mail) = signup_and_mail(lookup, domain_of(base_url))?;
    Ok((profile, limits, text, auth, exposure, mail))
}

/// Reads the three groups behind who may get an account and how they are
/// reached: the [`auth_settings`], the [`client_exposure`] the registration
/// modes grew, and the [`mailer_settings`] the approval and confirmation
/// mails go out over, built for `domain` — the instance's own, read off
/// the normalized base URL. Grouped because [`Config::from_lookup`] has a
/// size budget (AGENTS.md); see [`Signup`].
///
/// # Errors
///
/// Those of [`auth_settings`], [`client_exposure`] and [`mailer_settings`].
fn signup_and_mail<F>(lookup: &F, domain: &str) -> Result<Signup, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        auth_settings(lookup)?,
        client_exposure(lookup)?,
        mailer_settings(lookup, domain)?,
    ))
}

/// The areas [`RESTRICT_UNAUTHENTICATED_VAR`] names, as a set of the
/// canonical lowercase spellings in
/// [`RESTRICT_UNAUTHENTICATED_AREAS`]: a comma-separated list, each entry
/// trimmed and lowercased, so repeats fold and the order the operator wrote
/// is irrelevant; unset or empty restricts nothing.
///
/// # Errors
///
/// [`ConfigError::InvalidRestrictUnauthenticated`] for a name outside the
/// six areas — an operator's typo must stop the process, not silently leave
/// the profiles it meant to close open.
fn restrict_areas<F>(lookup: &F) -> Result<BTreeSet<String>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let configured = lookup(RESTRICT_UNAUTHENTICATED_VAR);
    let text = configured
        .as_deref()
        .unwrap_or(DEFAULT_RESTRICT_UNAUTHENTICATED);
    comma_list(text)
        .into_iter()
        .map(|area| {
            let area = area.to_lowercase();
            if RESTRICT_UNAUTHENTICATED_AREAS.contains(&area.as_str()) {
                Ok(area)
            } else {
                Err(ConfigError::InvalidRestrictUnauthenticated(area))
            }
        })
        .collect()
}

/// The two registration-mode toggles [`registration_modes`] reads, in
/// [`ExposureSettings`] field order; factored out because a pair of
/// same-typed toggles reads better by name than as a tuple.
type RegistrationModes = (Toggle, Toggle);

/// Reads the two settings that decide what a finished registration means:
/// whether the new account waits for staff approval (`approval_required` on
/// the instance endpoints) and whether it has to confirm its email address
/// (`account_activation_required` and `accountActivationRequired`). Grouped
/// because each is a plain default-or-parse and [`Config::from_lookup`] has
/// a size budget (AGENTS.md); the tuple comes back in field order. Both are
/// enforced by [`create_local_user`](crate::models::user::register::create_local_user)
/// and reported by the account-status gate of [`crate::api::oauth`].
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when either variable holds a
/// value that is neither spelling.
fn registration_modes<F>(lookup: &F) -> Result<RegistrationModes, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let approvals = boolean(
        lookup,
        REGISTRATIONS_APPROVAL_REQUIRED_VAR,
        DEFAULT_REGISTRATIONS_APPROVAL_REQUIRED,
    )?;
    let activation = boolean(
        lookup,
        ACCOUNT_ACTIVATION_REQUIRED_VAR,
        DEFAULT_ACCOUNT_ACTIVATION_REQUIRED,
    )?;
    Ok((approvals.into(), activation.into()))
}

/// Reads `OAUTH_PROVIDERS`: the JSON text of the external OAuth login
/// provider array, [`DEFAULT_OAUTH_PROVIDERS`] when the variable is unset
/// or blank.
///
/// # Errors
///
/// [`ConfigError::InvalidOauthProviders`] when the text is not a JSON array
/// of provider objects; the parser's own wording rides along so an operator
/// can see which key broke.
fn oauth_providers<F>(lookup: &F) -> Result<Vec<OauthProvider>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let configured = lookup(OAUTH_PROVIDERS_VAR).unwrap_or_default();
    let text = if configured.trim().is_empty() {
        DEFAULT_OAUTH_PROVIDERS
    } else {
        configured.as_str()
    };
    serde_json::from_str(text).map_err(|err| ConfigError::InvalidOauthProviders(err.to_string()))
}

/// Reads the five settings that configure the media proxy
/// (see [`crate::services::media_proxy`]). Grouped because each is a
/// plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); enabling the proxy without a long-enough secret is an
/// error, since every proxy link would be unsignable (and the route
/// unservable) without it.
fn media_proxy_settings<F>(lookup: &F) -> Result<MediaProxyConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let enabled = boolean(lookup, MEDIA_PROXY_ENABLED_VAR, DEFAULT_MEDIA_PROXY_ENABLED)?;
    let secret = lookup(MEDIA_PROXY_SECRET_VAR)
        .map(|secret| secret.trim().to_owned())
        .filter(|secret| !secret.is_empty());
    if enabled {
        let secret = secret
            .as_deref()
            .ok_or(ConfigError::Missing(MEDIA_PROXY_SECRET_VAR))?;
        if secret.chars().count() < MIN_MEDIA_PROXY_SECRET_CHARS {
            return Err(ConfigError::ShortMediaProxySecret(MEDIA_PROXY_SECRET_VAR));
        }
    }
    let base_url = lookup(MEDIA_PROXY_BASE_URL_VAR)
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty());
    let max_bytes = number(
        lookup,
        MEDIA_PROXY_MAX_BYTES_VAR,
        DEFAULT_MEDIA_PROXY_MAX_BYTES,
    )?;
    let blacklist = comma_list(&lookup(MEDIA_PROXY_BLACKLIST_VAR).unwrap_or_default());
    Ok(MediaProxyConfig {
        enabled,
        secret,
        base_url,
        max_bytes,
        blacklist,
    })
}

/// Reads `RICH_MEDIA_IGNORE_HOSTS`: the comma-separated list of hosts rich
/// media card fetching never visits, empty when unset or blank — the same
/// trimmed-list reading as the media proxy's blacklist.
fn rich_media_ignore_hosts<F>(lookup: &F) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    comma_list(
        &lookup(RICH_MEDIA_IGNORE_HOSTS_VAR)
            .unwrap_or_else(|| DEFAULT_RICH_MEDIA_IGNORE_HOSTS.to_owned()),
    )
}

/// Reads `RICH_MEDIA_REMOTE`: whether preview cards are fetched for the
/// remote URLs statuses link to, off by default.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when the variable holds a value
/// that is neither spelling.
fn rich_media_remote<F>(lookup: &F) -> Result<Toggle, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(boolean(lookup, RICH_MEDIA_REMOTE_VAR, DEFAULT_RICH_MEDIA_REMOTE)?.into())
}

/// Reads the two VAPID overrides, each trimmed, a blank standing for
/// unset (the generated pair then stays in charge).
fn vapid_settings<F>(lookup: &F) -> VapidConfig
where
    F: Fn(&str) -> Option<String>,
{
    VapidConfig {
        private_key: lookup(VAPID_PRIVATE_KEY_VAR)
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty()),
        public_key: lookup(VAPID_PUBLIC_KEY_VAR)
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty()),
    }
}

/// Reads the three settings that configure status translation
/// (see [`crate::services::translate`]). Grouped because each is a plain
/// default-or-parse and [`Config::from_lookup`] has a size budget (AGENTS.md).
/// An unset or blank provider is [`TranslationProvider::None`], the default;
/// a name that is not one of the three is a startup error, because it would
/// silently leave the instance translating nothing.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidTranslationProvider`] when
/// `TRANSLATION_PROVIDER` names no provider this instance speaks.
fn translation_settings<F>(lookup: &F) -> Result<TranslationConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup(TRANSLATION_PROVIDER_VAR).unwrap_or_default();
    let text = if raw.trim().is_empty() {
        "none".to_owned()
    } else {
        raw
    };
    let provider = TranslationProvider::parse(&text).ok_or(
        ConfigError::InvalidTranslationProvider(TRANSLATION_PROVIDER_VAR),
    )?;
    let api_url = lookup(TRANSLATION_API_URL_VAR)
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty());
    let api_key = lookup(TRANSLATION_API_KEY_VAR)
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty());
    Ok(TranslationConfig {
        provider,
        api_url,
        api_key,
    })
}

/// Reads the rate limiter settings (see [`crate::rate_limit`]): whether
/// limited routes throttle at all, the bucket table — Pleroma's defaults
/// with the `RATE_LIMITS` overrides merged in — and whether
/// `X-Forwarded-For` may name the client.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when a toggle variable holds a
/// value that is neither spelling, and [`ConfigError::InvalidRateLimits`]
/// when `RATE_LIMITS` holds a malformed entry.
fn rate_limit_settings<F>(lookup: &F) -> Result<RateLimitConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let enabled = boolean(lookup, RATE_LIMITS_ENABLED_VAR, DEFAULT_RATE_LIMITS_ENABLED)?.into();
    let trust_proxy_headers =
        boolean(lookup, TRUST_PROXY_HEADERS_VAR, DEFAULT_TRUST_PROXY_HEADERS)?.into();
    let mut limits = rate_limit::default_limits();
    if let Some(text) = lookup(RATE_LIMITS_VAR).filter(|text| !text.trim().is_empty()) {
        limits.extend(rate_limit::parse_limits(&text).map_err(ConfigError::InvalidRateLimits)?);
    }
    Ok(RateLimitConfig {
        enabled,
        limits,
        trust_proxy_headers,
    })
}

/// Reads a variable the instance cannot start without: its value, refusing
/// [`ConfigError::Missing`] when it is unset or empty. Both required settings
/// read through here, which is also what keeps [`Config::from_lookup`] inside
/// the size budget AGENTS.md gives it.
///
/// # Errors
///
/// Returns [`ConfigError::Missing`] naming `var`.
fn required_var<F>(lookup: &F, var: &'static str) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(var)
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing(var))
}

/// Reads a non-negative integer setting, applying `default` when the variable
/// is unset or empty.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] if the value is not a whole number.
fn number<F>(lookup: &F, var: &'static str, default: usize) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(var).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    raw.parse().ok().ok_or(ConfigError::InvalidNumber(var))
}

/// Reads a boolean setting, applying `default` when the variable is unset or
/// empty. Accepted spellings are `true`/`false` (case-insensitive) and `1`/`0`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] if the value is neither spelling.
fn boolean<F>(lookup: &F, var: &'static str, default: bool) -> Result<bool, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(var).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ConfigError::InvalidBoolean(var)),
    }
}

/// Splits a comma-separated list into trimmed, non-empty owned values.
fn comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Redacts `database_url`, which usually contains a password.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The whole field list does not fit one function inside Clippy's
        // line budget, so two segment helpers write it in declaration
        // order and `fmt` only opens, hands over and closes the document.
        let mut debug = f.debug_struct("Config");
        let debug = debug_identity_and_exposure(&mut debug, self);
        debug_delivery_and_federation(debug, self).finish()
    }
}

/// The `secret_key_base` rendered for `Debug`: the marker string in place
/// of the secret, `None` staying `None`. A function rather than the inline
/// closure the impl used to bind: the closure's text pushed the chain's
/// call past rustfmt's call width, splitting the field over lines the
/// [`Debug`] impl's line budget could not spend.
fn hide_secret(config: &Config) -> Option<&'static str> {
    config.secret_key_base.as_ref().map(|_| "<redacted>")
}

/// Writes the first segment of the [`Config`] `Debug` document: the
/// instance's identity, its content limits, storage locations, auth
/// requirements and everything it exposes to clients — every field from
/// `database_url` (redacted, it carries the password) to
/// `available_frontends`. The builder arrives borrowed and leaves borrowed,
/// so `fmt` can keep the chain going.
fn debug_identity_and_exposure<'a, 'b, 'd>(
    debug: &'d mut fmt::DebugStruct<'a, 'b>,
    config: &Config,
) -> &'d mut fmt::DebugStruct<'a, 'b> {
    debug
        .field("database_url", &"<redacted>")
        .field("bind_addr", &config.bind_addr)
        .field("base_url", &config.base_url)
        .field("frontend_base_url", &config.frontend_base_url)
        .field("instance_name", &config.instance_name)
        .field("instance_description", &config.instance_description)
        .field("contact_email", &config.contact_email)
        .field("instance_languages", &config.instance_languages)
        .field("max_status_chars", &config.max_status_chars)
        .field("max_media_attachments", &config.max_media_attachments)
        .field("max_poll_options", &config.max_poll_options)
        .field("max_poll_option_chars", &config.max_poll_option_chars)
        .field("min_poll_expiration_secs", &config.min_poll_expiration_secs)
        .field("max_poll_expiration_secs", &config.max_poll_expiration_secs)
        .field("max_chat_message_chars", &config.max_chat_message_chars)
        .field("max_media_description", &config.max_media_description)
        .field("max_display_name_chars", &config.max_display_name_chars)
        .field("max_bio_chars", &config.max_bio_chars)
        .field("max_profile_fields", &config.max_profile_fields)
        .field("max_report_comment", &config.max_report_comment)
        .field("max_image_pixels", &config.max_image_pixels)
        .field("max_pinned_statuses", &config.max_pinned_statuses)
        .field("min_expiration_secs", &config.min_expiration_secs)
        .field("min_scheduled_secs", &config.min_scheduled_secs)
        .field("max_scheduled_per_user", &config.max_scheduled_per_user)
        .field(
            "max_scheduled_per_user_per_day",
            &config.max_scheduled_per_user_per_day,
        )
        .field("upload_limits", &config.upload_limits)
        .field("avatar_limit_bytes", &config.avatar_limit_bytes)
        .field("banner_limit_bytes", &config.banner_limit_bytes)
        .field("inbox_max_bytes", &config.inbox_max_bytes)
        .field(
            "max_remote_mention_lookups",
            &config.max_remote_mention_lookups,
        )
        .field("uploads_dir", &config.uploads_dir)
        .field("emoji_dir", &config.emoji_dir)
        .field("media_base_url", &config.media_base_url)
        .field("accounts_email_optional", &config.accounts_email_optional)
        .field("min_password_length", &config.min_password_length)
        .field("oauth_token_ttl_secs", &config.oauth_token_ttl_secs)
        .field("password_reset_ttl_secs", &config.password_reset_ttl_secs)
        .field(
            "account_activation_required",
            &config.account_activation_required,
        )
        .field("allow_account_deletion", &config.allow_account_deletion)
        .field("mongooseim_enabled", &config.mongooseim_enabled)
        .field("captcha_enabled", &config.captcha_enabled)
        .field("healthcheck_enabled", &config.healthcheck_enabled)
        .field("registrations_open", &config.registrations_open)
        .field("invites_enabled", &config.invites_enabled)
        .field(
            "registrations_approval_required",
            &config.registrations_approval_required,
        )
        .field(
            "search_statuses_unauthenticated",
            &config.search_statuses_unauthenticated,
        )
        .field("restrict_unauthenticated", &config.restrict_unauthenticated)
        .field("oauth_providers", &config.oauth_providers)
        .field("available_frontends", &config.available_frontends)
}

/// Writes the second segment of the [`Config`] `Debug` document: the job
/// worker and its sweeps — the deleted-status purge age and the backup
/// trio — the streaming heartbeat, the mailer (whose `smtp_password` and
/// [`hide_secret`]'s `secret_key_base` are the impl's redactions beside
/// `database_url`), the media, translation and fetch services, and the
/// federation settings with the unreachable-host grace period, the MRF
/// chain and the rate limiter. The builder arrives borrowed and leaves
/// borrowed, so `fmt` can close the document it opened.
fn debug_delivery_and_federation<'a, 'b, 'd>(
    debug: &'d mut fmt::DebugStruct<'a, 'b>,
    config: &Config,
) -> &'d mut fmt::DebugStruct<'a, 'b> {
    debug
        .field("job_concurrency", &config.job_concurrency)
        .field("job_queues", &config.job_queues)
        .field(
            "purge_deleted_statuses_days",
            &config.purge_deleted_statuses_days,
        )
        .field("backup_validity_days", &config.backup_validity_days)
        .field("backup_limit_days", &config.backup_limit_days)
        .field("backup_purge", &config.backup_purge)
        .field(
            "streaming_ping_interval_secs",
            &config.streaming_ping_interval_secs,
        )
        .field("smtp_host", &config.smtp_host)
        .field("smtp_port", &config.smtp_port)
        .field("smtp_username", &config.smtp_username)
        .field("smtp_password", &"<redacted>")
        .field("smtp_tls", &config.smtp_tls)
        .field("mail_from", &config.mail_from)
        .field("secret_key_base", &hide_secret(config))
        .field("media_proxy", &config.media_proxy)
        .field("rich_media_ignore_hosts", &config.rich_media_ignore_hosts)
        .field("rich_media_remote", &config.rich_media_remote)
        .field("vapid", &config.vapid)
        .field("translation", &config.translation)
        .field("fetch", &config.fetch)
        .field("federation_enabled", &config.federation_enabled)
        .field(
            "accept_unsolicited_public",
            &config.accept_unsolicited_public,
        )
        .field("unreachable_grace_secs", &config.unreachable_grace_secs)
        .field("mrf", &config.mrf)
        .field("rate_limit", &config.rate_limit)
}

/// A configuration value is missing or invalid.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A required environment variable is unset or empty.
    Missing(&'static str),
    /// `BIND_ADDR` is not a valid `ip:port`.
    InvalidBindAddr(AddrParseError),
    /// `BASE_URL` is not an http(s) origin without path or query.
    InvalidBaseUrl,
    /// A numeric setting is not a whole number; the value is the variable name.
    InvalidNumber(&'static str),
    /// A boolean setting is not `true`/`false` (or `1`/`0`); the value is the
    /// variable name.
    InvalidBoolean(&'static str),
    /// `OAUTH_PROVIDERS` is not the JSON text of an array of provider
    /// objects; the value is what the JSON parser said.
    InvalidOauthProviders(String),
    /// `SMTP_TLS` is not one of `starttls`, `tls` or `none`; the value is the
    /// variable name.
    InvalidSmtpTls(&'static str),
    /// `TRANSLATION_PROVIDER` is not one of `none`, `libretranslate` or
    /// `deepl`; the value is the variable name.
    InvalidTranslationProvider(&'static str),
    /// The media proxy is enabled but its `MEDIA_PROXY_SECRET` is shorter
    /// than [`MIN_MEDIA_PROXY_SECRET_CHARS`] requires; the value is the
    /// variable name.
    ShortMediaProxySecret(&'static str),
    /// `RATE_LIMITS` holds an entry that is not a `bucket=max:window_secs`
    /// triple (or a degenerate one); the value is the parser's reason.
    InvalidRateLimits(String),
    /// `RESTRICT_UNAUTHENTICATED` names an area outside
    /// [`RESTRICT_UNAUTHENTICATED_AREAS`]; the value is the name it read.
    InvalidRestrictUnauthenticated(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(var) => write!(f, "required environment variable {var} is not set"),
            Self::InvalidBindAddr(err) => write!(f, "invalid {BIND_ADDR_VAR}: {err}"),
            Self::InvalidBaseUrl => write!(
                f,
                "invalid {BASE_URL_VAR}: expected an http(s) origin such as \
                 https://social.example with no path or query"
            ),
            Self::InvalidNumber(var) => write!(f, "invalid {var}: expected a whole number"),
            Self::InvalidBoolean(var) => write!(f, "invalid {var}: expected true or false"),
            Self::InvalidOauthProviders(detail) => write!(
                f,
                "invalid {OAUTH_PROVIDERS_VAR}: expected a JSON array of provider objects: {detail}"
            ),
            Self::InvalidSmtpTls(var) => {
                write!(f, "invalid {var}: expected one of starttls, tls or none")
            }
            Self::InvalidTranslationProvider(var) => write!(
                f,
                "invalid {var}: expected one of none, libretranslate or deepl"
            ),
            Self::ShortMediaProxySecret(var) => write!(
                f,
                "invalid {var}: expected at least {MIN_MEDIA_PROXY_SECRET_CHARS} characters"
            ),
            Self::InvalidRateLimits(detail) => {
                write!(f, "invalid {RATE_LIMITS_VAR}: {detail}")
            }
            Self::InvalidRestrictUnauthenticated(area) => write!(
                f,
                "invalid {RESTRICT_UNAUTHENTICATED_VAR}: `{area}` is not one of {}",
                RESTRICT_UNAUTHENTICATED_AREAS.join(", ")
            ),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidBindAddr(err) => Some(err),
            Self::Missing(_)
            | Self::InvalidBaseUrl
            | Self::InvalidNumber(_)
            | Self::InvalidBoolean(_)
            | Self::InvalidOauthProviders(_)
            | Self::InvalidSmtpTls(_)
            | Self::InvalidTranslationProvider(_)
            | Self::ShortMediaProxySecret(_)
            | Self::InvalidRateLimits(_)
            | Self::InvalidRestrictUnauthenticated(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const URL: &str = "postgres://user:secret@localhost/wired";
    const ORIGIN: &str = "https://social.example";

    fn lookup_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// Reads a [`Config`] with the two required variables set, plus `extra`
    /// (which may override them).
    fn try_config_with(extra: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut vars: Vec<(String, String)> = vec![
            (DATABASE_URL_VAR.to_owned(), URL.to_owned()),
            (BASE_URL_VAR.to_owned(), ORIGIN.to_owned()),
        ];
        for &(key, value) in extra {
            match vars.iter_mut().find(|(k, _)| k == key) {
                Some(existing) => existing.1 = value.to_owned(),
                None => vars.push((key.to_owned(), value.to_owned())),
            }
        }
        Config::from_lookup(lookup_from(
            &vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
        ))
    }

    /// Like [`try_config_with`], panicking on error.
    fn config_with(extra: &[(&str, &str)]) -> Config {
        try_config_with(extra).unwrap()
    }

    /// The result of reading a config whose `BASE_URL` is `value`.
    fn base_url_result(value: &str) -> Result<Config, ConfigError> {
        Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, value),
        ]))
    }

    #[test]
    fn reads_database_url_and_defaults_bind_addr() {
        let config = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, ORIGIN),
        ]))
        .unwrap();
        assert_eq!(config.database_url, URL, "database_url");
        assert_eq!(
            config.bind_addr,
            DEFAULT_BIND_ADDR.parse().unwrap(),
            "default bind_addr"
        );
    }

    #[test]
    fn reads_custom_bind_addr() {
        let config = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, ORIGIN),
            (BIND_ADDR_VAR, "0.0.0.0:8080"),
        ]))
        .unwrap();
        assert_eq!(
            config.bind_addr,
            "0.0.0.0:8080".parse().unwrap(),
            "custom bind_addr"
        );
    }

    #[test]
    fn missing_database_url_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[])).unwrap_err();
        assert_eq!(err, ConfigError::Missing(DATABASE_URL_VAR), "error variant");
        assert!(err.source().is_none(), "missing var has no source");
        assert_eq!(
            err.to_string(),
            "required environment variable DATABASE_URL is not set",
            "message"
        );
    }

    #[test]
    fn empty_database_url_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[(DATABASE_URL_VAR, "")])).unwrap_err();
        assert_eq!(err, ConfigError::Missing(DATABASE_URL_VAR), "error variant");
    }

    #[test]
    fn invalid_bind_addr_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, ORIGIN),
            (BIND_ADDR_VAR, "not-an-addr"),
        ]))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidBindAddr(_)),
            "error variant"
        );
        assert!(err.source().is_some(), "parse error is the source");
        assert!(
            err.to_string().starts_with("invalid BIND_ADDR: "),
            "message"
        );
    }

    #[test]
    fn instance_settings_default() {
        let config = config_with(&[]);
        assert_eq!(config.base_url, ORIGIN, "base_url");
        assert_eq!(config.instance_name, "Wired", "default instance_name");
        assert_eq!(
            config.instance_description, "A Pleroma-compatible instance",
            "default instance_description"
        );
        assert_eq!(config.contact_email, None, "default contact_email");
        assert_eq!(
            config.instance_languages,
            vec!["en".to_owned()],
            "default instance_languages"
        );
        assert_eq!(config.max_status_chars, 5000, "default max_status_chars");
        assert_eq!(
            config.max_media_attachments, 4,
            "default max_media_attachments"
        );
        assert_eq!(config.max_poll_options, 20, "default max_poll_options");
        assert_eq!(
            config.max_chat_message_chars, 5_000,
            "default max_chat_message_chars"
        );
        assert_eq!(
            config.max_media_description, 1_500,
            "default max_media_description"
        );
        assert_eq!(
            config.upload_limits.max_file_bytes, 16_000_000,
            "default UPLOAD_LIMIT_BYTES"
        );
        assert_eq!(
            config.upload_limits.max_files, 16,
            "default UPLOAD_MAX_FILES"
        );
        assert!(
            !config.fetch.http_allow_private_hosts,
            "the SSRF guard is on by default"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from("./uploads"),
            "default UPLOADS_DIR"
        );
        assert_eq!(
            config.emoji_dir,
            PathBuf::from("./emoji"),
            "default EMOJI_DIR"
        );
        assert_eq!(
            config.media_base_url, "https://social.example/media",
            "MEDIA_BASE_URL defaults to BASE_URL plus /media"
        );
        assert!(
            !config.accounts_email_optional,
            "an email address is required at registration by default"
        );
        assert_eq!(config.min_password_length, 8, "default MIN_PASSWORD_LENGTH");
        assert_eq!(
            config.oauth_token_ttl_secs, 2_592_000,
            "default OAUTH_TOKEN_TTL_SECS is 30 days"
        );
        assert!(
            !config.mongooseim_enabled.get(),
            "the `MongooseIM` auth endpoints are off by default"
        );
        assert!(
            !config.captcha_enabled.get(),
            "no captcha provider is configured by default"
        );
        assert!(
            !config.healthcheck_enabled.get(),
            "the healthcheck is off by default"
        );
        assert!(
            config.registrations_open.get(),
            "registration is open by default (Pleroma's default)"
        );
        assert_eq!(config.domain(), "social.example", "domain without port");
        assert!(config.is_https(), "https base_url");
    }

    /// The unauthenticated status-search toggle: off by default, the
    /// environment turning it on, blank falling back, and junk refused.
    #[test]
    fn search_statuses_unauthenticated_defaults_off_and_reads_its_variable() {
        assert!(
            !config_with(&[]).search_statuses_unauthenticated.get(),
            "anonymous requests search no statuses by default"
        );
        for value in ["true", "1", " TRUE "] {
            let config = config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, value)]);
            assert!(
                config.search_statuses_unauthenticated.get(),
                "`SEARCH_STATUSES_UNAUTHENTICATED={value:?}` turns it on"
            );
        }
        for value in ["false", "0"] {
            let config = config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, value)]);
            assert!(
                !config.search_statuses_unauthenticated.get(),
                "`SEARCH_STATUSES_UNAUTHENTICATED={value:?}` keeps it off"
            );
        }
        assert!(
            !config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, "")])
                .search_statuses_unauthenticated
                .get(),
            "an empty value applies the default"
        );
        let err = try_config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(SEARCH_STATUSES_UNAUTHENTICATED_VAR),
            "a value that is neither spelling is refused"
        );
        let debug = format!("{:?}", config_with(&[]));
        assert!(
            debug.contains("search_statuses_unauthenticated: Toggle(false)"),
            "the toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_instance_settings() {
        let config = config_with(&[
            (INSTANCE_NAME_VAR, "Snatter"),
            (INSTANCE_DESCRIPTION_VAR, "Where we all chatter"),
            (CONTACT_EMAIL_VAR, "admin@example.org"),
            (INSTANCE_LANGUAGES_VAR, "en, de ,fr"),
            (MAX_STATUS_CHARS_VAR, "300"),
            (MAX_MEDIA_ATTACHMENTS_VAR, "6"),
            (MAX_POLL_OPTIONS_VAR, "12"),
            (UPLOAD_LIMIT_BYTES_VAR, "1048576"),
            (UPLOAD_MAX_FILES_VAR, "4"),
            (HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true"),
            (UPLOADS_DIR_VAR, "/srv/wired/uploads"),
            (EMOJI_DIR_VAR, "/srv/wired/emoji"),
            (MEDIA_BASE_URL_VAR, "https://cdn.example/media"),
            (ACCOUNTS_EMAIL_OPTIONAL_VAR, "true"),
            (MIN_PASSWORD_LENGTH_VAR, "12"),
            (OAUTH_TOKEN_TTL_SECS_VAR, "3600"),
            (MONGOOSEIM_ENABLED_VAR, "true"),
            (CAPTCHA_ENABLED_VAR, "true"),
            (HEALTHCHECK_ENABLED_VAR, "true"),
        ]);
        assert_eq!(config.instance_name, "Snatter", "custom instance_name");
        assert_eq!(
            config.instance_description, "Where we all chatter",
            "custom instance_description"
        );
        assert_eq!(
            config.contact_email.as_deref(),
            Some("admin@example.org"),
            "custom contact_email"
        );
        assert_eq!(
            config.instance_languages,
            vec!["en".to_owned(), "de".to_owned(), "fr".to_owned()],
            "comma list is trimmed"
        );
        assert_eq!(config.max_status_chars, 300, "custom max_status_chars");
        assert_eq!(
            config.max_media_attachments, 6,
            "custom max_media_attachments"
        );
        assert_eq!(config.max_poll_options, 12, "custom max_poll_options");
        assert_eq!(
            config.upload_limits.max_file_bytes, 1_048_576,
            "custom UPLOAD_LIMIT_BYTES"
        );
        assert_eq!(config.upload_limits.max_files, 4, "custom UPLOAD_MAX_FILES");
        assert!(
            config.fetch.http_allow_private_hosts,
            "custom HTTP_ALLOW_PRIVATE_HOSTS"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from("/srv/wired/uploads"),
            "custom UPLOADS_DIR"
        );
        assert_eq!(
            config.emoji_dir,
            PathBuf::from("/srv/wired/emoji"),
            "custom EMOJI_DIR"
        );
        assert_eq!(
            config.media_base_url, "https://cdn.example/media",
            "custom MEDIA_BASE_URL"
        );
        assert!(
            config.accounts_email_optional,
            "custom ACCOUNTS_EMAIL_OPTIONAL"
        );
        assert_eq!(config.min_password_length, 12, "custom MIN_PASSWORD_LENGTH");
        assert_eq!(
            config.oauth_token_ttl_secs, 3_600,
            "custom OAUTH_TOKEN_TTL_SECS"
        );
        assert!(config.mongooseim_enabled.get(), "custom MONGOOSEIM_ENABLED");
        assert!(config.captcha_enabled.get(), "custom CAPTCHA_ENABLED");
        assert!(
            config.healthcheck_enabled.get(),
            "custom HEALTHCHECK_ENABLED"
        );
    }

    #[test]
    fn job_worker_settings_default() {
        let config = config_with(&[]);
        assert_eq!(config.job_concurrency, 10, "default job_concurrency");
        assert_eq!(
            config.job_queues,
            vec![
                "default".to_owned(),
                "federator_incoming".to_owned(),
                "federator_outgoing".to_owned(),
                "mailer".to_owned(),
                "scheduled".to_owned(),
                "background".to_owned(),
            ],
            "default job_queues"
        );
        assert_eq!(
            config.purge_deleted_statuses_days, 30,
            "default PURGE_DELETED_STATUSES_DAYS is thirty days"
        );
    }

    /// The deleted-status purge age: thirty days by default, the environment
    /// overriding it, `0` expressible (the handler's disable value), a blank
    /// falling back, junk refused, and `Debug` showing the value.
    #[test]
    fn purge_deleted_statuses_days_defaults_and_reads_its_variable() {
        let config = config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "7")]);
        assert_eq!(
            config.purge_deleted_statuses_days, 7,
            "custom PURGE_DELETED_STATUSES_DAYS"
        );
        let disabled = config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "0")]);
        assert_eq!(
            disabled.purge_deleted_statuses_days, 0,
            "zero is a legal value, and the purge handler reads it as disabled"
        );
        let blank = config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "")]);
        assert_eq!(
            blank.purge_deleted_statuses_days, 30,
            "an empty value applies the default"
        );
        let err = try_config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "many")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidNumber(PURGE_DELETED_STATUSES_DAYS_VAR),
            "a value that is not a whole number is refused"
        );
        assert!(err.source().is_none(), "no source");
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {PURGE_DELETED_STATUSES_DAYS_VAR}: ")),
            "message names the variable"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("purge_deleted_statuses_days: 7"),
            "the purge age is visible in Debug output: {debug}"
        );
    }

    /// The backup trio: thirty valid days, a seven-day interval and the
    /// purge sweep on by default, each environment variable overriding, a
    /// blank falling back, junk in the two day settings refused, and
    /// `Debug` showing all three.
    #[test]
    fn backup_settings_default_and_read_their_variables() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.backup_validity_days, 30,
            "default BACKUP_VALIDITY_DAYS is thirty days"
        );
        assert_eq!(
            defaults.backup_limit_days, 7,
            "default BACKUP_LIMIT_DAYS is seven days"
        );
        assert!(
            defaults.backup_purge.get(),
            "the backup purge sweep runs by default"
        );
        let config = config_with(&[
            (BACKUP_VALIDITY_DAYS_VAR, "10"),
            (BACKUP_LIMIT_DAYS_VAR, "2"),
            (BACKUP_PURGE_VAR, "false"),
        ]);
        assert_eq!(
            config.backup_validity_days, 10,
            "custom BACKUP_VALIDITY_DAYS"
        );
        assert_eq!(config.backup_limit_days, 2, "custom BACKUP_LIMIT_DAYS");
        assert!(
            !config.backup_purge.get(),
            "`BACKUP_PURGE=false` stops the sweep"
        );
        let blank = config_with(&[
            (BACKUP_VALIDITY_DAYS_VAR, ""),
            (BACKUP_LIMIT_DAYS_VAR, ""),
            (BACKUP_PURGE_VAR, ""),
        ]);
        assert_eq!(
            (
                blank.backup_validity_days,
                blank.backup_limit_days,
                blank.backup_purge.get(),
            ),
            (30, 7, true),
            "an empty value applies the default"
        );
        for (var, value) in [
            (BACKUP_VALIDITY_DAYS_VAR, "many"),
            (BACKUP_LIMIT_DAYS_VAR, "weekly"),
        ] {
            let err = try_config_with(&[(var, value)]).unwrap_err();
            assert_eq!(
                err,
                ConfigError::InvalidNumber(var),
                "`{var}={value:?}` is not a whole number"
            );
            assert!(err.source().is_none(), "no source");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names the variable"
            );
        }
        let err = try_config_with(&[(BACKUP_PURGE_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(BACKUP_PURGE_VAR),
            "a value that is neither boolean spelling is refused"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("backup_validity_days: 10"),
            "the archive validity window is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("backup_limit_days: 2"),
            "the per-user backup interval is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("backup_purge: Toggle(false)"),
            "the purge switch is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_job_worker_settings() {
        let config = config_with(&[
            (JOB_CONCURRENCY_VAR, "3"),
            (JOB_QUEUES_VAR, " default , mailer ,,scheduled"),
        ]);
        assert_eq!(config.job_concurrency, 3, "custom job_concurrency");
        assert_eq!(
            config.job_queues,
            vec![
                "default".to_owned(),
                "mailer".to_owned(),
                "scheduled".to_owned(),
            ],
            "comma list is trimmed, empty entries dropped"
        );
    }

    #[test]
    fn streaming_heartbeat_defaults_and_reads_its_variable() {
        assert_eq!(
            config_with(&[]).streaming_ping_interval_secs,
            DEFAULT_STREAMING_PING_INTERVAL_SECS,
            "default streaming heartbeat"
        );
        assert_eq!(
            config_with(&[(STREAMING_PING_INTERVAL_SECS_VAR, "5")]).streaming_ping_interval_secs,
            5,
            "custom streaming heartbeat"
        );
        assert_eq!(
            config_with(&[(STREAMING_PING_INTERVAL_SECS_VAR, "")]).streaming_ping_interval_secs,
            DEFAULT_STREAMING_PING_INTERVAL_SECS,
            "an empty value applies the default"
        );
    }

    #[test]
    fn mailer_settings_default_to_disabled_mail() {
        let config = config_with(&[]);
        assert_eq!(config.smtp_host, None, "mail is disabled by default");
        assert_eq!(config.smtp_port, 587, "default SMTP_PORT is submission");
        assert_eq!(config.smtp_username, None, "default SMTP_USERNAME");
        assert_eq!(config.smtp_password, None, "default SMTP_PASSWORD");
        assert_eq!(config.smtp_tls, SmtpTls::Starttls, "default SMTP_TLS");
        assert_eq!(
            config.mail_from, "noreply@social.example",
            "MAIL_FROM defaults to noreply at the instance domain"
        );
    }

    #[test]
    fn reads_custom_mailer_settings() {
        let config = config_with(&[
            (SMTP_HOST_VAR, " smtp.example.org "),
            (SMTP_PORT_VAR, "465"),
            (SMTP_USERNAME_VAR, " relay-user "),
            (SMTP_PASSWORD_VAR, "relay-secret"),
            (SMTP_TLS_VAR, "tls"),
            (MAIL_FROM_VAR, " hello@example.org "),
        ]);
        assert_eq!(
            config.smtp_host.as_deref(),
            Some("smtp.example.org"),
            "custom SMTP_HOST, trimmed"
        );
        assert_eq!(config.smtp_port, 465, "custom SMTP_PORT");
        assert_eq!(
            config.smtp_username.as_deref(),
            Some("relay-user"),
            "custom SMTP_USERNAME, trimmed"
        );
        assert_eq!(
            config.smtp_password.as_deref(),
            Some("relay-secret"),
            "custom SMTP_PASSWORD, kept verbatim"
        );
        assert_eq!(config.smtp_tls, SmtpTls::Tls, "custom SMTP_TLS");
        assert_eq!(
            config.mail_from, "hello@example.org",
            "custom MAIL_FROM, trimmed"
        );
    }

    #[test]
    fn empty_mailer_variables_read_as_unset() {
        let config = config_with(&[
            (SMTP_HOST_VAR, "  "),
            (SMTP_USERNAME_VAR, ""),
            (SMTP_PASSWORD_VAR, ""),
            (MAIL_FROM_VAR, " "),
        ]);
        assert_eq!(config.smtp_host, None, "blank SMTP_HOST leaves mail off");
        assert_eq!(config.smtp_username, None, "blank SMTP_USERNAME");
        assert_eq!(config.smtp_password, None, "blank SMTP_PASSWORD");
        assert_eq!(
            config.mail_from, "noreply@social.example",
            "blank MAIL_FROM falls back to the default"
        );
    }

    #[test]
    fn smtp_tls_parses_its_spellings() {
        for (value, want) in [
            ("starttls", SmtpTls::Starttls),
            (" STARTTLS ", SmtpTls::Starttls),
            ("tls", SmtpTls::Tls),
            ("TLS", SmtpTls::Tls),
            ("none", SmtpTls::None),
            ("None", SmtpTls::None),
        ] {
            let config = config_with(&[(SMTP_TLS_VAR, value)]);
            assert_eq!(config.smtp_tls, want, "SMTP_TLS={value:?}");
        }
        for value in ["", "  "] {
            let config = config_with(&[(SMTP_TLS_VAR, value)]);
            assert_eq!(
                config.smtp_tls,
                SmtpTls::Starttls,
                "SMTP_TLS={value:?} falls back to the default"
            );
        }
    }

    #[test]
    fn invalid_smtp_tls_is_an_error() {
        let err = try_config_with(&[(SMTP_TLS_VAR, "opportunistic")]).unwrap_err();
        assert_eq!(err, ConfigError::InvalidSmtpTls(SMTP_TLS_VAR), "variant");
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid SMTP_TLS: expected one of starttls, tls or none",
            "message names the variable and its vocabulary"
        );
    }

    #[test]
    fn smtp_port_must_be_a_port() {
        for value in ["many", "0.5", "65536"] {
            let err = try_config_with(&[(SMTP_PORT_VAR, value)]).unwrap_err();
            assert_eq!(
                err,
                ConfigError::InvalidNumber(SMTP_PORT_VAR),
                "SMTP_PORT={value:?} is refused"
            );
        }
    }

    #[test]
    fn mail_from_default_drops_the_base_url_port() {
        let config = config_with(&[(BASE_URL_VAR, "http://localhost:4000")]);
        assert_eq!(
            config.mail_from, "noreply@localhost",
            "a port is not part of an email domain"
        );
    }

    #[test]
    fn debug_redacts_the_smtp_password() {
        let config = config_with(&[
            (SMTP_HOST_VAR, "smtp.example.org"),
            (SMTP_USERNAME_VAR, "relay-user"),
            (SMTP_PASSWORD_VAR, "relay-secret"),
            (SMTP_TLS_VAR, "tls"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("relay-secret"),
            "the SMTP password must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"smtp_password: "<redacted>""#),
            "the marker names the field: {debug}"
        );
        assert!(
            debug.contains(r#"smtp_host: Some("smtp.example.org")"#),
            "the relay host shows: {debug}"
        );
        assert!(
            debug.contains(r#"smtp_username: Some("relay-user")"#),
            "the login name shows: {debug}"
        );
        assert!(
            debug.contains("smtp_tls: Tls"),
            "the encryption mode shows: {debug}"
        );
    }

    #[test]
    fn secret_key_base_defaults_to_unset() {
        let config = config_with(&[]);
        assert_eq!(
            config.secret_key_base, None,
            "no signing secret unless configured"
        );
    }

    #[test]
    fn reads_custom_secret_key_base() {
        let config = config_with(&[(SECRET_KEY_BASE_VAR, "  a-signing-secret  ")]);
        assert_eq!(
            config.secret_key_base.as_deref(),
            Some("a-signing-secret"),
            "the secret is trimmed"
        );
    }

    #[test]
    fn blank_secret_key_base_reads_as_unset() {
        let config = config_with(&[(SECRET_KEY_BASE_VAR, "   ")]);
        assert_eq!(config.secret_key_base, None, "a blank secret is no secret");
    }

    #[test]
    fn debug_redacts_the_secret_key_base() {
        let config = config_with(&[(SECRET_KEY_BASE_VAR, "a-signing-secret")]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-signing-secret"),
            "the signing secret must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"secret_key_base: Some("<redacted>")"#),
            "the marker names the field: {debug}"
        );
    }

    #[test]
    fn reads_custom_max_chat_message_chars() {
        let config = config_with(&[(MAX_CHAT_MESSAGE_CHARS_VAR, "140")]);
        assert_eq!(
            config.max_chat_message_chars, 140,
            "custom MAX_CHAT_MESSAGE_CHARS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_chat_message_chars: 140"),
            "the chat length is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_max_media_description() {
        let config = config_with(&[(MAX_MEDIA_DESCRIPTION_VAR, "100")]);
        assert_eq!(
            config.max_media_description, 100,
            "custom MAX_MEDIA_DESCRIPTION"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_media_description: 100"),
            "the description limit is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_profile_length_limits() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_display_name_chars, 50,
            "default MAX_DISPLAY_NAME_CHARS"
        );
        assert_eq!(defaults.max_bio_chars, 5_000, "default MAX_BIO_CHARS");

        let config = config_with(&[
            (MAX_DISPLAY_NAME_CHARS_VAR, "30"),
            (MAX_BIO_CHARS_VAR, "200"),
        ]);
        assert_eq!(
            config.max_display_name_chars, 30,
            "custom MAX_DISPLAY_NAME_CHARS"
        );
        assert_eq!(config.max_bio_chars, 200, "custom MAX_BIO_CHARS");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_display_name_chars: 30"),
            "the display-name limit is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_bio_chars: 200"),
            "the bio limit is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_max_profile_fields() {
        let defaults = config_with(&[]);
        assert_eq!(defaults.max_profile_fields, 4, "default MAX_PROFILE_FIELDS");

        let config = config_with(&[(MAX_PROFILE_FIELDS_VAR, "2")]);
        assert_eq!(config.max_profile_fields, 2, "custom MAX_PROFILE_FIELDS");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_profile_fields: 2"),
            "the field-count limit is visible in Debug output: {debug}"
        );
    }

    /// The report comment cap: Mastodon's 1000 by default, the environment
    /// overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_max_report_comment() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_report_comment, 1_000,
            "default MAX_REPORT_COMMENT"
        );

        let config = config_with(&[(MAX_REPORT_COMMENT_VAR, "140")]);
        assert_eq!(config.max_report_comment, 140, "custom MAX_REPORT_COMMENT");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_report_comment: 140"),
            "the report comment limit is visible in Debug output: {debug}"
        );
    }

    /// The pinned-status count: Pleroma's one by default, the environment
    /// overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_max_pinned_statuses() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_pinned_statuses, 1,
            "default MAX_PINNED_STATUSES"
        );

        let config = config_with(&[(MAX_PINNED_STATUSES_VAR, "5")]);
        assert_eq!(config.max_pinned_statuses, 5, "custom MAX_PINNED_STATUSES");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_pinned_statuses: 5"),
            "the pinned-status limit is visible in Debug output: {debug}"
        );
    }

    /// The shortest status expiration: Pleroma's 600 seconds by default,
    /// the environment overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_min_expiration_secs() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.min_expiration_secs, 600,
            "default MIN_EXPIRATION_SECS"
        );

        let config = config_with(&[(MIN_EXPIRATION_SECS_VAR, "120")]);
        assert_eq!(
            config.min_expiration_secs, 120,
            "custom MIN_EXPIRATION_SECS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("min_expiration_secs: 120"),
            "the expiration floor is visible in Debug output: {debug}"
        );
    }

    /// The poll option length cap: Pleroma's 200 characters by default, the
    /// environment overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_max_poll_option_chars() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_poll_option_chars, 200,
            "default MAX_POLL_OPTION_CHARS"
        );

        let config = config_with(&[(MAX_POLL_OPTION_CHARS_VAR, "50")]);
        assert_eq!(
            config.max_poll_option_chars, 50,
            "custom MAX_POLL_OPTION_CHARS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_poll_option_chars: 50"),
            "the poll option length cap is visible in Debug output: {debug}"
        );
    }

    /// The two poll expiration bounds: the 300-second floor and the
    /// 31_536_000-second (one-year) ceiling by default, the environment
    /// overriding each, and `Debug` showing both.
    #[test]
    fn reads_custom_poll_expiration_bounds() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.min_poll_expiration_secs, 300,
            "default MIN_POLL_EXPIRATION"
        );
        assert_eq!(
            defaults.max_poll_expiration_secs, 31_536_000,
            "default MAX_POLL_EXPIRATION"
        );

        let config = config_with(&[
            (MIN_POLL_EXPIRATION_VAR, "60"),
            (MAX_POLL_EXPIRATION_VAR, "86400"),
        ]);
        assert_eq!(
            config.min_poll_expiration_secs, 60,
            "custom MIN_POLL_EXPIRATION"
        );
        assert_eq!(
            config.max_poll_expiration_secs, 86_400,
            "custom MAX_POLL_EXPIRATION"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("min_poll_expiration_secs: 60"),
            "the poll floor is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_poll_expiration_secs: 86400"),
            "the poll ceiling is visible in Debug output: {debug}"
        );
    }

    /// The scheduled-statuses window and the two per-user limits: Pleroma's
    /// 300 seconds, 300 rows and 25 rows per day by default, the environment
    /// overriding each.
    #[test]
    fn scheduled_status_limits_default_and_read_their_variables() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.min_scheduled_secs, 300,
            "default MIN_SCHEDULED_SECS"
        );
        assert_eq!(
            defaults.max_scheduled_per_user, 300,
            "default MAX_SCHEDULED_PER_USER"
        );
        assert_eq!(
            defaults.max_scheduled_per_user_per_day, 25,
            "default MAX_SCHEDULED_PER_USER_PER_DAY"
        );

        let config = config_with(&[
            (MIN_SCHEDULED_SECS_VAR, "60"),
            (MAX_SCHEDULED_PER_USER_VAR, "10"),
            (MAX_SCHEDULED_PER_USER_PER_DAY_VAR, "5"),
        ]);
        assert_eq!(config.min_scheduled_secs, 60, "custom MIN_SCHEDULED_SECS");
        assert_eq!(
            config.max_scheduled_per_user, 10,
            "custom MAX_SCHEDULED_PER_USER"
        );
        assert_eq!(
            config.max_scheduled_per_user_per_day, 5,
            "custom MAX_SCHEDULED_PER_USER_PER_DAY"
        );
    }

    /// The three scheduled-status settings are visible in `Debug` output:
    /// they name limits, not secrets.
    #[test]
    fn debug_shows_the_scheduled_status_limits() {
        let config = config_with(&[
            (MIN_SCHEDULED_SECS_VAR, "60"),
            (MAX_SCHEDULED_PER_USER_VAR, "10"),
            (MAX_SCHEDULED_PER_USER_PER_DAY_VAR, "5"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("min_scheduled_secs: 60"),
            "the scheduled-statuses window is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_scheduled_per_user: 10"),
            "the per-user pending cap is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_scheduled_per_user_per_day: 5"),
            "the per-user per-day cap is visible in Debug output: {debug}"
        );
    }

    /// The two profile-image byte caps: Pleroma's avatar/banner defaults,
    /// the environment overriding each, and `Debug` showing both.
    #[test]
    fn reads_custom_profile_image_limits() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.avatar_limit_bytes, 2_000_000,
            "default AVATAR_LIMIT_BYTES"
        );
        assert_eq!(
            defaults.banner_limit_bytes, 4_000_000,
            "default BANNER_LIMIT_BYTES"
        );

        let config = config_with(&[
            (AVATAR_LIMIT_BYTES_VAR, "1024"),
            (BANNER_LIMIT_BYTES_VAR, "2048"),
        ]);
        assert_eq!(
            config.avatar_limit_bytes, 1_024,
            "custom AVATAR_LIMIT_BYTES"
        );
        assert_eq!(
            config.banner_limit_bytes, 2_048,
            "custom BANNER_LIMIT_BYTES"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("avatar_limit_bytes: 1024"),
            "the avatar cap is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("banner_limit_bytes: 2048"),
            "the banner cap is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn max_image_pixels_defaults_to_forty_megapixels() {
        let config = config_with(&[]);
        assert_eq!(
            config.max_image_pixels, 40_000_000,
            "default max_image_pixels"
        );
    }

    #[test]
    fn reads_custom_max_image_pixels() {
        let config = config_with(&[(MAX_IMAGE_PIXELS_VAR, "1000")]);
        assert_eq!(config.max_image_pixels, 1_000, "custom MAX_IMAGE_PIXELS");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_image_pixels: 1000"),
            "the pixel limit is visible in Debug output: {debug}"
        );
        let err = try_config_with(&[(MAX_IMAGE_PIXELS_VAR, "many")]).unwrap_err();
        assert_eq!(err, ConfigError::InvalidNumber(MAX_IMAGE_PIXELS_VAR));
    }

    #[test]
    fn inbox_max_bytes_defaults_to_one_mebibyte() {
        let config = config_with(&[]);
        assert_eq!(
            config.inbox_max_bytes, 1_048_576,
            "default INBOX_MAX_BYTES is 1 MiB"
        );
    }

    #[test]
    fn reads_custom_inbox_max_bytes() {
        let config = config_with(&[(INBOX_MAX_BYTES_VAR, "1024")]);
        assert_eq!(config.inbox_max_bytes, 1_024, "custom INBOX_MAX_BYTES");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("inbox_max_bytes: 1024"),
            "the inbox body cap is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn max_remote_mention_lookups_defaults_to_ten() {
        let config = config_with(&[]);
        assert_eq!(
            config.max_remote_mention_lookups, 10,
            "default MAX_REMOTE_MENTION_LOOKUPS is ten"
        );
        assert_eq!(
            Config::for_tests().max_remote_mention_lookups,
            10,
            "the test config keeps the default, so mention tests see it too"
        );
    }

    #[test]
    fn reads_custom_max_remote_mention_lookups() {
        let config = config_with(&[(MAX_REMOTE_MENTION_LOOKUPS_VAR, "3")]);
        assert_eq!(
            config.max_remote_mention_lookups, 3,
            "custom MAX_REMOTE_MENTION_LOOKUPS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_remote_mention_lookups: 3"),
            "the remote mention cap is visible in Debug output: {debug}"
        );
        let err = try_config_with(&[(MAX_REMOTE_MENTION_LOOKUPS_VAR, "plenty")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidNumber(MAX_REMOTE_MENTION_LOOKUPS_VAR),
            "a non-number cap is refused"
        );
    }

    #[test]
    fn the_chat_length_default_matches_the_storage_backstop() {
        // Same pairing as `models::oauth_token` keeps between the token-ttl
        // default and its model constant: the endpoint refuses above the
        // config number, the model above its own, and by default the two
        // numbers are one number.
        assert_eq!(
            DEFAULT_MAX_CHAT_MESSAGE_CHARS,
            crate::models::chat::MAX_CHAT_MESSAGE_CHARS,
            "the config default and the model's backstop agree"
        );
    }

    #[test]
    fn registrations_open_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(REGISTRATIONS_OPEN_VAR, value)]);
            assert_eq!(
                config.registrations_open.get(),
                want,
                "REGISTRATIONS_OPEN={value:?} means {want} (unset is open)"
            );
        }
    }

    #[test]
    fn invalid_registrations_open_is_an_error() {
        let err = try_config_with(&[(REGISTRATIONS_OPEN_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(REGISTRATIONS_OPEN_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {REGISTRATIONS_OPEN_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn debug_shows_registrations_open() {
        let config = config_with(&[(REGISTRATIONS_OPEN_VAR, "false")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("registrations_open: Toggle(false)"),
            "the registration toggle is visible in Debug output: {debug}"
        );
    }

    /// `INVITES_ENABLED` keeps its false default when the variable is
    /// unset: closed registrations refuse every attempt until the
    /// operator turns invites on.
    #[test]
    fn invites_are_off_by_default() {
        let config = config_with(&[]);
        assert!(
            !config.invites_enabled.get(),
            "invites are off by default: closed registrations refuse every attempt"
        );
    }

    #[test]
    fn invites_enabled_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(INVITES_ENABLED_VAR, value)]);
            assert_eq!(
                config.invites_enabled.get(),
                want,
                "INVITES_ENABLED={value:?} means {want} (unset is off)"
            );
        }
    }

    #[test]
    fn invalid_invites_enabled_is_an_error() {
        let err = try_config_with(&[(INVITES_ENABLED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(INVITES_ENABLED_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {INVITES_ENABLED_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn debug_shows_invites_enabled() {
        let config = config_with(&[(INVITES_ENABLED_VAR, "true")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("invites_enabled: Toggle(true)"),
            "the invite toggle is visible in Debug output: {debug}"
        );
    }

    /// Both registration modes are off until the operator says otherwise:
    /// a plain instance registers and logs in without approval or an email
    /// confirmation.
    #[test]
    fn registration_modes_are_off_by_default() {
        let config = config_with(&[]);
        assert!(
            !config.registrations_approval_required.get(),
            "approval is not required by default"
        );
        assert!(
            !config.account_activation_required.get(),
            "email confirmation is not required by default"
        );
    }

    #[test]
    fn registration_mode_toggles_parse_booleans() {
        for var in [
            REGISTRATIONS_APPROVAL_REQUIRED_VAR,
            ACCOUNT_ACTIVATION_REQUIRED_VAR,
        ] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
                ("", false),
            ] {
                let config = config_with(&[(var, value)]);
                let got = match var {
                    REGISTRATIONS_APPROVAL_REQUIRED_VAR => config.registrations_approval_required,
                    _ => config.account_activation_required,
                };
                assert_eq!(got.get(), want, "{var}={value:?} means {want}");
            }
        }
    }

    #[test]
    fn invalid_registration_modes_are_errors() {
        for var in [
            REGISTRATIONS_APPROVAL_REQUIRED_VAR,
            ACCOUNT_ACTIVATION_REQUIRED_VAR,
        ] {
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(
                err,
                ConfigError::InvalidBoolean(var),
                "error variant for {var}"
            );
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names the variable"
            );
        }
    }

    #[test]
    fn debug_shows_the_registration_modes() {
        let config = config_with(&[
            (REGISTRATIONS_APPROVAL_REQUIRED_VAR, "true"),
            (ACCOUNT_ACTIVATION_REQUIRED_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("registrations_approval_required: Toggle(true)"),
            "the approval toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("account_activation_required: Toggle(true)"),
            "the activation toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn fetch_toggles_parse_booleans() {
        for var in [SIGN_FETCHES_VAR, AUTHORIZED_FETCH_VAR] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
            ] {
                let config = config_with(&[(var, value)]);
                let got = match var {
                    SIGN_FETCHES_VAR => config.fetch.sign_fetches,
                    _ => config.fetch.authorized_fetch,
                };
                assert_eq!(got.get(), want, "{var}={value:?} means {want}");
            }
        }
    }

    #[test]
    fn fetch_toggle_defaults_apply_when_unset_or_empty() {
        let unset = config_with(&[]);
        assert!(
            unset.fetch.sign_fetches.get() && !unset.fetch.authorized_fetch.get(),
            "unset toggles sit at their defaults"
        );
        let config = config_with(&[(SIGN_FETCHES_VAR, ""), (AUTHORIZED_FETCH_VAR, "")]);
        assert!(
            config.fetch.sign_fetches.get(),
            "empty SIGN_FETCHES stays at the default"
        );
        assert!(
            !config.fetch.authorized_fetch.get(),
            "empty AUTHORIZED_FETCH stays at the default"
        );
    }

    #[test]
    fn invalid_fetch_toggles_are_errors() {
        for var in [SIGN_FETCHES_VAR, AUTHORIZED_FETCH_VAR] {
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBoolean(var), "error for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }

    #[test]
    fn debug_shows_the_fetch_toggles() {
        let config = config_with(&[(SIGN_FETCHES_VAR, "false"), (AUTHORIZED_FETCH_VAR, "true")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("sign_fetches: Toggle(false)"),
            "the signed-fetch toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("authorized_fetch: Toggle(true)"),
            "the authorized-fetch toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn fetch_bounds_default_to_twenty() {
        let config = config_with(&[]);
        assert_eq!(
            config.fetch.limits.thread_fetch_depth, 20,
            "default THREAD_FETCH_DEPTH is twenty ancestors"
        );
        assert_eq!(
            config.fetch.limits.max_fetches_per_activity, 20,
            "default MAX_FETCHES_PER_ACTIVITY is twenty dereferences"
        );
    }

    #[test]
    fn fetch_bounds_parse_numbers() {
        let config = config_with(&[
            (THREAD_FETCH_DEPTH_VAR, "8"),
            (MAX_FETCHES_PER_ACTIVITY_VAR, "3"),
        ]);
        assert_eq!(
            config.fetch.limits.thread_fetch_depth, 8,
            "custom THREAD_FETCH_DEPTH"
        );
        assert_eq!(
            config.fetch.limits.max_fetches_per_activity, 3,
            "custom MAX_FETCHES_PER_ACTIVITY"
        );
    }

    #[test]
    fn debug_shows_the_fetch_bounds() {
        let config = config_with(&[(THREAD_FETCH_DEPTH_VAR, "6")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("thread_fetch_depth: 6"),
            "the thread-fetch depth is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_fetches_per_activity: 20"),
            "the per-activity fetch cap is visible in Debug output: {debug}"
        );
    }

    /// `MRF_POLICIES` is unset by default: an instance runs no policy
    /// chain until a setting or the variable enables one.
    #[test]
    fn mrf_policies_default_to_the_empty_chain() {
        let config = config_with(&[]);
        assert!(
            config.mrf.policies.is_empty(),
            "no MRF policies without MRF_POLICIES or the `mrf` setting"
        );
    }

    /// `MRF_POLICIES` is a comma list: trimmed, empties dropped.
    #[test]
    fn mrf_policies_parses_a_comma_list() {
        let config = config_with(&[(MRF_POLICIES_VAR, "simple, keyword ,")]);
        assert_eq!(
            config.mrf.policies,
            vec!["simple".to_owned(), "keyword".to_owned()],
            "MRF_POLICIES lists the chain in order"
        );
    }

    /// `Debug` renders the MRF group (through the `Debug` chain's
    /// federation helper, beside the toggles the helper also prints).
    #[test]
    fn debug_shows_the_mrf_policies() {
        let config = config_with(&[(MRF_POLICIES_VAR, "noop,simple")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains(r#"policies: ["noop", "simple"]"#),
            "the policy-name list is visible in Debug output: {debug}"
        );
    }

    /// `MRF_TRANSPARENCY` is on by default: a moderating instance says so.
    #[test]
    fn mrf_transparency_is_on_by_default() {
        let config = config_with(&[]);
        assert!(
            config.mrf.transparency.get(),
            "the lists an instance moderates by are published unless its operator \
             says otherwise (Pleroma's `[:mrf, :transparency]` default)"
        );
        assert!(
            config.mrf.transparency_exclusions.is_empty(),
            "and nothing is excluded without MRF_TRANSPARENCY_EXCLUSIONS"
        );
    }

    /// `MRF_TRANSPARENCY` accepts the same two spellings as every other
    /// toggle.
    #[test]
    fn mrf_transparency_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("FALSE", false),
            (" 0 ", false),
            ("1", true),
            ("", true),
        ] {
            let config = config_with(&[(MRF_TRANSPARENCY_VAR, value)]);
            assert_eq!(
                config.mrf.transparency.get(),
                want,
                "MRF_TRANSPARENCY={value:?} means {want} (unset publishes)"
            );
        }
    }

    /// A value that is neither spelling refuses startup rather than
    /// silently publishing (or hiding) the lists.
    #[test]
    fn invalid_mrf_transparency_is_an_error() {
        let err = try_config_with(&[(MRF_TRANSPARENCY_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(MRF_TRANSPARENCY_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {MRF_TRANSPARENCY_VAR}: ")),
            "message names the variable"
        );
    }

    /// `MRF_TRANSPARENCY_EXCLUSIONS` is a comma list: trimmed, empties
    /// dropped, order kept.
    #[test]
    fn mrf_transparency_exclusions_parse_a_comma_list() {
        let config = config_with(&[(
            MRF_TRANSPARENCY_EXCLUSIONS_VAR,
            "*.partner.example, press.example ,",
        )]);
        assert_eq!(
            config.mrf.transparency_exclusions,
            vec!["*.partner.example".to_owned(), "press.example".to_owned()],
            "the patterns arrive as written, ready to match the lists with"
        );
    }

    /// `Debug` renders both transparency settings inside the MRF group:
    /// the switch and the hidden patterns (which are not secret — the point
    /// of the flag is that they are hidden from *anonymous* readers).
    #[test]
    fn debug_shows_the_mrf_transparency_settings() {
        let config = config_with(&[
            (MRF_TRANSPARENCY_VAR, "false"),
            (MRF_TRANSPARENCY_EXCLUSIONS_VAR, "press.example"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("transparency: Toggle(false)"),
            "the switch is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains(r#"transparency_exclusions: ["press.example"]"#),
            "and so is the hidden-pattern list: {debug}"
        );
    }

    /// `FEDERATION_ENABLED` keeps its true default when the variable is
    /// unset: an instance federates unless its operator turns it off.
    #[test]
    fn federation_is_enabled_by_default() {
        let config = config_with(&[]);
        assert!(
            config.federation_enabled.get(),
            "federation is on by default (Pleroma's `[:federator, :enabled]`)"
        );
    }

    #[test]
    fn federation_enabled_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(FEDERATION_ENABLED_VAR, value)]);
            assert_eq!(
                config.federation_enabled.get(),
                want,
                "FEDERATION_ENABLED={value:?} means {want} (unset federates)"
            );
        }
    }

    #[test]
    fn invalid_federation_enabled_is_an_error() {
        let err = try_config_with(&[(FEDERATION_ENABLED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(FEDERATION_ENABLED_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {FEDERATION_ENABLED_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn debug_shows_federation_enabled() {
        let config = config_with(&[(FEDERATION_ENABLED_VAR, "false")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("federation_enabled: Toggle(false)"),
            "the federation toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn accept_unsolicited_public_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(ACCEPT_UNSOLICITED_PUBLIC_VAR, value)]);
            assert_eq!(
                config.accept_unsolicited_public.get(),
                want,
                "ACCEPT_UNSOLICITED_PUBLIC={value:?} means {want} (unset accepts)"
            );
        }
    }

    #[test]
    fn invalid_accept_unsolicited_public_is_an_error() {
        let err = try_config_with(&[(ACCEPT_UNSOLICITED_PUBLIC_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ACCEPT_UNSOLICITED_PUBLIC_VAR),
            "error variant"
        );
    }

    #[test]
    fn debug_shows_accept_unsolicited_public() {
        let config = config_with(&[(ACCEPT_UNSOLICITED_PUBLIC_VAR, "false")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("accept_unsolicited_public: Toggle(false)"),
            "the ingest gate toggle is visible in Debug output: {debug}"
        );
    }

    /// The unreachable-host grace period: zero by default, so every marked
    /// host counts, the environment overriding it, and `Debug` showing the
    /// value.
    #[test]
    fn reads_custom_unreachable_grace_secs() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.unreachable_grace_secs, 0,
            "default UNREACHABLE_GRACE_SECS"
        );

        let config = config_with(&[(UNREACHABLE_GRACE_SECS_VAR, "3600")]);
        assert_eq!(
            config.unreachable_grace_secs, 3600,
            "custom UNREACHABLE_GRACE_SECS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("unreachable_grace_secs: 3600"),
            "the grace period is visible in Debug output: {debug}"
        );
    }

    /// An empty `UNREACHABLE_GRACE_SECS` is the unset state, and anything
    /// that is not a whole number refuses startup.
    #[test]
    fn invalid_unreachable_grace_secs_is_an_error() {
        let blank = config_with(&[(UNREACHABLE_GRACE_SECS_VAR, "")]);
        assert_eq!(
            blank.unreachable_grace_secs, 0,
            "an empty value is the default, not an error"
        );

        let err = try_config_with(&[(UNREACHABLE_GRACE_SECS_VAR, "an-hour")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidNumber(UNREACHABLE_GRACE_SECS_VAR),
            "error variant"
        );
    }

    #[test]
    fn toggle_serializes_as_its_inner_bool() {
        assert_eq!(
            serde_json::to_value(Toggle::new(true)).unwrap(),
            json!(true)
        );
        assert_eq!(
            serde_json::to_value(Toggle::new(false)).unwrap(),
            json!(false)
        );
    }

    #[test]
    fn media_base_url_is_trimmed_and_defaults_to_the_base_url() {
        for (value, want) in [
            (
                " https://cdn.example/media/// ",
                "https://cdn.example/media",
            ),
            ("", "https://social.example/media"),
            ("/", "https://social.example/media"),
            ("https://cdn.example/media", "https://cdn.example/media"),
        ] {
            let config = config_with(&[(MEDIA_BASE_URL_VAR, value)]);
            assert_eq!(
                config.media_base_url, want,
                "MEDIA_BASE_URL={value:?} means {want}"
            );
        }
        let with_port = config_with(&[(BASE_URL_VAR, "http://localhost:4000")]);
        assert_eq!(
            with_port.media_base_url, "http://localhost:4000/media",
            "the default follows BASE_URL, port included"
        );
    }

    /// `FRONTEND_BASE_URL`: unset or blank falls back to the normalized
    /// `BASE_URL`, a set value wins with its surrounding whitespace and
    /// trailing slashes stripped (the reading shared with `MEDIA_BASE_URL`
    /// and the media proxy's base URL, so nothing unusable can reach a
    /// `Location` header), and the field shows in Debug output.
    #[test]
    fn frontend_base_url_is_trimmed_and_defaults_to_the_base_url() {
        let unset = config_with(&[]);
        assert_eq!(
            unset.frontend_base_url, unset.base_url,
            "unset FRONTEND_BASE_URL equals BASE_URL"
        );
        assert_eq!(
            unset.frontend_base_url, ORIGIN,
            "the default is the normalized origin"
        );
        for (value, want) in [
            ("https://frontend.example", "https://frontend.example"),
            (" https://frontend.example/// ", "https://frontend.example"),
            ("https://frontend.example/", "https://frontend.example"),
            ("", ORIGIN),
            ("/", ORIGIN),
        ] {
            let config = config_with(&[(FRONTEND_BASE_URL_VAR, value)]);
            assert_eq!(
                config.frontend_base_url, want,
                "FRONTEND_BASE_URL={value:?} means {want}"
            );
        }
        let with_port = config_with(&[(BASE_URL_VAR, "http://localhost:4000")]);
        assert_eq!(
            with_port.frontend_base_url, "http://localhost:4000",
            "the default follows BASE_URL, port included"
        );
        let debug = format!(
            "{:?}",
            config_with(&[(FRONTEND_BASE_URL_VAR, " https://frontend.example/ ")])
        );
        assert!(
            debug.contains(r#"frontend_base_url: "https://frontend.example""#),
            "frontend_base_url is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn uploads_dir_is_trimmed_and_defaults_to_uploads() {
        for (value, want) in [
            ("  /var/uploads  ", "/var/uploads"),
            ("uploads", "uploads"),
            ("", "./uploads"),
            ("   ", "./uploads"),
        ] {
            let config = config_with(&[(UPLOADS_DIR_VAR, value)]);
            assert_eq!(
                config.uploads_dir,
                PathBuf::from(want),
                "UPLOADS_DIR={value:?} means {want}"
            );
        }
    }

    #[test]
    fn emoji_dir_is_trimmed_and_defaults_to_emoji() {
        for (value, want) in [
            ("  /var/emoji  ", "/var/emoji"),
            ("emoji", "emoji"),
            ("", "./emoji"),
            ("   ", "./emoji"),
        ] {
            let config = config_with(&[(EMOJI_DIR_VAR, value)]);
            assert_eq!(
                config.emoji_dir,
                PathBuf::from(want),
                "EMOJI_DIR={value:?} means {want}"
            );
        }
    }

    #[test]
    fn http_allow_private_hosts_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(HTTP_ALLOW_PRIVATE_HOSTS_VAR, value)]);
            assert_eq!(
                config.fetch.http_allow_private_hosts, want,
                "HTTP_ALLOW_PRIVATE_HOSTS={value:?} means {want}"
            );
        }
    }

    #[test]
    fn invalid_http_allow_private_hosts_is_an_error() {
        let err = try_config_with(&[(HTTP_ALLOW_PRIVATE_HOSTS_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(HTTP_ALLOW_PRIVATE_HOSTS_VAR),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {HTTP_ALLOW_PRIVATE_HOSTS_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn accounts_email_optional_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(ACCOUNTS_EMAIL_OPTIONAL_VAR, value)]);
            assert_eq!(
                config.accounts_email_optional, want,
                "ACCOUNTS_EMAIL_OPTIONAL={value:?} means {want}"
            );
        }
    }

    #[test]
    fn invalid_accounts_email_optional_is_an_error() {
        let err = try_config_with(&[(ACCOUNTS_EMAIL_OPTIONAL_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ACCOUNTS_EMAIL_OPTIONAL_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {ACCOUNTS_EMAIL_OPTIONAL_VAR}: ")),
            "message names the variable"
        );
    }

    /// Email confirmation: off by default (this server activates accounts
    /// at registration), every boolean spelling read, junk refused.
    #[test]
    fn account_activation_required_defaults_off_and_parses_booleans() {
        assert!(
            !config_with(&[]).account_activation_required.get(),
            "no account must confirm its email by default"
        );
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(ACCOUNT_ACTIVATION_REQUIRED_VAR, value)]);
            assert_eq!(
                config.account_activation_required.get(),
                want,
                "ACCOUNT_ACTIVATION_REQUIRED={value:?} means {want}"
            );
        }
        let err = try_config_with(&[(ACCOUNT_ACTIVATION_REQUIRED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ACCOUNT_ACTIVATION_REQUIRED_VAR),
            "error variant"
        );
    }

    /// Account deletion: offered by default (Pleroma offers the endpoint),
    /// every boolean spelling read, junk refused.
    #[test]
    fn allow_account_deletion_defaults_on_and_parses_booleans() {
        assert!(
            config_with(&[]).allow_account_deletion.get(),
            "accounts may delete themselves by default"
        );
        for (value, want) in [
            ("true", true),
            ("1", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(ALLOW_ACCOUNT_DELETION_VAR, value)]);
            assert_eq!(
                config.allow_account_deletion.get(),
                want,
                "ALLOW_ACCOUNT_DELETION={value:?} means {want} (blank falls back)"
            );
        }
        let err = try_config_with(&[(ALLOW_ACCOUNT_DELETION_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ALLOW_ACCOUNT_DELETION_VAR),
            "error variant"
        );
    }

    #[test]
    fn reads_custom_min_password_length() {
        let config = config_with(&[(MIN_PASSWORD_LENGTH_VAR, "3")]);
        assert_eq!(
            config.min_password_length, 3,
            "MIN_PASSWORD_LENGTH below the 8-character default is honored"
        );
    }

    #[test]
    fn reads_password_reset_token_ttl() {
        let default = config_with(&[]);
        assert_eq!(
            default.password_reset_ttl_secs, 86_400,
            "default PASSWORD_RESET_TTL_SECS is 24 hours"
        );
        let custom = config_with(&[(PASSWORD_RESET_TTL_SECS_VAR, "7200")]);
        assert_eq!(
            custom.password_reset_ttl_secs, 7_200,
            "custom PASSWORD_RESET_TTL_SECS is honored"
        );
    }

    #[test]
    fn debug_shows_the_registration_settings() {
        let config = config_with(&[
            (ACCOUNTS_EMAIL_OPTIONAL_VAR, "true"),
            (MIN_PASSWORD_LENGTH_VAR, "10"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("accounts_email_optional: true"),
            "the email flag is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("min_password_length: 10"),
            "the password minimum is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn debug_shows_http_allow_private_hosts() {
        let config = config_with(&[(HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true")]);
        assert!(
            format!("{config:?}").contains("http_allow_private_hosts: true"),
            "the guard flag is visible in Debug output"
        );
    }

    #[test]
    fn debug_shows_the_upload_storage_settings() {
        let config = config_with(&[(UPLOADS_DIR_VAR, "/srv/uploads")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("uploads_dir: \"/srv/uploads\""),
            "the uploads directory is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("media_base_url: \"https://social.example/media\""),
            "the media URL prefix is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("emoji_dir: \"./emoji\""),
            "the emoji packs directory is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn debug_shows_the_token_ttl_setting() {
        let config = config_with(&[
            (OAUTH_TOKEN_TTL_SECS_VAR, "600"),
            (PASSWORD_RESET_TTL_SECS_VAR, "1200"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("oauth_token_ttl_secs: 600"),
            "the token lifetime is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("password_reset_ttl_secs: 1200"),
            "the reset-token lifetime is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn misc_toggles_parse_booleans() {
        for var in [
            MONGOOSEIM_ENABLED_VAR,
            CAPTCHA_ENABLED_VAR,
            HEALTHCHECK_ENABLED_VAR,
        ] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
                ("", false),
            ] {
                let config = config_with(&[(var, value)]);
                let got = match var {
                    MONGOOSEIM_ENABLED_VAR => config.mongooseim_enabled,
                    CAPTCHA_ENABLED_VAR => config.captcha_enabled,
                    _ => config.healthcheck_enabled,
                };
                assert_eq!(got.get(), want, "{var}={value:?} means {want}");
            }
        }
    }

    #[test]
    fn invalid_misc_toggles_are_errors() {
        for var in [
            MONGOOSEIM_ENABLED_VAR,
            CAPTCHA_ENABLED_VAR,
            HEALTHCHECK_ENABLED_VAR,
        ] {
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBoolean(var), "error for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }

    #[test]
    fn debug_shows_the_misc_toggles() {
        let config = config_with(&[
            (MONGOOSEIM_ENABLED_VAR, "true"),
            (CAPTCHA_ENABLED_VAR, "false"),
            (HEALTHCHECK_ENABLED_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("mongooseim_enabled: Toggle(true)"),
            "the `MongooseIM` toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("captcha_enabled: Toggle(false)"),
            "the captcha toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("healthcheck_enabled: Toggle(true)"),
            "the healthcheck toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn media_proxy_defaults_to_disabled_passthrough() {
        let config = config_with(&[]);
        assert!(!config.media_proxy.enabled, "the proxy is off by default");
        assert_eq!(
            config.media_proxy.secret, None,
            "no signing secret unless configured"
        );
        assert_eq!(
            config.media_proxy.base_url, None,
            "proxy links default to BASE_URL"
        );
        assert_eq!(
            config.media_proxy.max_bytes, 50_000_000,
            "default MEDIA_PROXY_MAX_BYTES"
        );
        assert!(
            config.media_proxy.blacklist.is_empty(),
            "nothing is blacklisted by default"
        );
    }

    #[test]
    fn reads_custom_media_proxy_settings() {
        let config = config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (
                MEDIA_PROXY_SECRET_VAR,
                "  a-secret-of-exactly-32-characters  ",
            ),
            (MEDIA_PROXY_BASE_URL_VAR, " https://cdn.example/ "),
            (MEDIA_PROXY_MAX_BYTES_VAR, "1024"),
            (
                MEDIA_PROXY_BLACKLIST_VAR,
                "evil.example, cdn-only.example ,",
            ),
        ]);
        assert!(config.media_proxy.enabled, "custom MEDIA_PROXY_ENABLED");
        assert_eq!(
            config.media_proxy.secret.as_deref(),
            Some("a-secret-of-exactly-32-characters"),
            "the secret is trimmed"
        );
        assert_eq!(
            config.media_proxy.base_url.as_deref(),
            Some("https://cdn.example"),
            "MEDIA_PROXY_BASE_URL is trimmed and loses its trailing slash"
        );
        assert_eq!(config.media_proxy.max_bytes, 1024, "custom byte cap");
        assert_eq!(
            config.media_proxy.blacklist,
            vec!["evil.example".to_owned(), "cdn-only.example".to_owned()],
            "the blacklist is a trimmed comma list"
        );
    }

    #[test]
    fn media_proxy_secret_empty_string_is_unset() {
        let err = try_config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (MEDIA_PROXY_SECRET_VAR, "   "),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            ConfigError::Missing(MEDIA_PROXY_SECRET_VAR),
            "an empty secret is a missing secret when enabled"
        );
    }

    #[test]
    fn enabling_the_media_proxy_without_a_secret_is_an_error() {
        let err = try_config_with(&[(MEDIA_PROXY_ENABLED_VAR, "true")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Missing(MEDIA_PROXY_SECRET_VAR),
            "error variant"
        );
        assert!(
            err.to_string().contains(MEDIA_PROXY_SECRET_VAR),
            "message names the variable: {err}"
        );
    }

    #[test]
    fn short_media_proxy_secret_is_an_error() {
        let err = try_config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (MEDIA_PROXY_SECRET_VAR, "too-short"),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            ConfigError::ShortMediaProxySecret(MEDIA_PROXY_SECRET_VAR),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid MEDIA_PROXY_SECRET: expected at least 32 characters",
            "message states the minimum"
        );
    }

    #[test]
    fn media_proxy_secret_needs_no_length_when_disabled() {
        let config = config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "false"),
            (MEDIA_PROXY_SECRET_VAR, "short"),
        ]);
        assert!(
            !config.media_proxy.enabled,
            "the disabled proxy accepts any configured secret"
        );
        assert_eq!(
            config.media_proxy.secret.as_deref(),
            Some("short"),
            "the configured secret is kept for when it is enabled"
        );
    }

    #[test]
    fn invalid_media_proxy_enabled_is_an_error() {
        let err = try_config_with(&[(MEDIA_PROXY_ENABLED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(MEDIA_PROXY_ENABLED_VAR),
            "error variant"
        );
    }

    #[test]
    fn rich_media_defaults_to_nothing_ignored_and_no_remote_cards() {
        let config = config_with(&[]);
        assert!(
            config.rich_media_ignore_hosts.is_empty(),
            "nothing is ignored by default"
        );
        assert!(
            !config.rich_media_remote.get(),
            "remote cards are off by default"
        );
    }

    #[test]
    fn reads_custom_rich_media_settings() {
        let config = config_with(&[
            (RICH_MEDIA_IGNORE_HOSTS_VAR, "internal.example, 127.0.0.1 ,"),
            (RICH_MEDIA_REMOTE_VAR, "true"),
        ]);
        assert_eq!(
            config.rich_media_ignore_hosts,
            vec!["internal.example".to_owned(), "127.0.0.1".to_owned()],
            "the ignore list is a trimmed comma list"
        );
        assert!(config.rich_media_remote.get(), "custom RICH_MEDIA_REMOTE");
    }

    #[test]
    fn rich_media_ignore_list_of_only_separators_is_empty() {
        let config = config_with(&[(RICH_MEDIA_IGNORE_HOSTS_VAR, " , , ")]);
        assert!(
            config.rich_media_ignore_hosts.is_empty(),
            "blank entries fold away, like the media proxy's blacklist"
        );
    }

    #[test]
    fn invalid_rich_media_remote_is_an_error() {
        let err = try_config_with(&[(RICH_MEDIA_REMOTE_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(RICH_MEDIA_REMOTE_VAR),
            "error variant"
        );
    }

    #[test]
    fn debug_shows_the_rich_media_settings() {
        let config = config_with(&[
            (RICH_MEDIA_IGNORE_HOSTS_VAR, "blocked.example"),
            (RICH_MEDIA_REMOTE_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("rich_media_ignore_hosts: [\"blocked.example\"]"),
            "the ignore list is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("rich_media_remote: Toggle(true)"),
            "the remote toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn debug_redacts_the_media_proxy_secret() {
        let config = config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (MEDIA_PROXY_SECRET_VAR, "a-secret-of-exactly-32-characters"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-secret-of-exactly-32-characters"),
            "the signing secret must not leak: {debug}"
        );
        assert!(
            debug.contains("secret: Some(\"<redacted>\")"),
            "the redaction marker stands in: {debug}"
        );
        assert!(
            debug.contains("enabled: true"),
            "the enablement flag is visible: {debug}"
        );
    }

    #[test]
    fn translation_is_off_by_default() {
        let config = config_with(&[]);
        assert_eq!(
            config.translation.provider,
            TranslationProvider::None,
            "no provider until the operator names one"
        );
        assert_eq!(config.translation.api_url, None, "no API URL by default");
        assert_eq!(config.translation.api_key, None, "no API key by default");
        assert!(
            !config.translation.is_configured(),
            "an unconfigured provider is reported as such"
        );
    }

    #[test]
    fn reads_custom_translation_settings() {
        let config = config_with(&[
            (TRANSLATION_PROVIDER_VAR, " LibreTranslate "),
            (TRANSLATION_API_URL_VAR, " https://translate.example/ "),
            (TRANSLATION_API_KEY_VAR, "  lt-key  "),
        ]);
        assert_eq!(
            config.translation.provider,
            TranslationProvider::LibreTranslate,
            "the provider name is matched case-insensitively and trimmed"
        );
        assert_eq!(
            config.translation.api_url.as_deref(),
            Some("https://translate.example"),
            "TRANSLATION_API_URL is trimmed and loses its trailing slash"
        );
        assert_eq!(
            config.translation.api_key.as_deref(),
            Some("lt-key"),
            "TRANSLATION_API_KEY is trimmed"
        );
        assert!(
            config.translation.is_configured(),
            "a named provider with a URL is usable"
        );
    }

    #[test]
    fn translation_provider_accepts_every_spelling() {
        for (value, provider) in [
            ("none", TranslationProvider::None),
            ("NONE", TranslationProvider::None),
            ("", TranslationProvider::None),
            ("   ", TranslationProvider::None),
            ("libretranslate", TranslationProvider::LibreTranslate),
            ("LibreTranslate", TranslationProvider::LibreTranslate),
            ("deepl", TranslationProvider::DeepL),
            ("DeepL", TranslationProvider::DeepL),
        ] {
            let config = config_with(&[(TRANSLATION_PROVIDER_VAR, value)]);
            assert_eq!(
                config.translation.provider, provider,
                "`{value}` selects its provider"
            );
        }
    }

    #[test]
    fn invalid_translation_provider_is_an_error() {
        let err = try_config_with(&[(TRANSLATION_PROVIDER_VAR, "google")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidTranslationProvider(TRANSLATION_PROVIDER_VAR),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid TRANSLATION_PROVIDER: expected one of none, libretranslate or deepl",
            "message names the variable and its three values"
        );
    }

    #[test]
    fn a_provider_without_a_url_or_key_is_not_configured() {
        let urlless = config_with(&[(TRANSLATION_PROVIDER_VAR, "libretranslate")]);
        assert!(
            !urlless.translation.is_configured(),
            "a provider with no server to talk to is unusable"
        );
        let keyless = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "https://api.deepl.example"),
        ]);
        assert!(
            !keyless.translation.is_configured(),
            "DeepL has no unauthenticated mode, so its key is required"
        );
        let with_key = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "https://api.deepl.example"),
            (TRANSLATION_API_KEY_VAR, "dl-key"),
        ]);
        assert!(
            with_key.translation.is_configured(),
            "DeepL with URL and key is usable"
        );
        let hosted = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "libretranslate"),
            (TRANSLATION_API_URL_VAR, "https://translate.example"),
        ]);
        assert!(
            hosted.translation.is_configured(),
            "LibreTranslate needs no key: a self-hosted server asks for none"
        );
    }

    #[test]
    fn empty_translation_url_and_key_are_unset() {
        let config = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "   "),
            (TRANSLATION_API_KEY_VAR, ""),
        ]);
        assert_eq!(
            config.translation.api_url, None,
            "a blank URL reads as unset"
        );
        assert_eq!(
            config.translation.api_key, None,
            "a blank key reads as unset"
        );
        assert!(
            !config.translation.is_configured(),
            "so the provider stays unconfigured"
        );
    }

    #[test]
    fn vapid_overrides_default_to_unset() {
        let config = Config::for_tests();
        assert_eq!(config.vapid.private_key, None, "default private key");
        assert_eq!(config.vapid.public_key, None, "default public key");
    }

    #[test]
    fn vapid_overrides_are_trimmed_and_blanks_unset() {
        let set = config_with(&[
            (VAPID_PRIVATE_KEY_VAR, "  PEM-BODY-WITH-SPACES  "),
            (VAPID_PUBLIC_KEY_VAR, "  url-safe-public-key  "),
        ]);
        assert_eq!(
            set.vapid.private_key.as_deref(),
            Some("PEM-BODY-WITH-SPACES"),
            "the private half reads trimmed"
        );
        assert_eq!(
            set.vapid.public_key.as_deref(),
            Some("url-safe-public-key"),
            "the public half reads trimmed"
        );
        for (private, public) in [("   ", ""), ("", "  ")] {
            let blank = config_with(&[
                (VAPID_PRIVATE_KEY_VAR, private),
                (VAPID_PUBLIC_KEY_VAR, public),
            ]);
            assert_eq!(
                (
                    blank.vapid.private_key.as_deref(),
                    blank.vapid.public_key.as_deref()
                ),
                (None, None),
                "blank overrides read as unset, keeping the generated pair"
            );
        }
    }

    #[test]
    fn debug_redacts_the_vapid_private_key() {
        let config = config_with(&[
            (VAPID_PRIVATE_KEY_VAR, "a-signing-secret-never-logged"),
            (VAPID_PUBLIC_KEY_VAR, "the-advertised-public-half"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-signing-secret-never-logged"),
            "the private key must not leak: {debug}"
        );
        assert!(
            debug.contains("private_key: Some(\"<redacted>\")"),
            "the redaction marker stands in: {debug}"
        );
        assert!(
            debug.contains("public_key: Some(\"the-advertised-public-half\")"),
            "the public half shows: {debug}"
        );
    }

    #[test]
    fn debug_redacts_the_translation_api_key() {
        let config = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "https://api.deepl.example"),
            (TRANSLATION_API_KEY_VAR, "a-provider-secret-never-logged"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-provider-secret-never-logged"),
            "the API key must not leak: {debug}"
        );
        assert!(
            debug.contains("api_key: Some(\"<redacted>\")"),
            "the redaction marker stands in: {debug}"
        );
        assert!(
            debug.contains("provider: DeepL"),
            "the provider itself is visible: {debug}"
        );
    }

    #[test]
    fn empty_contact_email_is_none() {
        let config = config_with(&[(CONTACT_EMAIL_VAR, "")]);
        assert_eq!(config.contact_email, None, "empty contact_email");
    }

    #[test]
    fn available_frontends_default_to_empty() {
        let config = config_with(&[]);
        assert!(
            config.available_frontends.is_empty(),
            "no frontend is pickable until the operator names some"
        );
    }

    #[test]
    fn available_frontends_parses_a_comma_list() {
        let config = config_with(&[(AVAILABLE_FRONTENDS_VAR, " pleroma-fe, mochi-pub ,,x")]);
        assert_eq!(
            config.available_frontends,
            vec![
                "pleroma-fe".to_owned(),
                "mochi-pub".to_owned(),
                "x".to_owned()
            ],
            "the list is trimmed and blanks drop, order kept"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains(r#"available_frontends: ["pleroma-fe", "mochi-pub", "x"]"#),
            "the frontend list is visible in Debug output: {debug}"
        );
    }

    /// The ticket's one-provider `OAUTH_PROVIDERS` document, verbatim.
    const GITHUB_PROVIDERS: &str = r#"[{"name":"github","client_id":"cid","client_secret":"csecret","authorize_url":"https://github.com/login/oauth/authorize","token_url":"https://github.com/login/oauth/access_token","userinfo_url":"https://api.github.com/user","scopes":"read:user user:email","uid_field":"id","nickname_field":"login","email_field":"email","name_field":"name"}]"#;

    #[test]
    fn oauth_providers_default_to_empty() {
        for value in [None, Some(""), Some("   ")] {
            let extra = value.map_or_else(Vec::new, |text| vec![(OAUTH_PROVIDERS_VAR, text)]);
            let config = config_with(&extra);
            assert!(
                config.oauth_providers.is_empty(),
                "OAUTH_PROVIDERS={value:?} means no provider"
            );
            assert!(
                config.oauth_provider("github").is_none(),
                "and no name is configured"
            );
        }
    }

    #[test]
    fn oauth_providers_parses_the_documented_shape() {
        let config = config_with(&[(OAUTH_PROVIDERS_VAR, GITHUB_PROVIDERS)]);
        assert_eq!(config.oauth_providers.len(), 1, "one provider");
        let github = config.oauth_provider("github").expect("github");
        assert_eq!(github.name, "github", "name");
        assert_eq!(github.client_id, "cid", "client_id");
        assert_eq!(github.client_secret, "csecret", "client_secret");
        assert_eq!(
            github.authorize_url, "https://github.com/login/oauth/authorize",
            "authorize_url"
        );
        assert_eq!(
            github.token_url, "https://github.com/login/oauth/access_token",
            "token_url"
        );
        assert_eq!(
            github.userinfo_url, "https://api.github.com/user",
            "userinfo_url"
        );
        assert_eq!(github.scopes, "read:user user:email", "scopes");
        assert_eq!(github.uid_field, "id", "uid_field");
        assert_eq!(github.nickname_field, "login", "nickname_field");
        assert_eq!(github.email_field, "email", "email_field");
        assert_eq!(github.name_field, "name", "name_field");
        assert!(
            config.oauth_provider("GitLab").is_none(),
            "a name nobody configured is not found"
        );
    }

    #[test]
    fn oauth_providers_lists_them_all_and_applies_the_defaults() {
        let config = config_with(&[(
            OAUTH_PROVIDERS_VAR,
            r#"[{"name":"github","client_id":"a","client_secret":"b","authorize_url":"https://g.example/authorize","token_url":"https://g.example/token","userinfo_url":"https://g.example/user"},{"name":"keycloak","client_id":"c","client_secret":"d","authorize_url":"https://k.example/authorize","token_url":"https://k.example/token","userinfo_url":"https://k.example/userinfo","scopes":"openid","uid_field":"sub","nickname_field":"preferred_username","email_field":"email","name_field":"name"}]"#,
        )]);
        let names: Vec<&str> = config
            .oauth_providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["github", "keycloak"],
            "the array keeps its order"
        );
        let github = config.oauth_provider("github").expect("github");
        assert_eq!(github.scopes, "", "scopes defaults to sending none");
        assert_eq!(github.uid_field, "id", "uid_field defaults to id");
        assert_eq!(
            github.nickname_field, "",
            "an unnamed field reads as absent"
        );
        assert_eq!(
            config
                .oauth_provider("keycloak")
                .map(|p| p.uid_field.as_str()),
            Some("sub"),
            "an explicit name wins"
        );
    }

    #[test]
    fn invalid_oauth_providers_is_a_startup_error() {
        for value in [
            "not json",
            r#"{"name":"github"}"#,
            "[]]",
            r#"[{"name":"github","client_id":"a","client_secret":"b","authorize_url":"https://g.example/a","token_url":"https://g.example/t","userinfo_url":"https://g.example/u","bogus":1}]"#,
            r#"[{"client_id":"a","client_secret":"b","authorize_url":"https://g.example/a","token_url":"https://g.example/t","userinfo_url":"https://g.example/u"}]"#,
        ] {
            let err = try_config_with(&[(OAUTH_PROVIDERS_VAR, value)]).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidOauthProviders(_)),
                "`{value}` is refused, got {err}"
            );
            assert!(
                err.to_string()
                    .starts_with(&format!("invalid {OAUTH_PROVIDERS_VAR}: ")),
                "the message names the variable"
            );
            assert!(err.source().is_none(), "no source");
        }
    }

    #[test]
    fn debug_redacts_the_provider_client_secret() {
        let config = config_with(&[(OAUTH_PROVIDERS_VAR, GITHUB_PROVIDERS)]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("csecret"),
            "the provider secret must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"client_secret: "<redacted>""#),
            "the marker names the field: {debug}"
        );
        assert!(
            debug.contains("https://api.github.com/user"),
            "the rest of the provider shows: {debug}"
        );
    }

    #[test]
    fn missing_base_url_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[(DATABASE_URL_VAR, URL)])).unwrap_err();
        assert_eq!(err, ConfigError::Missing(BASE_URL_VAR), "error variant");
        assert_eq!(
            err.to_string(),
            "required environment variable BASE_URL is not set",
            "message"
        );
        let empty_err = base_url_result("").unwrap_err();
        assert_eq!(
            empty_err,
            ConfigError::Missing(BASE_URL_VAR),
            "empty BASE_URL is missing"
        );
    }

    #[test]
    fn base_url_with_invalid_scheme_is_an_error() {
        for value in [
            "ftp://example.com",
            "https://",
            "not a url",
            "social.example",
        ] {
            let err = base_url_result(value).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBaseUrl, "error for {value}");
            assert!(err.source().is_none(), "no source for {value}");
        }
        let message = ConfigError::InvalidBaseUrl.to_string();
        assert!(
            message.starts_with("invalid BASE_URL: "),
            "message names the variable: {message}"
        );
    }

    #[test]
    fn base_url_with_path_or_query_is_an_error() {
        for value in [
            "https://example.com/instance",
            "https://example.com/a/b",
            "https://example.com?next=/login",
        ] {
            let err = base_url_result(value).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBaseUrl, "error for {value}");
        }
    }

    #[test]
    fn base_url_trailing_slash_is_stripped() {
        let config = base_url_result("https://social.example/").unwrap();
        assert_eq!(config.base_url, ORIGIN, "trailing slash stripped");
        assert_eq!(config.domain(), "social.example", "domain unchanged");
    }

    #[test]
    fn base_url_with_port_keeps_it_in_domain() {
        let config = base_url_result("http://localhost:4000").unwrap();
        assert_eq!(config.base_url, "http://localhost:4000", "base_url kept");
        assert_eq!(config.domain(), "localhost:4000", "domain with port");
        assert!(!config.is_https(), "http base_url is not https");
    }

    #[test]
    fn base_url_default_port_is_dropped() {
        let config = base_url_result("https://example.com:443/").unwrap();
        assert_eq!(config.base_url, "https://example.com", "default port gone");
        assert_eq!(config.domain(), "example.com", "domain without port");
        assert!(config.is_https(), "https stays https");
    }

    #[test]
    fn invalid_numbers_are_errors() {
        for var in [
            MAX_STATUS_CHARS_VAR,
            MAX_MEDIA_ATTACHMENTS_VAR,
            MAX_POLL_OPTIONS_VAR,
            MAX_POLL_OPTION_CHARS_VAR,
            MIN_POLL_EXPIRATION_VAR,
            MAX_POLL_EXPIRATION_VAR,
            MAX_CHAT_MESSAGE_CHARS_VAR,
            MAX_MEDIA_DESCRIPTION_VAR,
            MAX_DISPLAY_NAME_CHARS_VAR,
            MAX_BIO_CHARS_VAR,
            MAX_PROFILE_FIELDS_VAR,
            MAX_REPORT_COMMENT_VAR,
            MAX_PINNED_STATUSES_VAR,
            MIN_EXPIRATION_SECS_VAR,
            MIN_SCHEDULED_SECS_VAR,
            MAX_SCHEDULED_PER_USER_VAR,
            MAX_SCHEDULED_PER_USER_PER_DAY_VAR,
            UPLOAD_LIMIT_BYTES_VAR,
            UPLOAD_MAX_FILES_VAR,
            AVATAR_LIMIT_BYTES_VAR,
            BANNER_LIMIT_BYTES_VAR,
            INBOX_MAX_BYTES_VAR,
            MAX_REMOTE_MENTION_LOOKUPS_VAR,
            MIN_PASSWORD_LENGTH_VAR,
            OAUTH_TOKEN_TTL_SECS_VAR,
            PASSWORD_RESET_TTL_SECS_VAR,
            JOB_CONCURRENCY_VAR,
            PURGE_DELETED_STATUSES_DAYS_VAR,
            STREAMING_PING_INTERVAL_SECS_VAR,
            MEDIA_PROXY_MAX_BYTES_VAR,
            THREAD_FETCH_DEPTH_VAR,
            MAX_FETCHES_PER_ACTIVITY_VAR,
        ] {
            let err = try_config_with(&[(var, "many")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidNumber(var), "error for {var}");
            assert!(err.source().is_none(), "no source for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }

    #[test]
    fn debug_redacts_database_url() {
        let config = config_with(&[]);
        let debug = format!("{config:?}");
        // The URL is `postgres://user:secret@localhost/wired`; the literal
        // word `secret` also names the media proxy's (redacted) field, so
        // the leak check is against the credential, not the word.
        assert!(!debug.contains("user:secret@"), "password must not leak");
        assert!(debug.contains("<redacted>"), "redaction marker");
        assert!(debug.contains(ORIGIN), "base_url is shown");
    }

    #[test]
    fn from_env_reads_process_environment() {
        // Never mutates the environment; just checks `from_env` reads the real one.
        let from_env = Config::from_env();
        let from_lookup = Config::from_lookup(|key| std::env::var(key).ok());
        assert_eq!(
            from_env, from_lookup,
            "from_env matches the process environment"
        );
    }

    #[test]
    fn for_tests_is_the_fixed_lookup_config() {
        let config = Config::for_tests();
        assert_eq!(config.base_url, TEST_BASE_URL, "test base_url");
        assert_eq!(
            config.frontend_base_url, "https://test.example",
            "test frontend_base_url defaults onto the test base_url"
        );
        assert_eq!(config.database_url, TEST_DATABASE_URL, "placeholder url");
        assert_eq!(config.domain(), "test.example", "test domain");
        assert!(config.is_https(), "test base_url is https");
        assert!(
            config.fetch.http_allow_private_hosts,
            "test config disables the SSRF guard for local mini-servers"
        );
        assert_eq!(
            config.instance_name, DEFAULT_INSTANCE_NAME,
            "defaults apply"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from(DEFAULT_UPLOADS_DIR),
            "test uploads go to the default directory"
        );
        assert_eq!(
            config.emoji_dir,
            PathBuf::from(DEFAULT_EMOJI_DIR),
            "test packs are read from the default directory"
        );
        assert_eq!(
            config.media_base_url, "https://test.example/media",
            "test media URLs hang off the test BASE_URL"
        );
        assert_eq!(config.smtp_host, None, "test config has mail disabled");
        assert_eq!(
            config.mail_from, "noreply@test.example",
            "test mail defaults to the test domain"
        );
        assert!(
            !config.media_proxy.enabled,
            "the media proxy is off for tests by default, so entity URLs pass through"
        );
        assert!(
            config.rich_media_ignore_hosts.is_empty(),
            "the test config ignores no rich media hosts"
        );
        assert!(
            !config.rich_media_remote.get(),
            "rich media cards fetch no remote URLs for tests by default"
        );
        let expected = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, TEST_DATABASE_URL),
            (BASE_URL_VAR, TEST_BASE_URL),
            (HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true"),
            (RATE_LIMITS_ENABLED_VAR, "false"),
            (RICH_MEDIA_IGNORE_HOSTS_VAR, DEFAULT_RICH_MEDIA_IGNORE_HOSTS),
            (RICH_MEDIA_REMOTE_VAR, "false"),
        ]))
        .unwrap();
        assert_eq!(config, expected, "for_tests equals from_lookup");
    }

    #[test]
    fn for_tests_fallback_matches_parsed_config() {
        // `for_tests` only uses the fallback if parsing its fixed values ever
        // fails; this proves the fallback builds the very same Config.
        let parsed = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, TEST_DATABASE_URL),
            (BASE_URL_VAR, TEST_BASE_URL),
            (HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true"),
            (RATE_LIMITS_ENABLED_VAR, "false"),
            (RICH_MEDIA_IGNORE_HOSTS_VAR, DEFAULT_RICH_MEDIA_IGNORE_HOSTS),
            (RICH_MEDIA_REMOTE_VAR, "false"),
        ]))
        .unwrap();
        assert_eq!(
            Config::test_defaults(),
            parsed,
            "fallback equals the parsed test config"
        );
    }

    /// The limiter's three settings, at their defaults: on, Pleroma's
    /// bucket table, and no proxy trust.
    #[test]
    fn rate_limit_settings_default() {
        let config = config_with(&[]);
        assert!(
            config.rate_limit.enabled.get(),
            "rate limiting is on by default"
        );
        assert!(
            !config.rate_limit.trust_proxy_headers.get(),
            "X-Forwarded-For is not trusted by default"
        );
        assert_eq!(
            config.rate_limit.limits,
            rate_limit::default_limits(),
            "the table is Pleroma's defaults"
        );
        let authentication = config
            .rate_limit
            .limits
            .get("authentication")
            .copied()
            .unwrap();
        assert_eq!(
            (authentication.max, authentication.window),
            (20, std::time::Duration::from_secs(5)),
            "authentication is 20 per 5 seconds"
        );
        let confirmation = config
            .rate_limit
            .limits
            .get("account_confirmation_resend")
            .copied()
            .unwrap();
        assert_eq!(
            (confirmation.max, confirmation.window),
            (5, std::time::Duration::from_secs(8_600)),
            "account_confirmation_resend is 5 per 8600 seconds"
        );
        assert_eq!(config.rate_limit.limits.len(), 9, "exactly nine buckets");
    }

    /// `RATE_LIMITS` overrides entries of the default table and adds
    /// buckets to it; the untouched defaults stay.
    #[test]
    fn reads_custom_rate_limits() {
        let config = config_with(&[(
            RATE_LIMITS_VAR,
            "authentication=20:5,app_account_creation=5:1800,search=2:2",
        )]);
        assert_eq!(
            config.rate_limit.limits.len(),
            9,
            "no bucket left the table"
        );
        assert_eq!(
            config.rate_limit.limits.get("search").copied().unwrap().max,
            2,
            "the override replaces the default"
        );
        assert_eq!(
            config
                .rate_limit
                .limits
                .get("relations_actions")
                .copied()
                .unwrap()
                .max,
            10,
            "an untouched bucket keeps its default"
        );
    }

    /// A malformed `RATE_LIMITS` entry refuses startup, and the error
    /// names the variable and the reason.
    #[test]
    fn invalid_rate_limits_is_an_error() {
        let err = try_config_with(&[(RATE_LIMITS_VAR, "authentication=20")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidRateLimits(
                "entry 'authentication=20' is not max:window_secs".to_owned()
            ),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid RATE_LIMITS: entry 'authentication=20' is not max:window_secs",
            "message names the variable and the reason"
        );
    }

    /// The two new toggles parse the shared boolean spellings, and a blank
    /// value keeps the default.
    #[test]
    fn rate_limit_toggles_parse_booleans() {
        for var in [RATE_LIMITS_ENABLED_VAR, TRUST_PROXY_HEADERS_VAR] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
            ] {
                let config = config_with(&[(var, value)]);
                assert_eq!(
                    toggle_of(&config, var),
                    want,
                    "{var}={value:?} means {want}"
                );
            }
            let default = if var == RATE_LIMITS_ENABLED_VAR {
                DEFAULT_RATE_LIMITS_ENABLED
            } else {
                DEFAULT_TRUST_PROXY_HEADERS
            };
            let config = config_with(&[(var, "")]);
            assert_eq!(
                toggle_of(&config, var),
                default,
                "empty {var} keeps the default"
            );
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBoolean(var), "error for {var}");
        }
    }

    /// The named toggle's value on this config.
    fn toggle_of(config: &Config, var: &str) -> bool {
        if var == RATE_LIMITS_ENABLED_VAR {
            config.rate_limit.enabled.get()
        } else {
            config.rate_limit.trust_proxy_headers.get()
        }
    }

    /// `Debug` shows the limiter settings; nothing here is secret material.
    #[test]
    fn debug_shows_the_rate_limit_settings() {
        let config = config_with(&[
            (RATE_LIMITS_ENABLED_VAR, "false"),
            (TRUST_PROXY_HEADERS_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("enabled: Toggle(false)"),
            "the enablement is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("trust_proxy_headers: Toggle(true)"),
            "the proxy trust is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("limits:"),
            "the bucket table is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("\"authentication\": RateLimit"),
            "and it shows the buckets by name: {debug}"
        );
    }

    /// The test configuration runs with the limiter off, so a suite never
    /// trips its own budget, and the table still carries the defaults.
    #[test]
    fn for_tests_disables_rate_limiting() {
        let config = Config::for_tests();
        assert!(
            !config.rate_limit.enabled.get(),
            "tests answer unthrottled unless they read the var themselves"
        );
        assert_eq!(
            config.rate_limit.limits,
            rate_limit::default_limits(),
            "the table keeps the defaults for tests that opt in"
        );
        assert!(
            !config.rate_limit.trust_proxy_headers.get(),
            "tests are unproxied by default"
        );
    }

    /// The restricted areas: nothing by default, a comma list read trimmed
    /// and lowercased with repeats folded, an empty value restricting
    /// nothing, and an unknown area refusing startup rather than leaving a
    /// profile silently open.
    #[test]
    fn restrict_unauthenticated_defaults_open_and_reads_its_areas() {
        let open = config_with(&[]);
        assert!(
            open.restrict_unauthenticated.is_empty(),
            "a default instance restricts nothing"
        );
        for area in RESTRICT_UNAUTHENTICATED_AREAS {
            assert!(!open.restricts(area), "`{area}` is open by default");
        }
        let config = config_with(&[(
            RESTRICT_UNAUTHENTICATED_VAR,
            " PROFILES_LOCAL , profiles_remote, timelines_local, profiles_local ",
        )]);
        assert!(config.restricts("profiles_local"), "the half is closed");
        assert!(config.restricts("profiles_remote"), "and so is its twin");
        assert!(
            config.restricts("timelines_local"),
            "as is a timelines half"
        );
        assert!(
            !config.restricts("activities_local"),
            "an area the variable never named stays open"
        );
        assert!(
            !config.restricts("no-such-area"),
            "an unknown name restricts nothing"
        );
        assert_eq!(
            config.restrict_unauthenticated.len(),
            3,
            "the repeat folds and the spacing is trimmed: {:?}",
            config.restrict_unauthenticated
        );
        assert!(
            !config_with(&[(RESTRICT_UNAUTHENTICATED_VAR, "")]).restricts("profiles_local"),
            "an empty value restricts nothing"
        );
    }

    /// An area outside [`RESTRICT_UNAUTHENTICATED_AREAS`] is a startup
    /// failure naming the word it could not place.
    #[test]
    fn an_unknown_restricted_area_is_an_error() {
        let err = try_config_with(&[(RESTRICT_UNAUTHENTICATED_VAR, "profiles_everywhere")])
            .expect_err("an unknown area is refused");
        assert_eq!(
            err,
            ConfigError::InvalidRestrictUnauthenticated("profiles_everywhere".to_owned()),
            "the error carries the offending name"
        );
        assert_eq!(
            err.to_string(),
            "invalid RESTRICT_UNAUTHENTICATED: `profiles_everywhere` is not one of \
             timelines_local, timelines_federated, profiles_local, profiles_remote, \
             activities_local, activities_remote",
            "the wording names the variable and the areas it takes"
        );
        assert!(err.source().is_none(), "there is no underlying error");
    }

    /// `Debug` shows the restricted areas: they name reads, not secrets.
    #[test]
    fn debug_shows_the_restricted_areas() {
        let debug = format!(
            "{:?}",
            config_with(&[(RESTRICT_UNAUTHENTICATED_VAR, "profiles_local")])
        );
        assert!(
            debug.contains("restrict_unauthenticated"),
            "the setting is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("profiles_local"),
            "and it shows which area is closed: {debug}"
        );
    }
}
