//! Every `DEFAULT_*` constant: the documented default of each environment
//! variable [`Config`] reads.
//!
//! The module also keeps the [`MIN_MEDIA_PROXY_SECRET_CHARS`] floor, and the
//! shared pieces of the [`Config::for_tests`] fallback built from the
//! constants.

#[cfg(any(test, feature = "integration-tests"))]
use std::path::PathBuf;

#[cfg(doc)]
use super::Config;

#[cfg(any(test, feature = "integration-tests"))]
use crate::{extract::multipart::UploadLimits, rate_limit};

#[cfg(any(test, feature = "integration-tests"))]
use super::{
    federation_types::{FetchSettings, RateLimitConfig, Toggle},
    frontend::{FrontendSettings, InstanceDocumentSettings, StaticSettings},
    integrations::MediaProxyConfig,
    parse::comma_list,
};

/// Listen address used when `BIND_ADDR` is unset.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:4000";
/// Instance name used when `INSTANCE_NAME` is unset.
pub const DEFAULT_INSTANCE_NAME: &str = "Wired";
/// Description used when `INSTANCE_DESCRIPTION` is unset. Wired is the
/// subject; Pleroma is named only as what the server is compatible with.
pub const DEFAULT_INSTANCE_DESCRIPTION: &str = "A Wired instance, compatible with Pleroma clients";
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
pub(crate) const TEST_UPLOAD_LIMITS: UploadLimits = UploadLimits {
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
/// Directory the instance documents live in when `INSTANCE_DOCS_DIR` is
/// unset, relative to the working directory.
///
/// Pleroma keeps them under `[:instance, :static_dir]` (its default
/// `instance/static`, `Pleroma.Web.InstanceDocument`), which the
/// `InstanceStatic` plug serves at the top level; the same shape with one
/// setting folded in.
pub const DEFAULT_INSTANCE_DOCS_DIR: &str = "./instance";
/// Document names used when `INSTANCE_DOCUMENT_NAMES` is unset: the two
/// documents Pleroma's `Pleroma.Web.InstanceDocument` map knows, spelled
/// as the `<name>.html` storage this server writes them under.
pub const DEFAULT_INSTANCE_DOCUMENT_NAMES: &str = "terms-of-service,instance-panel";
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
/// Media-proxy cache ledger size used when `MEDIA_PROXY_CACHE_LIMIT` is
/// unset: 100 000 served URLs kept, the oldest pruned past that.
pub const DEFAULT_MEDIA_PROXY_CACHE_LIMIT: usize = 100_000;
/// Directory admin-installed frontends live in when `FRONTENDS_DIR` is
/// unset: `./frontends`, relative to the working directory.
///
/// The same shape as [`DEFAULT_UPLOADS_DIR`]'s and
/// [`DEFAULT_EMOJI_DIR`]'s defaults, and Pleroma's
/// `Pleroma.Frontend.dir()` (`lib/pleroma/frontend.ex`) with its
/// `static_dir` parent step folded into the one setting.
pub const DEFAULT_FRONTENDS_DIR: &str = "./frontends";
/// Frontend list used when `FRONTENDS_AVAILABLE` is unset: nothing is
/// installable, so `GET /api/v1/pleroma/admin/frontends` answers `[]`.
pub const DEFAULT_FRONTENDS_AVAILABLE: &str = "[]";
/// The media-proxy defaults together as a [`MediaProxyConfig`], spelled
/// once for [`Config::test_defaults`]'s literal; the real read of the same
/// group is the `media_proxy_settings` function.
#[cfg(any(test, feature = "integration-tests"))]
pub(crate) const TEST_MEDIA_PROXY: MediaProxyConfig = MediaProxyConfig {
    enabled: DEFAULT_MEDIA_PROXY_ENABLED,
    secret: None,
    base_url: None,
    max_bytes: DEFAULT_MEDIA_PROXY_MAX_BYTES,
    blacklist: Vec::new(),
    cache_limit: DEFAULT_MEDIA_PROXY_CACHE_LIMIT,
    purge_url: None,
};
/// The rate-limiter test defaults as a [`RateLimitConfig`], spelled once
/// for [`Config::test_defaults`]'s literal like [`TEST_UPLOAD_LIMITS`] and
/// [`TEST_MEDIA_PROXY`]; a function rather than a `const`, because
/// [`default_limits`](crate::rate_limit::default_limits) builds its
/// [`std::collections::BTreeMap`] at runtime.
#[cfg(any(test, feature = "integration-tests"))]
pub(crate) fn test_rate_limit() -> RateLimitConfig {
    RateLimitConfig {
        enabled: Toggle::new(false),
        limits: rate_limit::default_limits(),
        trust_proxy_headers: Toggle::new(DEFAULT_TRUST_PROXY_HEADERS),
        trusted_proxy_hops: DEFAULT_TRUSTED_PROXY_HOPS,
    }
}
/// The admin-managed static content defaults — no installable frontend,
/// the stock document pair — as a [`StaticSettings`], spelled once for
/// [`Config::test_defaults`]'s literal like [`TEST_MEDIA_PROXY`]; a
/// function rather than a `const` because [`PathBuf::from`] is not a
/// `const fn`.
#[cfg(any(test, feature = "integration-tests"))]
pub(crate) fn test_statics() -> StaticSettings {
    StaticSettings {
        frontends: FrontendSettings {
            dir: PathBuf::from(DEFAULT_FRONTENDS_DIR),
            available: Vec::new(),
        },
        documents: InstanceDocumentSettings {
            dir: PathBuf::from(DEFAULT_INSTANCE_DOCS_DIR),
            names: comma_list(DEFAULT_INSTANCE_DOCUMENT_NAMES),
        },
    }
}
/// The outbound-fetch defaults as a [`FetchSettings`] with the SSRF guard
/// off, spelled once for [`Config::test_defaults`]'s literal like
/// [`test_statics`]: the suites talk to local mini-servers.
#[cfg(any(test, feature = "integration-tests"))]
pub(crate) fn test_fetch() -> FetchSettings {
    FetchSettings {
        http_allow_private_hosts: true,
        ..FetchSettings::default()
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
/// Proxy hops used when `TRUSTED_PROXY_HOPS` is unset: one reverse proxy
/// in front of the server, so the rightmost `X-Forwarded-For` entry is the
/// client it saw.
pub const DEFAULT_TRUSTED_PROXY_HOPS: usize = 1;
/// `DATABASE_URL` used by [`Config::for_tests`]: a placeholder URL that
/// points at nothing; integration tests get their real pool from the fixture
/// in [`crate::test_db`].
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_DATABASE_URL: &str = "postgres://localhost/wired_test";
/// `BASE_URL` used by [`Config::for_tests`].
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_BASE_URL: &str = "https://test.example";

#[cfg(test)]
mod tests {
    use super::*;

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
}
