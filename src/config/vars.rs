//! The environment variable names [`Config`](crate::config::Config) reads,
//! one `*_VAR` constant per setting plus the
//! [`RESTRICT_UNAUTHENTICATED_AREAS`] vocabulary, documented by what the
//! variable holds.

#[cfg(doc)]
use super::{defaults::MIN_MEDIA_PROXY_SECRET_CHARS, integrations::OauthProvider};
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
/// Environment variable holding the directory the instance documents
/// (static HTML pages like the terms of service) are stored in, each
/// written as `<name>.html` (see [`crate::api::admin::apps_docs`]).
pub const INSTANCE_DOCS_DIR_VAR: &str = "INSTANCE_DOCS_DIR";
/// Environment variable holding the comma-separated list of instance
/// document names the admin document routes accept and the JSON terms of
/// service reads (see [`crate::api::admin::apps_docs`]).
pub const INSTANCE_DOCUMENT_NAMES_VAR: &str = "INSTANCE_DOCUMENT_NAMES";
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
/// Environment variable holding how many served URLs the media proxy
/// cache table keeps at most; the oldest are pruned past it.
pub const MEDIA_PROXY_CACHE_LIMIT_VAR: &str = "MEDIA_PROXY_CACHE_LIMIT";
/// Environment variable holding the origin `PURGE` invalidation requests
/// for proxied media are sent to; unset means no purge requests are made.
pub const MEDIA_PROXY_PURGE_URL_VAR: &str = "MEDIA_PROXY_PURGE_URL";
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
/// Environment variable holding whether the `X-Forwarded-For` header may
/// name the client for rate limiting.
pub const TRUST_PROXY_HEADERS_VAR: &str = "TRUST_PROXY_HEADERS";
/// Environment variable holding how many reverse proxies sit in front of
/// this server, i.e. how many `X-Forwarded-For` entries from the right
/// were appended by trusted hops.
pub const TRUSTED_PROXY_HOPS_VAR: &str = "TRUSTED_PROXY_HOPS";
