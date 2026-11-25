//! [`Config`] itself: the application's boot-time settings, one field per
//! setting (or per group of settings that belong together), each
//! documented by the environment variable it comes from.
//!
//! Its readers live in the sibling modules, assembled by
//! [`Config::from_lookup`]; the
//! redacting [`Debug`](std::fmt::Debug) impl lives in
//! [`debug`](crate::config::debug).

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

use crate::extract::multipart::UploadLimits;

use super::{
    federation_types::{FetchSettings, MrfSettings, RateLimitConfig, Toggle},
    frontend::StaticSettings,
    integrations::{MediaProxyConfig, OauthProvider, SmtpTls, TranslationConfig, VapidConfig},
    protocols::EnabledProtocols,
};

#[cfg(doc)]
use super::vars::RESTRICT_UNAUTHENTICATED_AREAS;

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
    /// `/notice/{id}` can hang its path straight onto it. (The default
    /// avatar, banner, thumbnail and mascot are not frontend files: the
    /// backend serves them under [`media_base_url`](Self::media_base_url).)
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
    /// The media proxy settings, from the `MEDIA_PROXY_*` variables
    /// (see [`crate::services::media_proxy`]).
    pub media_proxy: MediaProxyConfig,
    /// The admin-managed static content: the instance documents, from the
    /// `INSTANCE_DOCUMENT*` variables
    /// (see [`crate::api::admin::apps_docs`]).
    pub statics: StaticSettings,
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
    /// The protocols this instance runs, from `PROTOCOLS`, in
    /// [`ProtocolId::ALL`](crate::protocols::ProtocolId::ALL) order.
    pub protocols: EnabledProtocols,
}
