//! The public description of this instance and the endpoints hung under
//! `/instance` (`docs/ARCHITECTURE.md` decision 1).
//!
//! `GET /api/v1/instance` and `GET /api/v2/instance` serve the document —
//! identity, limits, statistics and rules — while `peers`, `rules`,
//! `domain_blocks`, `translation_languages`, `activity` and
//! `extended_description` each serve one of its sub-resources.
//!
//! The reference is Pleroma's `InstanceController` and `InstanceView`
//! (`lib/pleroma/web/mastodon_api/controllers/instance_controller.ex`,
//! `lib/pleroma/web/mastodon_api/views/instance_view.ex`), whose `show`,
//! `show2`, `peers`, `rules`, `domain_blocks` and `translation_languages`
//! actions these handlers mirror; `activity` and `extended_description`
//! are Mastodon API completeness endpoints Pleroma does not have. Where
//! the ticket's shape and Pleroma 2.10.2's defaults meet, the ticket wins;
//! values Pleroma reads from its runtime config take this instance's
//! [`Config`](crate::config::Config) where it has an equivalent setting and
//! Pleroma's shipped default otherwise — the same rule the `NodeInfo`
//! document follows ([`crate::api::nodeinfo`]). The `features` and
//! `restrict_unauthenticated` lists come from
//! [`crate::services::instance_meta`], the builder `NodeInfo` already
//! shares, so the two endpoints never disagree about what this instance
//! can do — and `peers` asks the same builder whether unauthenticated
//! callers may see remote instances at all.
//!
//! None of these endpoints needs authentication — Pleroma's controller
//! `plug`s `skip_auth` — and none takes parameters. Two of the reads cost
//! more than a query: `show`'s `stats` block aggregates over large tables and
//! is cached in memory for [`STATS_TTL`], and `translation_languages` asks a
//! third-party provider for its language list and is cached for
//! [`LANGUAGES_TTL`] — both in the [`TimedCache`] slot of the same
//! [`crate::state::AppState`]. Rules, the contact account and the peer list
//! are read fresh on every request and answer the moment they change.
//!
//! The three moderation reads share one source, the [MRF
//! configuration](crate::federation::mrf) the pipeline itself runs: the
//! `federation` block publishes the policy names and their lists,
//! `domain_blocks` renders the domain lists as Mastodon domain blocks, and
//! `peers` keeps the rejected hosts out. All three answer through the
//! operator's transparency settings — `MRF_TRANSPARENCY` switches the
//! publishing off, and `MRF_TRANSPARENCY_EXCLUSIONS` hides hosts from what
//! is published while they stay blocked.
//!
//! The module is split by concern: `handlers` holds the endpoint functions,
//! `v1` the `/api/v1/instance` document, `v2` the `/api/v2/instance`
//! document, `pleroma` the extension block both documents carry, and
//! `domain_blocks` the domain-block and extended-description views. This
//! file keeps the shared constants, the cached statistics type, the SQL and
//! the two routers.
//!
//! [`TimedCache`]: crate::util::cache::TimedCache
//! [`LANGUAGES_TTL`]: crate::services::translate::LANGUAGES_TTL

use axum::{Router, routing::get};
use chrono::TimeDelta;
use serde::Serialize;

use crate::{state::AppState, util::cache::TimedCache};

mod domain_blocks;
mod handlers;
mod pleroma;
#[cfg(test)]
mod test_support;
mod v1;
mod v2;

use handlers::{
    show_activity, show_domain_blocks, show_extended_description, show_instance, show_instance_v2,
    show_peers, show_rules, show_translation_languages,
};

/// How long the `stats` block may be served from the in-memory cache.
///
/// Pleroma's `Pleroma.Stats` `GenServer` recalculates on the same
/// 60-second interval (`lib/pleroma/stats.ex`).
pub const STATS_TTL: TimeDelta = TimeDelta::seconds(60);

/// The [`crate::state::AppState`] slot caching this endpoint's
/// statistics; `Default` builds it with [`STATS_TTL`].
pub type InstanceStatsCache = TimedCache<InstanceStats>;

impl Default for TimedCache<InstanceStats> {
    /// An empty stats cache with the endpoint's [`STATS_TTL`] window, so
    /// every [`AppState`] constructor writes the same `Arc::default()`.
    fn default() -> Self {
        Self::new(STATS_TTL)
    }
}

/// The Mastodon API level this endpoint claims compatibility with
/// (Pleroma's `@mastodon_api_level`, `instance_view.ex`).
const MASTODON_API_LEVEL: &str = "2.7.2";
/// The Pleroma release whose HTTP interface this server replicates
/// (decision 1), kept in the version's compatibility comment.
const PLEROMA_COMPATIBILITY_VERSION: &str = "2.10.2";
/// Path appended to [`Config::frontend_base_url`] for the instance
/// thumbnail (Pleroma's `[:instance, :instance_thumbnail]`, whose default
/// is an image under the static root; wired-be serves API only, so the
/// frontend ships the file and the URL is advertised whether or not it
/// sits behind it).
///
/// [`Config::frontend_base_url`]: crate::config::Config::frontend_base_url
const THUMBNAIL_PATH: &str = "/images/thumbnail.jpg";
/// Characters one poll option may hold (Pleroma's `[:instance,
/// :poll_limits]` `max_option_chars`).
const MAX_POLL_OPTION_CHARS: usize = 200;
/// Seconds a poll may run for at least; 0 means "no minimum" (Pleroma's
/// `min_expiration`).
const MIN_POLL_EXPIRATION_SECS: usize = 0;
/// Seconds a poll may run for at most: a year (Pleroma's `max_expiration`
/// of `365 * 24 * 60 * 60`).
const MAX_POLL_EXPIRATION_SECS: usize = 365 * 24 * 60 * 60;
/// Bytes an uploaded avatar may hold (Pleroma's `avatar_upload_limit`).
const AVATAR_UPLOAD_LIMIT: usize = 2_000_000;
/// Bytes an uploaded banner may hold (Pleroma's `banner_upload_limit`).
const BANNER_UPLOAD_LIMIT: usize = 4_000_000;
/// Bytes a profile background may hold (Pleroma's
/// `background_upload_limit`).
const BACKGROUND_UPLOAD_LIMIT: usize = 4_000_000;
/// Characters the instance description (and a profile note) may hold
/// (Pleroma's `[:instance, :description_limit]`).
const DESCRIPTION_LIMIT: usize = 5_000;
/// Characters Mastodon reserves per URL when counting a status against
/// the limit; Pleroma's API documents no link shortening, so this is
/// Mastodon 2.7.2's shipped value.
const CHARACTERS_RESERVED_PER_URL: usize = 23;
/// Pixels one uploaded image may hold in total (Mastodon's
/// `image_matrix_limit`).
const IMAGE_MATRIX_LIMIT: usize = 16_777_216;
/// Pixels one uploaded video frame may hold in total (Mastodon's
/// `video_matrix_limit`).
const VIDEO_MATRIX_LIMIT: usize = 2_304_000;
/// Frames per second an uploaded video may run at (Mastodon's
/// `video_frame_rate_limit`).
const VIDEO_FRAME_RATE_LIMIT: usize = 60;
/// Tags one account may feature (Mastodon 2.7.2's shipped value).
const MAX_FEATURED_TAGS: usize = 10;
/// Fields one local account may set (Pleroma's `max_account_fields`).
const MAX_ACCOUNT_FIELDS: usize = 10;
/// Fields a remote account may carry before they are dropped (Pleroma's
/// `max_remote_account_fields`).
const MAX_REMOTE_ACCOUNT_FIELDS: usize = 20;
/// Characters a profile field name may hold (Pleroma's
/// `account_field_name_length`).
const ACCOUNT_FIELD_NAME_LENGTH: usize = 512;
/// Characters a profile field value may hold (Pleroma's
/// `account_field_value_length`).
const ACCOUNT_FIELD_VALUE_LENGTH: usize = 2048;
/// Source formats a status may be written in (the three
/// [`crate::util::format`] renders; Pleroma's list names bbcode and
/// Misskey markdown too, which this server has no renderer for).
const POST_FORMATS: [&str; 3] = ["text/plain", "text/html", "text/markdown"];
/// Attachment types the upload endpoint accepts: media types are sniffed
/// from the bytes, so the configuration block names Pleroma's catch-all
/// (`InstanceView.configuration`, `instance_view.ex`).
const SUPPORTED_MIME_TYPES: [&str; 1] = ["application/octet-stream"];
/// Monthly active users reported while no activity counter exists.
const MONTHLY_ACTIVE_USERS: usize = 0;
/// The public source repository of this server, reported as the v2
/// document's `source_url` (Pleroma's `Pleroma.Application.repository()`).
const SOURCE_URL: &str = "https://github.com/TheStranjer/wired-be";
/// Statuses one account may pin (Mastodon's shipped limit; Pleroma's
/// config default is `1`, but this block follows Mastodon's numbers the
/// way [`MAX_FEATURED_TAGS`] already does).
const MAX_PINNED_STATUSES: usize = 5;
/// Peer hosts `GET /api/v1/instance/peers` reports at most, the cap the
/// listing answers with even on a far larger table.
const PEER_LIMIT: i64 = 1_000;

/// The three counters of a Pleroma-compatible instance, read in one
/// round trip and cached for [`STATS_TTL`].
///
/// The field names drop the shared `count` postfix for Clippy's
/// `struct_field_names`; `serde` and `sqlx` restore it on the wire and in
/// the query, so both sides still speak Mastodon's
/// `user_count`/`status_count`/`domain_count`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct InstanceStats {
    /// Accounts this instance owns that are still active.
    #[serde(rename = "user_count")]
    #[sqlx(rename = "user_count")]
    pub users: i64,
    /// Live, non-reblog posts this instance owns.
    #[serde(rename = "status_count")]
    #[sqlx(rename = "status_count")]
    pub statuses: i64,
    /// Instances whose users are mirrored here.
    #[serde(rename = "domain_count")]
    #[sqlx(rename = "domain_count")]
    pub domains: i64,
}

/// [`read_stats`](handlers::read_stats): the three counters in one query.
/// `user_count` counts active local accounts, `status_count` the live
/// non-reblog posts this instance owns (a reblog shares its original's post,
/// so counting both would double it), and `domain_count` the distinct hosts
/// of the remote actors mirrored here. The `user_count` subquery is closed
/// with the shared [`NOT_INSTANCE_ACTOR`] fragment: the instance actor is a
/// local active account and belongs in no public statistic.
///
/// [`NOT_INSTANCE_ACTOR`]: crate::models::user::NOT_INSTANCE_ACTOR
const STATS_SQL_LOCAL: &str = "SELECT
       (SELECT count(*) FROM users
         WHERE local AND is_active";
/// The rest of [`STATS_SQL_LOCAL`], pushed after the instance-actor
/// exclusion (see [`read_stats`](handlers::read_stats)).
const STATS_SQL_REST: &str = ") AS user_count,
       (SELECT count(*) FROM statuses
         WHERE local AND deleted_at IS NULL AND reblog_of_id IS NULL) AS status_count,
       (SELECT count(DISTINCT domain) FROM users
         WHERE NOT local AND domain IS NOT NULL) AS domain_count";

/// The [`Instance`](v1::Instance) `contact_account` row: the first local
/// admin — lowest id, which sorts in creation order (decision 4), among the
/// active admins; the same row the staff lists read the role from.
const FIRST_ADMIN_SQL: &str = "SELECT * FROM users
     WHERE is_admin AND local AND is_active
     ORDER BY id
        LIMIT 1";

/// [`list_peers`](handlers::list_peers): the distinct hosts of the remote
/// actors mirrored here — local rows carry no `domain` (decision 7), so
/// "not null" is "remote". Sorted and capped at [`PEER_LIMIT`] rows;
/// Pleroma reads the same distinct hosts through its `Pleroma.Stats`
/// `GenServer`. The sort is pinned to the `"C"` collation (byte order) so
/// the answer, and which hosts the cap cuts off, does not depend on the
/// database's locale.
const PEERS_SQL: &str = r#"SELECT DISTINCT domain COLLATE "C" AS domain FROM users
      WHERE domain IS NOT NULL
  ORDER BY 1
        LIMIT $1"#;

/// The `/api/v1/instance` router, merged into [`crate::api::v1`]: the
/// instance document and the sub-resources hung beside it.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/instance", get(show_instance))
        .route("/instance/activity", get(show_activity))
        .route("/instance/domain_blocks", get(show_domain_blocks))
        .route(
            "/instance/extended_description",
            get(show_extended_description),
        )
        .route("/instance/peers", get(show_peers))
        .route("/instance/rules", get(show_rules))
        .route(
            "/instance/translation_languages",
            get(show_translation_languages),
        )
}

/// The `/api/v2/instance` router, merged into [`crate::api::v2`]: the
/// reshaped document Pleroma's `show2` serves. The `/instance`
/// sub-resources stay v1-only — Pleroma's router mounts them there and
/// nowhere else.
pub fn v2_router() -> Router<AppState> {
    Router::new().route("/instance", get(show_instance_v2))
}
