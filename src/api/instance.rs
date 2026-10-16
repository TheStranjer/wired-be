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
//! [`Config`] where it has an equivalent setting and Pleroma's shipped
//! default otherwise — the same rule the `NodeInfo` document follows
//! ([`crate::api::nodeinfo`]). The `features` and `restrict_unauthenticated`
//! lists come from [`crate::services::instance_meta`], the builder
//! `NodeInfo` already shares, so the two endpoints never disagree about
//! what this instance can do — and `peers` asks the same builder whether
//! unauthenticated callers may see remote instances at all.
//!
//! None of these endpoints needs authentication — Pleroma's controller
//! `plug`s `skip_auth` — and none takes parameters. Of the `show` reads
//! only the `stats` block is expensive across large tables, so it is
//! cached in memory for [`STATS_TTL`] in the [`TimedCache`] on
//! [`crate::state::AppState`]; rules, the contact account and the peer
//! list are read fresh on every request and answer the moment they
//! change.
//!
//! [`TimedCache`]: crate::util::cache::TimedCache

use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{
    config::{Config, Toggle},
    entities::account::Account,
    error::AppResult,
    models::{
        instance_policy_domain::{self, InstancePolicyDomain},
        instance_rule,
        instance_rule::InstanceRule,
        user::{NOT_INSTANCE_ACTOR, User},
    },
    services::instance_meta,
    state::AppState,
    util::{cache::TimedCache, time::mastodon_time},
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
/// Path appended to [`Config::base_url`] for the instance thumbnail
/// (Pleroma's `[:instance, :instance_thumbnail]`, whose default is an
/// image under the static root; wired-be serves API only, so the URL is
/// advertised whether or not a file sits behind it).
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
/// Whether a translation provider is configured; none is until the
/// translation ticket lands, so `configuration.translation.enabled` and
/// `GET /api/v1/instance/translation_languages` report that (Pleroma's
/// `Pleroma.Language.Translation.configured?()`).
const TRANSLATION_CONFIGURED: bool = false;

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

/// [`read_stats`]: the three counters in one query. `user_count` counts
/// active local accounts, `status_count` the live non-reblog posts this
/// instance owns (a reblog shares its original's post, so counting both
/// would double it), and `domain_count` the distinct hosts of the remote
/// actors mirrored here. The `user_count` subquery is closed with the
/// shared [`NOT_INSTANCE_ACTOR`] fragment: the instance actor is a local
/// active account and belongs in no public statistic.
///
/// [`NOT_INSTANCE_ACTOR`]: crate::models::user::NOT_INSTANCE_ACTOR
const STATS_SQL_LOCAL: &str = "SELECT
       (SELECT count(*) FROM users
         WHERE local AND is_active";
/// The rest of [`STATS_SQL_LOCAL`], pushed after the instance-actor
/// exclusion (see [`read_stats`]).
const STATS_SQL_REST: &str = ") AS user_count,
       (SELECT count(*) FROM statuses
         WHERE local AND deleted_at IS NULL AND reblog_of_id IS NULL) AS status_count,
       (SELECT count(DISTINCT domain) FROM users
         WHERE NOT local AND domain IS NOT NULL) AS domain_count";

/// The [`Instance`] `contact_account` row: the first local admin — lowest
/// id, which sorts in creation order (decision 4), among the active
/// admins; the same row the staff lists read the role from.
const FIRST_ADMIN_SQL: &str = "SELECT * FROM users
     WHERE is_admin AND local AND is_active
     ORDER BY id
        LIMIT 1";

/// [`list_peers`]: the distinct hosts of the remote actors mirrored here
/// — local rows carry no `domain` (decision 7), so "not null" is
/// "remote". Sorted and capped at [`PEER_LIMIT`] rows; Pleroma reads the
/// same distinct hosts through its `Pleroma.Stats` `GenServer`.
const PEERS_SQL: &str = "SELECT DISTINCT domain FROM users
      WHERE domain IS NOT NULL
  ORDER BY domain
        LIMIT $1";

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

/// `GET /api/v1/instance` — the public instance document. No
/// authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn show_instance(State(state): State<AppState>) -> AppResult<Json<Instance>> {
    let now = Utc::now();
    let counters = if let Some(cached) = state.instance_stats_cache.get(now) {
        cached
    } else {
        let fresh = read_stats(&state.db).await?;
        state.instance_stats_cache.insert(now, fresh);
        fresh
    };
    let rules = instance_rule::list(&state.db).await?;
    let contact = read_first_admin(&state.db).await?;
    Ok(Json(Instance::build(
        &state.config,
        &counters,
        &rules,
        contact.as_ref(),
    )))
}

/// The [`Instance`] `stats` counters, read live.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
async fn read_stats(pool: &PgPool) -> AppResult<InstanceStats> {
    let mut builder = QueryBuilder::<Postgres>::new(STATS_SQL_LOCAL);
    let _exclude: &mut _ = builder.push(NOT_INSTANCE_ACTOR);
    let _rest: &mut _ = builder.push(STATS_SQL_REST);
    let stats = builder
        .build_query_as::<InstanceStats>()
        .fetch_one(pool)
        .await?;
    Ok(stats)
}

/// The [`Instance::contact_account`] user row: the first active local
/// admin, `None` when the instance has no admin to point at.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
async fn read_first_admin(pool: &PgPool) -> AppResult<Option<User>> {
    let admin = sqlx::query_as::<_, User>(FIRST_ADMIN_SQL)
        .fetch_optional(pool)
        .await?;
    Ok(admin)
}

/// `GET /api/v2/instance` — the public instance document in Mastodon 4's
/// reshaped form. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn show_instance_v2(State(state): State<AppState>) -> AppResult<Json<InstanceV2>> {
    let rules = instance_rule::list(&state.db).await?;
    let contact = read_first_admin(&state.db).await?;
    Ok(Json(InstanceV2::build(
        &state.config,
        &rules,
        contact.as_ref(),
    )))
}

/// [`show_peers`]: the peer list — distinct `users.domain`,
/// alphabetically, at most [`PEER_LIMIT`] hosts — unless the instance
/// restricts unauthenticated reads, in which case nothing is listed.
async fn list_peers(pool: &PgPool) -> AppResult<Vec<String>> {
    let peers = sqlx::query_scalar::<_, String>(PEERS_SQL)
        .bind(PEER_LIMIT)
        .fetch_all(pool)
        .await?;
    Ok(peers)
}

/// Whether the [`instance_meta`] access restrictions hide the federated
/// timeline from anonymous callers — the Akkoma-style gate Pleroma's
/// `peers` action grows behind it: an instance that will not serve remote
/// content to strangers publishes no peer list either. wired-be has no
/// private-instance mode to set, so today this is always `false`.
fn peers_restricted() -> bool {
    instance_meta::restrict_unauthenticated()
        .get("timelines")
        .and_then(|timelines| timelines.get("federated"))
        == Some(&Value::Bool(true))
}

/// `GET /api/v1/instance/peers` — the JSON array of known remote
/// domains. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn show_peers(State(state): State<AppState>) -> AppResult<Json<Vec<String>>> {
    if peers_restricted() {
        return Ok(Json(Vec::new()));
    }
    Ok(Json(list_peers(&state.db).await?))
}

/// `GET /api/v1/instance/rules` — the instance rules, the same entries
/// and order as the v1 document's `rules` block. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn show_rules(State(state): State<AppState>) -> AppResult<Json<Vec<Rule>>> {
    let rules = instance_rule::list(&state.db).await?;
    Ok(Json(rules.iter().map(Rule::from_model).collect()))
}

/// `GET /api/v1/instance/domain_blocks` — the domains rejected or
/// silenced by instance policy, from [`crate::models::instance_policy_domain`].
/// No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn show_domain_blocks(State(state): State<AppState>) -> AppResult<Json<Vec<DomainBlock>>> {
    let policies = instance_policy_domain::list(&state.db).await?;
    Ok(Json(
        policies
            .iter()
            .filter_map(DomainBlock::from_model)
            .collect(),
    ))
}

/// `GET /api/v1/instance/translation_languages` — the language pairs a
/// translation provider offers. No provider is configured until the
/// translation ticket lands, so the document stays the empty object
/// Pleroma's `translation_languages.json` renders then. No
/// authentication.
async fn show_translation_languages() -> Json<Map<String, Value>> {
    Json(Map::new())
}

/// `GET /api/v1/instance/activity` — Mastodon's weekly activity stats.
/// wired-be keeps no activity counters (the same reason the `pleroma`
/// block's `stats.mau` is zero), so the listing is the empty array
/// Mastodon answers before its first counted week. No authentication.
async fn show_activity() -> Json<Vec<Value>> {
    Json(Vec::new())
}

/// `GET /api/v1/instance/extended_description` — Mastodon's long-form
/// instance description. wired-be has one description setting, so the
/// document is that text wrapped in a paragraph, stamped with the
/// current time (there is no stored revision to date from). No
/// authentication.
async fn show_extended_description(State(state): State<AppState>) -> Json<ExtendedDescription> {
    Json(ExtendedDescription::build(&state.config, Utc::now()))
}

/// The `version` string: Mastodon's API level, commented with the Pleroma
/// release replicated and this crate's own version — the same comment
/// style the [`NodeInfo`] software version uses.
///
/// [`NodeInfo`]: crate::api::nodeinfo
fn version_string() -> String {
    format!(
        "{MASTODON_API_LEVEL} (compatible; Pleroma {PLEROMA_COMPATIBILITY_VERSION} wired-be {})",
        env!("CARGO_PKG_VERSION")
    )
}

/// The `urls.streaming_api` value: the websocket origin of this instance
/// — `wss` under `https`, plain `ws` otherwise (Pleroma's
/// `Endpoint.websocket_url()`).
fn streaming_api_url(config: &Config) -> String {
    let scheme = if config.is_https() { "wss" } else { "ws" };
    format!("{scheme}://{}", config.domain())
}

/// The whole instance document, in the key order Pleroma renders it.
#[derive(Debug, Serialize)]
struct Instance {
    /// The domain of [`Config::base_url`], without scheme or port default.
    uri: String,
    /// Instance display name (`INSTANCE_NAME`).
    title: String,
    /// Instance description (`INSTANCE_DESCRIPTION`); wired-be has no
    /// separate short one, so both keys carry it (Pleroma's two config
    /// keys collapse into one setting).
    short_description: String,
    /// The same description again, where Mastodon puts the long form.
    description: String,
    /// Admin contact email (`CONTACT_EMAIL`), empty when unconfigured
    /// (Pleroma answers `null`; the ticket spells the empty string).
    email: String,
    /// The compatibility version line ([`version_string`]).
    version: String,
    /// The streaming endpoint group.
    urls: Urls,
    /// The live counters ([`InstanceStats`]).
    stats: InstanceStats,
    /// Instance thumbnail URL.
    thumbnail: String,
    /// Content language codes (`INSTANCE_LANGUAGES`).
    languages: Vec<String>,
    /// Whether anyone may register (`REGISTRATIONS_OPEN`).
    registrations: Toggle,
    /// Whether registration needs approval; wired-be has no approval
    /// mode yet, so `false` until the registration-modes ticket updates
    /// it (Pleroma's `account_approval_required` default).
    approval_required: bool,
    /// Whether registration invites exist (`INVITES_ENABLED`).
    invites_enabled: bool,
    /// The account clients should contact the staff through: the first
    /// local admin, or JSON `null` when there is none (Pleroma resolves
    /// its `contact_username` config key the same way, missing user
    /// included).
    contact_account: Option<Account>,
    /// The instance rules, lowest priority first ([`Instance::build`]).
    rules: Vec<Rule>,
    /// Status length (`MAX_STATUS_CHARS`), Mastodon-era spelling.
    max_toot_chars: usize,
    /// Attachment limit per status (`MAX_MEDIA_ATTACHMENTS`).
    max_media_attachments: usize,
    /// The poll limits this instance enforces (Pleroma's extra block,
    /// beside [`Configuration`]'s Mastodon-shaped one).
    poll_limits: PollLimits,
    /// Bytes one uploaded file may hold (`UPLOAD_LIMIT_BYTES`).
    upload_limit: usize,
    /// Bytes an uploaded avatar may hold.
    avatar_upload_limit: usize,
    /// Bytes a profile background may hold.
    background_upload_limit: usize,
    /// Bytes an uploaded banner may hold.
    banner_upload_limit: usize,
    /// Characters the instance description may hold.
    description_limit: usize,
    /// The Mastodon client-configuration block.
    configuration: Configuration,
    /// Pleroma's extension block.
    pleroma: PleromaBlock,
}

impl Instance {
    /// The document for the instance `config` serves, with the cached
    /// `stats` counters, the stored `rules` and `contact` as the account
    /// to advertise.
    ///
    /// Pure (decision 3): every value comes from one of the four inputs
    /// or a documented constant.
    fn build(
        config: &Config,
        stats: &InstanceStats,
        rules: &[InstanceRule],
        contact: Option<&User>,
    ) -> Self {
        Self {
            uri: config.domain().to_owned(),
            title: config.instance_name.clone(),
            short_description: config.instance_description.clone(),
            description: config.instance_description.clone(),
            email: config.contact_email.clone().unwrap_or_default(),
            version: version_string(),
            urls: Urls {
                streaming_api: streaming_api_url(config),
            },
            stats: *stats,
            thumbnail: format!("{}{THUMBNAIL_PATH}", config.base_url),
            languages: config.instance_languages.clone(),
            registrations: config.registrations_open,
            approval_required: false,
            invites_enabled: config.invites_enabled.get(),
            contact_account: contact.map(|user| Account::from_user(user, config)),
            rules: rules.iter().map(Rule::from_model).collect(),
            max_toot_chars: config.max_status_chars,
            max_media_attachments: config.max_media_attachments,
            poll_limits: PollLimits::for_config(config),
            upload_limit: config.upload_limits.max_file_bytes,
            avatar_upload_limit: AVATAR_UPLOAD_LIMIT,
            background_upload_limit: BACKGROUND_UPLOAD_LIMIT,
            banner_upload_limit: BANNER_UPLOAD_LIMIT,
            description_limit: DESCRIPTION_LIMIT,
            configuration: Configuration::for_config(config),
            pleroma: PleromaBlock::pleroma(),
        }
    }
}

/// The `urls` block.
#[derive(Debug, Serialize)]
struct Urls {
    /// The websocket address of the streaming API ([`streaming_api_url`]).
    streaming_api: String,
}

/// One entry of the [`rules`](Instance::rules) list, as Pleroma's
/// `rule.json` renderer writes it.
///
/// The id is the stored rule's key as the hyphenated string clients know
/// (decision 4); the text and hint come from the row unchanged.
#[derive(Debug, Serialize)]
struct Rule {
    /// The rule's primary key.
    id: String,
    /// The rule text.
    text: String,
    /// The explanatory hint, empty when the instance gave none.
    hint: String,
}

impl Rule {
    /// Renders the stored `rule` for this document.
    fn from_model(rule: &InstanceRule) -> Self {
        Self {
            id: rule.id.to_string(),
            text: rule.text.clone(),
            hint: rule.hint.clone(),
        }
    }
}

/// The top-level `poll_limits` block (Pleroma's extra key, beside
/// [`PollsConfiguration`]'s Mastodon-shaped one); its keys keep Pleroma's
/// `snake_case` config names.
#[derive(Debug, Serialize)]
struct PollLimits {
    /// Options one poll may offer (`MAX_POLL_OPTIONS`).
    max_options: usize,
    /// Characters one option may hold.
    max_option_chars: usize,
    /// Shortest poll duration, in seconds.
    min_expiration: usize,
    /// Longest poll duration, in seconds.
    max_expiration: usize,
}

impl PollLimits {
    /// The limits this instance enforces: its configured option count
    /// with Pleroma's shipped values around it.
    const fn for_config(config: &Config) -> Self {
        Self {
            max_options: config.max_poll_options,
            max_option_chars: MAX_POLL_OPTION_CHARS,
            min_expiration: MIN_POLL_EXPIRATION_SECS,
            max_expiration: MAX_POLL_EXPIRATION_SECS,
        }
    }
}

/// The `configuration` block: Mastodon's client limits, in Mastodon's
/// group and key order.
#[derive(Debug, Serialize)]
struct Configuration {
    /// The status limits group.
    statuses: StatusesConfiguration,
    /// The upload limits group.
    media_attachments: MediaAttachmentsConfiguration,
    /// The poll limits group.
    polls: PollsConfiguration,
    /// The account limits group.
    accounts: AccountsConfiguration,
}

impl Configuration {
    /// The block for the limits `config` configures, with the shipped
    /// constants for what wired-be does not make configurable.
    const fn for_config(config: &Config) -> Self {
        Self {
            statuses: StatusesConfiguration::for_config(config),
            media_attachments: MediaAttachmentsConfiguration::for_config(config),
            polls: PollsConfiguration::for_config(config),
            accounts: AccountsConfiguration {
                max_featured_tags: MAX_FEATURED_TAGS,
            },
        }
    }
}

/// The `configuration.statuses` group.
#[derive(Debug, Serialize)]
struct StatusesConfiguration {
    /// Characters per status (`MAX_STATUS_CHARS`).
    max_characters: usize,
    /// Attachments per status (`MAX_MEDIA_ATTACHMENTS`).
    max_media_attachments: usize,
    /// Characters Mastodon subtracts per link before counting.
    characters_reserved_per_url: usize,
}

impl StatusesConfiguration {
    /// The status limits `config` configures, beside the fixed
    /// link reserve.
    const fn for_config(config: &Config) -> Self {
        Self {
            max_characters: config.max_status_chars,
            max_media_attachments: config.max_media_attachments,
            characters_reserved_per_url: CHARACTERS_RESERVED_PER_URL,
        }
    }
}

/// The `configuration.media_attachments` group.
#[derive(Debug, Serialize)]
struct MediaAttachmentsConfiguration {
    /// The attachment types the upload endpoint documents.
    supported_mime_types: &'static [&'static str],
    /// Bytes one image may hold (`UPLOAD_LIMIT_BYTES`).
    image_size_limit: usize,
    /// Bytes one video may hold (`UPLOAD_LIMIT_BYTES`).
    video_size_limit: usize,
    /// Total pixels one image may hold.
    image_matrix_limit: usize,
    /// Total pixels per frame one video may hold.
    video_matrix_limit: usize,
    /// Frames per second one video may run at.
    video_frame_rate_limit: usize,
}

impl MediaAttachmentsConfiguration {
    /// The upload limits `config` configures, beside the fixed
    /// matrix/frame-rate caps.
    const fn for_config(config: &Config) -> Self {
        let upload_limit = config.upload_limits.max_file_bytes;
        Self {
            supported_mime_types: &SUPPORTED_MIME_TYPES,
            image_size_limit: upload_limit,
            video_size_limit: upload_limit,
            image_matrix_limit: IMAGE_MATRIX_LIMIT,
            video_matrix_limit: VIDEO_MATRIX_LIMIT,
            video_frame_rate_limit: VIDEO_FRAME_RATE_LIMIT,
        }
    }
}

/// The `configuration.polls` group — the same four limits as
/// [`PollLimits`], under Mastodon's own key names.
#[derive(Debug, Serialize)]
struct PollsConfiguration {
    /// Options one poll may offer (`MAX_POLL_OPTIONS`).
    max_options: usize,
    /// Characters one option may hold.
    max_characters_per_option: usize,
    /// Shortest poll duration, in seconds.
    min_expiration: usize,
    /// Longest poll duration, in seconds.
    max_expiration: usize,
}

impl PollsConfiguration {
    /// The poll limits `config` configures, beside Pleroma's shipped
    /// option length and duration window.
    const fn for_config(config: &Config) -> Self {
        Self {
            max_options: config.max_poll_options,
            max_characters_per_option: MAX_POLL_OPTION_CHARS,
            min_expiration: MIN_POLL_EXPIRATION_SECS,
            max_expiration: MAX_POLL_EXPIRATION_SECS,
        }
    }
}

/// The `configuration.accounts` group.
#[derive(Debug, Serialize)]
struct AccountsConfiguration {
    /// Tags one account may feature.
    max_featured_tags: usize,
}

/// The `pleroma` extension block: Pleroma's own view of the instance
/// (its `pleroma_configuration`, `instance_view.ex`, restricted to the
/// keys the ticket documents).
#[derive(Debug, Serialize)]
struct PleromaBlock {
    /// The capability metadata block.
    metadata: PleromaMetadata,
    /// The extra counters Pleroma reports here.
    stats: PleromaStats,
    /// The VAPID key Web Push clients would need; no push sender exists,
    /// so JSON `null` (Pleroma answers its configured key or `null`).
    vapid_public_key: Value,
}

impl PleromaBlock {
    /// The block of a plain public instance: the shared capability lists,
    /// no push key, and the zeroed extra counters.
    fn pleroma() -> Self {
        Self {
            metadata: PleromaMetadata::build(),
            stats: PleromaStats {
                mau: MONTHLY_ACTIVE_USERS,
            },
            vapid_public_key: Value::Null,
        }
    }
}

/// The `pleroma.stats` block.
#[derive(Debug, Serialize)]
struct PleromaStats {
    /// Monthly active users; wired-be keeps no activity counter yet, so
    /// a constant zero (the ticket's answer while the counters ticket is
    /// open).
    mau: usize,
}

/// The `pleroma.metadata` block: what this instance can do, in Pleroma's
/// key order.
#[derive(Debug, Serialize)]
struct PleromaMetadata {
    /// Whether a new account needs activation before it works;
    /// wired-be activates on registration (Pleroma's default).
    account_activation_required: bool,
    /// Whether registration must state a birthday; it need not.
    birthday_required: bool,
    /// The age a birthday must show to register; 0 means no minimum.
    birthday_min_age: usize,
    /// Feature flags, shared with `NodeInfo` ([`instance_meta::features`]).
    features: &'static [&'static str],
    /// The federation state block.
    federation: Federation,
    /// Profile-field limits.
    fields_limits: FieldsLimits,
    /// Source formats a status may be written in.
    post_formats: &'static [&'static str],
    /// Which reads unauthenticated callers must not make, shared with
    /// `NodeInfo` ([`instance_meta::restrict_unauthenticated`]).
    restrict_unauthenticated: Value,
    /// The markup allowances behind the scrubbing policy
    /// ([`crate::util::sanitize`]).
    markup: Markup,
}

impl PleromaMetadata {
    /// The metadata of a plain public instance: the shared feature and
    /// access lists, no moderation filters, Pleroma's shipped limits.
    fn build() -> Self {
        Self {
            account_activation_required: false,
            birthday_required: false,
            birthday_min_age: 0,
            features: instance_meta::features(),
            federation: Federation::federating(),
            fields_limits: FieldsLimits::pleroma_defaults(),
            post_formats: &POST_FORMATS,
            restrict_unauthenticated: instance_meta::restrict_unauthenticated(),
            markup: Markup::denying(),
        }
    }
}

/// The `pleroma.metadata.federation` block — the same shape and answer
/// `NodeInfo` reports (a plain federating instance with no MRF).
#[derive(Debug, Serialize)]
struct Federation {
    /// Whether this instance federates.
    enabled: bool,
    /// The moderation rewrite filters in force; this server runs none.
    mrf_policies: Vec<&'static str>,
    /// Instances whose posts are held back; none is configured.
    quarantined_instances: Vec<&'static str>,
    /// Whether the MRF transparency lists exclude anything; with no MRF
    /// there is nothing to exclude.
    exclusions: bool,
}

impl Federation {
    /// A plain federating instance with no moderation filters.
    const fn federating() -> Self {
        Self {
            enabled: true,
            mrf_policies: Vec::new(),
            quarantined_instances: Vec::new(),
            exclusions: false,
        }
    }
}

/// The `pleroma.metadata.fields_limits` block; unlike `NodeInfo`'s
/// camelCase `fieldsLimits`, Pleroma writes this one under its own
/// `snake_case` config names.
#[derive(Debug, Serialize)]
struct FieldsLimits {
    /// Fields one local account may set.
    max_fields: usize,
    /// Fields a remote account may carry before they are dropped.
    max_remote_fields: usize,
    /// Characters a field name may hold.
    name_length: usize,
    /// Characters a field value may hold.
    value_length: usize,
}

impl FieldsLimits {
    /// Pleroma 2.10.2's shipped values; wired-be enforces no stricter
    /// profile-field settings.
    const fn pleroma_defaults() -> Self {
        Self {
            max_fields: MAX_ACCOUNT_FIELDS,
            max_remote_fields: MAX_REMOTE_ACCOUNT_FIELDS,
            name_length: ACCOUNT_FIELD_NAME_LENGTH,
            value_length: ACCOUNT_FIELD_VALUE_LENGTH,
        }
    }
}

/// The `pleroma.metadata.markup` block: the formatting this instance
/// allows, as the ticket documents it. The field names drop Pleroma's
/// `allow_` config prefix for Clippy's `struct_field_names`; `serde`
/// restores it on the wire, so consumers still read `allow_inline_images`
/// and friends.
#[derive(Debug, Serialize)]
struct Markup {
    /// Whether a status may show remote images inline; it may not.
    #[serde(rename = "allow_inline_images")]
    inline_images: Toggle,
    /// Whether heading markup survives; it does not.
    #[serde(rename = "allow_headings")]
    headings: Toggle,
    /// Whether tables survive; they do not.
    #[serde(rename = "allow_tables")]
    tables: Toggle,
    /// Whether font styling survives; it does not.
    #[serde(rename = "allow_fonts")]
    fonts: Toggle,
}

impl Markup {
    /// The markup this instance's sanitizer lets through per the ticket:
    /// none of the four optional allowances.
    const fn denying() -> Self {
        Self {
            inline_images: Toggle::new(false),
            headings: Toggle::new(false),
            tables: Toggle::new(false),
            fonts: Toggle::new(false),
        }
    }
}

/// The `GET /api/v2/instance` document, in the shape Pleroma's
/// `InstanceView.render("show2.json", …)` writes: the same facts as
/// [`Instance`], regrouped the way Mastodon 4 clients read them —
/// `domain`/`source_url`/`usage`/`thumbnail`, nested `registrations` and
/// `contact`, a widened `configuration` — plus Pleroma's own `pleroma`
/// block, unchanged from v1.
#[derive(Debug, Serialize)]
struct InstanceV2 {
    /// The domain of [`Config::base_url`], without scheme or port default.
    domain: String,
    /// Instance display name (`INSTANCE_NAME`).
    title: String,
    /// The compatibility version line ([`version_string`]).
    version: String,
    /// This server's source repository ([`SOURCE_URL`]).
    source_url: &'static str,
    /// Instance description (`INSTANCE_DESCRIPTION`); like the v1
    /// document, the one setting fills both of Pleroma's description
    /// keys, and v2 keeps only the long one.
    description: String,
    /// The user counters block.
    usage: Usage,
    /// The instance thumbnail block.
    thumbnail: Thumbnail,
    /// Content language codes (`INSTANCE_LANGUAGES`).
    languages: Vec<String>,
    /// The widened client-configuration block.
    configuration: ConfigurationV2,
    /// The registration state, nested as Mastodon 4 spells it.
    registrations: RegistrationsV2,
    /// The admin contact pair: email and account.
    contact: Contact,
    /// The instance rules, lowest priority first ([`Rule::from_model`]).
    rules: Vec<Rule>,
    /// Pleroma's extension block, the same [`PleromaBlock`] v1 writes.
    pleroma: PleromaBlock,
}

impl InstanceV2 {
    /// The document for the instance `config` serves, with the stored
    /// `rules` and `contact` as the account to advertise. The `usage`
    /// counter needs no database read while no activity counter exists.
    ///
    /// Pure (decision 3): every value comes from one of the three inputs
    /// or a documented constant.
    fn build(config: &Config, rules: &[InstanceRule], contact: Option<&User>) -> Self {
        Self {
            domain: config.domain().to_owned(),
            title: config.instance_name.clone(),
            version: version_string(),
            source_url: SOURCE_URL,
            description: config.instance_description.clone(),
            usage: Usage::monthly(),
            thumbnail: Thumbnail::for_config(config),
            languages: config.instance_languages.clone(),
            configuration: ConfigurationV2::for_config(config),
            registrations: RegistrationsV2::for_config(config),
            contact: Contact::build(config, contact),
            rules: rules.iter().map(Rule::from_model).collect(),
            pleroma: PleromaBlock::pleroma(),
        }
    }
}

/// The `usage` block.
#[derive(Debug, Serialize)]
struct Usage {
    /// The user counters.
    users: UsageUsers,
}

impl Usage {
    /// The usage block of an instance that counts no activity: the same
    /// zero the `pleroma` block reports as `stats.mau`.
    const fn monthly() -> Self {
        Self {
            users: UsageUsers {
                active_month: MONTHLY_ACTIVE_USERS,
            },
        }
    }
}

/// The `usage.users` block.
#[derive(Debug, Serialize)]
struct UsageUsers {
    /// Users active in the last month; wired-be keeps no activity
    /// counter yet, so a constant zero (the ticket's answer while the
    /// counters ticket is open).
    active_month: usize,
}

/// The `thumbnail` block: Mastodon 4 wraps the URL v1 writes bare.
#[derive(Debug, Serialize)]
struct Thumbnail {
    /// The instance thumbnail URL ([`THUMBNAIL_PATH`] under
    /// [`Config::base_url`]).
    url: String,
}

impl Thumbnail {
    /// The thumbnail block for `config`'s origin.
    fn for_config(config: &Config) -> Self {
        Self {
            url: format!("{}{THUMBNAIL_PATH}", config.base_url),
        }
    }
}

/// The v2 `configuration` block: the three limit groups v1 already
/// writes, beside Mastodon 4's own `urls`, `vapid`, `translation` and
/// the widened `accounts` group.
#[derive(Debug, Serialize)]
struct ConfigurationV2 {
    /// The endpoint group.
    urls: EndpointUrls,
    /// The Web Push keys group.
    vapid: Vapid,
    /// The account limits group, wider than v1's.
    accounts: AccountsConfigurationV2,
    /// The status limits group, as in v1.
    statuses: StatusesConfiguration,
    /// The upload limits group, as in v1.
    media_attachments: MediaAttachmentsConfiguration,
    /// The poll limits group, as in v1.
    polls: PollsConfiguration,
    /// The translation support group.
    translation: TranslationConfiguration,
}

impl ConfigurationV2 {
    /// The block for the limits `config` configures; the shared v1
    /// groups are reused whole, the Mastodon 4-only groups carry the
    /// shipped constants.
    fn for_config(config: &Config) -> Self {
        Self {
            urls: EndpointUrls::for_config(config),
            vapid: Vapid::unsupported(),
            accounts: AccountsConfigurationV2::shipped(),
            statuses: StatusesConfiguration::for_config(config),
            media_attachments: MediaAttachmentsConfiguration::for_config(config),
            polls: PollsConfiguration::for_config(config),
            translation: TranslationConfiguration::unconfigured(),
        }
    }
}

/// The v2 `configuration.urls` group.
#[derive(Debug, Serialize)]
struct EndpointUrls {
    /// The websocket address of the streaming API ([`streaming_api_url`],
    /// under Mastodon 4's own key name).
    streaming: String,
    /// The status page URL; wired-be configures none, so JSON `null`
    /// (Pleroma's `[:instance, :status_page]` default).
    status: Value,
}

impl EndpointUrls {
    /// The endpoint group for `config`'s origin, with no status page.
    fn for_config(config: &Config) -> Self {
        Self {
            streaming: streaming_api_url(config),
            status: Value::Null,
        }
    }
}

/// The v2 `configuration.vapid` group.
#[derive(Debug, Serialize)]
struct Vapid {
    /// The Web Push public key; no push sender exists, so the empty
    /// string clients treat as "no push" (Pleroma answers its configured
    /// key, which ships unset).
    public_key: String,
}

impl Vapid {
    /// The group of an instance with no push sender.
    const fn unsupported() -> Self {
        Self {
            public_key: String::new(),
        }
    }
}

/// The v2 `configuration.accounts` group: the featured-tag limit v1
/// reports, plus the pinned-status one.
#[derive(Debug, Serialize)]
struct AccountsConfigurationV2 {
    /// Tags one account may feature.
    max_featured_tags: usize,
    /// Statuses one account may pin.
    max_pinned_statuses: usize,
}

impl AccountsConfigurationV2 {
    /// The shipped account limits.
    const fn shipped() -> Self {
        Self {
            max_featured_tags: MAX_FEATURED_TAGS,
            max_pinned_statuses: MAX_PINNED_STATUSES,
        }
    }
}

/// The v2 `configuration.translation` group.
#[derive(Debug, Serialize)]
struct TranslationConfiguration {
    /// Whether a translation provider is configured; none is until the
    /// translation ticket lands ([`TRANSLATION_CONFIGURED`]).
    enabled: bool,
}

impl TranslationConfiguration {
    /// The group of an instance with no translation provider.
    const fn unconfigured() -> Self {
        Self {
            enabled: TRANSLATION_CONFIGURED,
        }
    }
}

/// The v2 `registrations` block: the flat v1 pair, nested, plus the
/// invitation fields Mastodon 4 adds (this instance has neither).
#[derive(Debug, Serialize)]
struct RegistrationsV2 {
    /// Whether anyone may register (`REGISTRATIONS_OPEN`).
    enabled: Toggle,
    /// Whether registration needs approval; wired-be has no approval
    /// mode yet, so `false` until the registration-modes ticket updates
    /// it (Pleroma's `account_approval_required` default).
    approval_required: bool,
    /// The text shown on the registration form; `None` renders JSON
    /// `null`, the answer when the instance writes none.
    message: Value,
    /// The external registration URL; wired-be registers in place, so
    /// JSON `null`.
    url: Value,
}

impl RegistrationsV2 {
    /// The block for `config`'s registration state, with the two
    /// never-set extras.
    const fn for_config(config: &Config) -> Self {
        Self {
            enabled: config.registrations_open,
            approval_required: false,
            message: Value::Null,
            url: Value::Null,
        }
    }
}

/// The v2 `contact` block: the two flat v1 keys (`email`,
/// `contact_account`), nested.
#[derive(Debug, Serialize)]
struct Contact {
    /// Admin contact email (`CONTACT_EMAIL`), empty when unconfigured
    /// (the same answer [`Instance::email`] gives).
    email: String,
    /// The account clients should contact the staff through: the first
    /// local admin, or JSON `null` when there is none.
    account: Option<Account>,
}

impl Contact {
    /// The block for `config`'s email and the `account` to advertise.
    fn build(config: &Config, account: Option<&User>) -> Self {
        Self {
            email: config.contact_email.clone().unwrap_or_default(),
            account: account.map(|user| Account::from_user(user, config)),
        }
    }
}

/// One entry of `GET /api/v1/instance/domain_blocks`: a domain this
/// instance's own policy restricts, in the shape Mastodon's domain-block
/// listing documents.
///
/// The `digest` lets a client check the block without learning the
/// domain (Mastodon hashes it the same way); this instance serves the
/// domain in the clear anyway, so both are in the entry.
#[derive(Debug, Serialize)]
struct DomainBlock {
    /// The blocked host.
    domain: String,
    /// Lowercase hex `SHA-256` of [`domain`](Self::domain).
    digest: String,
    /// How the block reads to clients: `suspend` or `silence`
    /// ([`DomainBlock::severity_of`]).
    severity: &'static str,
    /// Why the domain is blocked, empty when the instance gave no
    /// reason.
    comment: String,
}

impl DomainBlock {
    /// Renders the stored policy `row`, `None` when its action maps to
    /// no Mastodon severity — Pleroma's renderer likewise drops the
    /// policy groups without a severity.
    fn from_model(row: &InstancePolicyDomain) -> Option<Self> {
        let severity = Self::severity_of(&row.action)?;
        Some(Self {
            digest: sha256_hex(&row.domain),
            domain: row.domain.clone(),
            severity,
            comment: row.reason.clone(),
        })
    }

    /// The Mastodon severity of a policy `action`: a rejected domain is
    /// suspended; silenced and media-scrubbed domains both stay visible
    /// under `silence` (the ticket's mapping; Pleroma likewise folds its
    /// soft blocks into one severity).
    fn severity_of(action: &str) -> Option<&'static str> {
        match action {
            "reject" => Some("suspend"),
            "silence" | "media_removal" => Some("silence"),
            _ => None,
        }
    }
}

/// `GET /api/v1/instance/extended_description`: Mastodon's long-form
/// description, stored as rich text with a revision date. wired-be has
/// the one [`Config::instance_description`] and no revisions, so
/// [`ExtendedDescription::build`] wraps it in a paragraph and stamps it
/// with the request time.
#[derive(Debug, Serialize)]
struct ExtendedDescription {
    /// When the description last changed ([`ExtendedDescription::build`]).
    updated_at: String,
    /// The description as an HTML paragraph.
    content: String,
}

impl ExtendedDescription {
    /// The document for `config`'s description, stamped `now` in
    /// Mastodon's time format (decision 5).
    fn build(config: &Config, now: DateTime<Utc>) -> Self {
        Self {
            updated_at: mastodon_time(&now),
            content: format!("<p>{}</p>", config.instance_description),
        }
    }
}

/// The lowercase hex `SHA-256` of `text` — Mastodon's domain-block
/// digest: Pleroma's renderer writes it as `:crypto.hash(:sha256, host)`
/// encoded with `Base.encode16(case: :lower)`.
fn sha256_hex(text: &str) -> String {
    let mut hex = String::with_capacity(Sha256::output_size() * 2);
    for &byte in &Sha256::digest(text.as_bytes()) {
        hex.push(hex_digit(byte >> 4));
        hex.push(hex_digit(byte & 0x0F));
    }
    hex
}

/// One lowercase hex digit for `nibble` (which must be below `16`).
fn hex_digit(nibble: u8) -> char {
    if nibble < 10 {
        char::from(b'0' + nibble)
    } else {
        char::from(b'a' + (nibble - 10))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone as _};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    /// A fixed instant for the rules fixture rows; the timestamps are not
    /// part of the document, so any valid one works.
    fn stamped() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    /// A stored rule with `id` text `n` for rendering tests.
    fn rule(id: &str, text: &str, hint: &str, priority: i32) -> InstanceRule {
        InstanceRule {
            id: Uuid::parse_str(id).unwrap(),
            text: text.to_owned(),
            hint: hint.to_owned(),
            priority,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// The empty-instance document on the test config: zeroed stats, no
    /// rules, no admin.
    fn empty_document() -> Value {
        let config = Config::for_tests();
        let stats = InstanceStats {
            users: 0,
            statuses: 0,
            domains: 0,
        };
        serde_json::to_value(Instance::build(&config, &stats, &[], None)).unwrap()
    }

    /// The sorted object keys of `value`, for key-set assertions.
    fn keys_of(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .expect("a document is an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn the_version_names_the_api_level_pleroma_and_the_crate() {
        assert_eq!(
            version_string(),
            format!(
                "2.7.2 (compatible; Pleroma 2.10.2 wired-be {})",
                env!("CARGO_PKG_VERSION")
            ),
            "the ticket's compatibility comment with the real crate version"
        );
    }

    #[test]
    fn the_streaming_url_follows_the_base_url_scheme() {
        let https = Config::for_tests();
        assert_eq!(
            streaming_api_url(&https),
            "wss://test.example",
            "https gains the secure websocket scheme"
        );
        let mut http = Config::for_tests();
        http.base_url = "http://localhost:4000".to_owned();
        assert_eq!(
            streaming_api_url(&http),
            "ws://localhost:4000",
            "http keeps plain ws, domain with port"
        );
    }

    #[test]
    fn the_document_holds_the_ticket_top_level_key_set() {
        let document = empty_document();
        assert_eq!(
            keys_of(&document),
            vec![
                "approval_required",
                "avatar_upload_limit",
                "background_upload_limit",
                "banner_upload_limit",
                "configuration",
                "contact_account",
                "description",
                "description_limit",
                "email",
                "invites_enabled",
                "languages",
                "max_media_attachments",
                "max_toot_chars",
                "pleroma",
                "poll_limits",
                "registrations",
                "rules",
                "short_description",
                "stats",
                "thumbnail",
                "title",
                "upload_limit",
                "uri",
                "urls",
                "version",
            ],
            "the ticket's top-level key set"
        );
        assert_eq!(
            keys_of(&document["stats"]),
            vec!["domain_count", "status_count", "user_count"],
            "the stats block"
        );
        assert_eq!(
            keys_of(&document["poll_limits"]),
            vec![
                "max_expiration",
                "max_option_chars",
                "max_options",
                "min_expiration"
            ],
            "the poll limits block"
        );
    }

    #[test]
    fn the_document_holds_the_ticket_nested_key_sets() {
        let document = empty_document();
        assert_eq!(
            keys_of(&document["configuration"]),
            vec!["accounts", "media_attachments", "polls", "statuses"],
            "the configuration groups"
        );
        assert_eq!(
            keys_of(&document["configuration"]["media_attachments"]),
            vec![
                "image_matrix_limit",
                "image_size_limit",
                "supported_mime_types",
                "video_frame_rate_limit",
                "video_matrix_limit",
                "video_size_limit",
            ],
            "the media limits"
        );
        assert_eq!(
            keys_of(&document["pleroma"]),
            vec!["metadata", "stats", "vapid_public_key"],
            "the pleroma block"
        );
        assert_eq!(
            keys_of(&document["pleroma"]["metadata"]),
            vec![
                "account_activation_required",
                "birthday_min_age",
                "birthday_required",
                "features",
                "federation",
                "fields_limits",
                "markup",
                "post_formats",
                "restrict_unauthenticated",
            ],
            "the pleroma metadata block"
        );
        assert_eq!(
            keys_of(&document["pleroma"]["metadata"]["markup"]),
            vec![
                "allow_fonts",
                "allow_headings",
                "allow_inline_images",
                "allow_tables",
            ],
            "the markup keys keep Pleroma's allow_ prefix on the wire"
        );
    }

    #[test]
    fn the_empty_document_holds_the_instance_identity() {
        let document = empty_document();
        assert_eq!(document["uri"], "test.example", "the config domain");
        assert_eq!(document["title"], "Wired", "the config name");
        assert_eq!(
            document["short_description"], document["description"],
            "both descriptions are the one config setting"
        );
        assert_eq!(document["email"], "", "an unconfigured email is empty");
        assert_eq!(
            document["thumbnail"],
            "https://test.example/images/thumbnail.jpg"
        );
        assert_eq!(document["languages"], json!(["en"]), "the config languages");
        assert_eq!(document["registrations"], true, "open by default");
        assert_eq!(
            document["approval_required"], false,
            "until the registration ticket"
        );
        assert_eq!(
            document["invites_enabled"], false,
            "INVITES_ENABLED is off by default"
        );
        assert_eq!(document["contact_account"], Value::Null, "no admin fixture");
        assert_eq!(document["rules"], json!([]), "no rules stored");
        assert_eq!(
            document["stats"],
            json!({"user_count": 0, "status_count": 0, "domain_count": 0}),
            "the zeroed counters"
        );
    }

    /// `invites_enabled` is the `INVITES_ENABLED` setting, on both sides
    /// of its default.
    #[test]
    fn invites_enabled_mirrors_the_config() {
        let stats = InstanceStats {
            users: 0,
            statuses: 0,
            domains: 0,
        };
        let mut config = Config::for_tests();
        config.invites_enabled = Toggle::new(true);
        let on = serde_json::to_value(Instance::build(&config, &stats, &[], None)).unwrap();
        assert_eq!(
            on["invites_enabled"], true,
            "INVITES_ENABLED=true is advertised"
        );
        config.invites_enabled = Toggle::new(false);
        let off = serde_json::to_value(Instance::build(&config, &stats, &[], None)).unwrap();
        assert_eq!(off["invites_enabled"], false, "and the off state stays off");
    }

    #[test]
    fn the_limit_blocks_hold_their_defaults() {
        let document = empty_document();
        assert_eq!(document["max_toot_chars"], 5000, "the config default");
        assert_eq!(document["max_media_attachments"], 4, "the config default");
        assert_eq!(document["upload_limit"], 16_000_000, "the config default");
        assert_eq!(
            document["avatar_upload_limit"], 2_000_000,
            "Pleroma's default"
        );
        assert_eq!(
            document["background_upload_limit"], 4_000_000,
            "Pleroma's default"
        );
        assert_eq!(
            document["banner_upload_limit"], 4_000_000,
            "Pleroma's default"
        );
        assert_eq!(document["description_limit"], 5000, "Pleroma's default");
        assert_eq!(
            document["poll_limits"],
            json!({"max_options": 20, "max_option_chars": 200, "min_expiration": 0, "max_expiration": 31_536_000}),
            "the configured option count with Pleroma's window"
        );
        assert_eq!(
            document["configuration"]["statuses"],
            json!({"max_characters": 5000, "max_media_attachments": 4, "characters_reserved_per_url": 23}),
            "the status limits mirror the config"
        );
        assert_eq!(
            document["configuration"]["media_attachments"],
            json!({
                "supported_mime_types": ["application/octet-stream"],
                "image_size_limit": 16_000_000,
                "video_size_limit": 16_000_000,
                "image_matrix_limit": 16_777_216,
                "video_matrix_limit": 2_304_000,
                "video_frame_rate_limit": 60,
            }),
            "the upload limits mirror the config"
        );
        assert_eq!(
            document["configuration"]["polls"],
            json!({"max_options": 20, "max_characters_per_option": 200, "min_expiration": 0, "max_expiration": 31_536_000}),
            "the poll limits appear under Mastodon's names too"
        );
        assert_eq!(
            document["configuration"]["accounts"],
            json!({"max_featured_tags": 10}),
            "Mastodon's shipped featured-tag limit"
        );
    }

    #[test]
    fn the_pleroma_block_holds_the_instance_metadata() {
        let document = empty_document();
        assert_eq!(document["pleroma"]["stats"], json!({"mau": 0}));
        assert_eq!(
            document["pleroma"]["vapid_public_key"],
            Value::Null,
            "no push sender exists"
        );
        let metadata = &document["pleroma"]["metadata"];
        assert_eq!(metadata["account_activation_required"], false);
        assert_eq!(metadata["birthday_required"], false);
        assert_eq!(metadata["birthday_min_age"], 0);
        assert_eq!(
            metadata["features"],
            json!(instance_meta::features()),
            "the shared feature list"
        );
        assert_eq!(
            metadata["restrict_unauthenticated"],
            instance_meta::restrict_unauthenticated(),
            "the shared access restrictions"
        );
        assert_eq!(
            metadata["federation"],
            json!({"enabled": true, "mrf_policies": [], "quarantined_instances": [], "exclusions": false}),
            "plain federation, no filters"
        );
        assert_eq!(
            metadata["fields_limits"],
            json!({"max_fields": 10, "max_remote_fields": 20, "name_length": 512, "value_length": 2048}),
            "Pleroma's shipped field limits, snake_cased"
        );
        assert_eq!(
            metadata["post_formats"],
            json!(["text/plain", "text/html", "text/markdown"]),
            "the three formats this server renders"
        );
        assert_eq!(
            metadata["markup"],
            json!({
                "allow_inline_images": false,
                "allow_headings": false,
                "allow_tables": false,
                "allow_fonts": false,
            }),
            "the ticket's markup answer"
        );
    }

    #[test]
    fn limits_and_identity_follow_the_config() {
        let mut config = Config::for_tests();
        config.instance_name = "Snatter".to_owned();
        config.instance_description = "Where we all chatter".to_owned();
        config.contact_email = Some("admin@example.org".to_owned());
        config.instance_languages = vec!["en".to_owned(), "de".to_owned()];
        config.base_url = "http://snatter.example:4000".to_owned();
        config.max_status_chars = 300;
        config.max_media_attachments = 6;
        config.max_poll_options = 12;
        config.upload_limits.max_file_bytes = 1_048_576;
        config.registrations_open = Toggle::new(false);
        let stats = InstanceStats {
            users: 7,
            statuses: 11,
            domains: 3,
        };
        let document = serde_json::to_value(Instance::build(&config, &stats, &[], None)).unwrap();
        assert_eq!(
            document["uri"], "snatter.example:4000",
            "the domain with port"
        );
        assert_eq!(document["title"], "Snatter");
        assert_eq!(document["description"], "Where we all chatter");
        assert_eq!(document["email"], "admin@example.org");
        assert_eq!(document["languages"], json!(["en", "de"]));
        assert_eq!(
            document["urls"]["streaming_api"],
            "ws://snatter.example:4000"
        );
        assert_eq!(
            document["thumbnail"],
            "http://snatter.example:4000/images/thumbnail.jpg"
        );
        assert_eq!(document["registrations"], false);
        assert_eq!(document["max_toot_chars"], 300);
        assert_eq!(document["max_media_attachments"], 6);
        assert_eq!(document["poll_limits"]["max_options"], 12);
        assert_eq!(document["upload_limit"], 1_048_576);
        assert_eq!(document["configuration"]["statuses"]["max_characters"], 300);
        assert_eq!(
            document["configuration"]["media_attachments"]["image_size_limit"],
            1_048_576
        );
        assert_eq!(
            document["stats"],
            json!({"user_count": 7, "status_count": 11, "domain_count": 3})
        );
    }

    #[test]
    fn rules_render_id_text_and_hint() {
        let rules = vec![
            rule(
                "01934567-89ab-7cde-f012-3456789abcde",
                "Be nice",
                "say hi",
                0,
            ),
            rule("01934567-89ab-7cde-f012-3456789abcdf", "No spam", "", 1),
        ];
        let document = serde_json::to_value(empty_document_with_rules(&rules)).unwrap();
        assert_eq!(
            document["rules"],
            json!([
                {"id": "01934567-89ab-7cde-f012-3456789abcde", "text": "Be nice", "hint": "say hi"},
                {"id": "01934567-89ab-7cde-f012-3456789abcdf", "text": "No spam", "hint": ""},
            ]),
            "ids arrive as the hyphenated lowercase strings (decision 4)"
        );
    }

    /// The empty-instance document carrying `rules`.
    fn empty_document_with_rules(rules: &[InstanceRule]) -> Instance {
        let config = Config::for_tests();
        let stats = InstanceStats {
            users: 0,
            statuses: 0,
            domains: 0,
        };
        Instance::build(&config, &stats, rules, None)
    }

    /// The empty-instance v2 document on the test config: zeroed usage,
    /// no rules, no contact.
    fn empty_document_v2() -> Value {
        let config = Config::for_tests();
        serde_json::to_value(InstanceV2::build(&config, &[], None)).unwrap()
    }

    #[test]
    fn the_v2_document_holds_the_ticket_key_sets() {
        let document = empty_document_v2();
        assert_eq!(
            keys_of(&document),
            vec![
                "configuration",
                "contact",
                "description",
                "domain",
                "languages",
                "pleroma",
                "registrations",
                "rules",
                "source_url",
                "thumbnail",
                "title",
                "usage",
                "version",
            ],
            "the ticket's v2 top-level key set"
        );
        assert_eq!(
            keys_of(&document["configuration"]),
            vec![
                "accounts",
                "media_attachments",
                "polls",
                "statuses",
                "translation",
                "urls",
                "vapid",
            ],
            "the v2 configuration groups"
        );
        assert_eq!(
            keys_of(&document["registrations"]),
            vec!["approval_required", "enabled", "message", "url"],
            "the nested registration block"
        );
        assert_eq!(
            keys_of(&document["contact"]),
            vec!["account", "email"],
            "the nested contact block"
        );
        assert_eq!(
            keys_of(&document["usage"]),
            vec!["users"],
            "the usage block counts users"
        );
        assert_eq!(
            keys_of(&document["usage"]["users"]),
            vec!["active_month"],
            "the monthly-active counter"
        );
        assert_eq!(
            keys_of(&document["thumbnail"]),
            vec!["url"],
            "v2 wraps the thumbnail URL"
        );
    }

    #[test]
    fn the_v2_document_holds_the_instance_identity() {
        let document = empty_document_v2();
        assert_eq!(document["domain"], "test.example", "the config domain");
        assert_eq!(document["title"], "Wired", "the config name");
        assert_eq!(
            document["description"], "A Pleroma-compatible instance",
            "the config description"
        );
        assert_eq!(
            document["source_url"], SOURCE_URL,
            "the shipped repository URL"
        );
        assert_eq!(
            document["source_url"],
            "https://github.com/TheStranjer/wired-be"
        );
        assert_eq!(
            document["thumbnail"]["url"], "https://test.example/images/thumbnail.jpg",
            "the wrapped thumbnail URL"
        );
        assert_eq!(document["languages"], json!(["en"]), "the config languages");
        assert_eq!(
            document["usage"]["users"]["active_month"], 0,
            "no activity counter exists yet"
        );
        assert_eq!(document["rules"], json!([]), "no rules stored");
    }

    #[test]
    fn the_v2_configuration_groups_hold_their_defaults() {
        let document = empty_document_v2();
        assert_eq!(
            document["configuration"]["urls"],
            json!({"streaming": "wss://test.example", "status": null}),
            "the streaming origin and no status page"
        );
        assert_eq!(
            document["configuration"]["vapid"],
            json!({"public_key": ""}),
            "no push sender exists"
        );
        assert_eq!(
            document["configuration"]["accounts"],
            json!({"max_featured_tags": 10, "max_pinned_statuses": 5}),
            "Mastodon's shipped account limits"
        );
        assert_eq!(
            document["configuration"]["statuses"],
            json!({"max_characters": 5000, "max_media_attachments": 4, "characters_reserved_per_url": 23}),
            "the status limits mirror the config"
        );
        assert_eq!(
            document["configuration"]["translation"],
            json!({"enabled": false}),
            "no translation provider is configured"
        );
        let v1 = empty_document();
        for group in ["media_attachments", "polls"] {
            assert_eq!(
                document["configuration"][group], v1["configuration"][group],
                "the {group} group is the shared v1 builder, unchanged"
            );
        }
        assert_eq!(
            document["registrations"],
            json!({"enabled": true, "approval_required": false, "message": null, "url": null}),
            "the config toggle nested with the two never-set extras"
        );
        assert_eq!(
            document["contact"],
            json!({"email": "", "account": null}),
            "an unconfigured email is empty, no admin fixture"
        );
        assert_eq!(
            document["pleroma"], v1["pleroma"],
            "the pleroma block is the shared v1 builder, unchanged"
        );
    }

    #[test]
    fn the_v2_document_follows_the_config() {
        let mut config = Config::for_tests();
        config.instance_name = "Snatter".to_owned();
        config.instance_description = "Where we all chatter".to_owned();
        config.contact_email = Some("admin@example.org".to_owned());
        config.instance_languages = vec!["en".to_owned(), "de".to_owned()];
        config.base_url = "http://snatter.example:4000".to_owned();
        config.max_status_chars = 300;
        config.max_media_attachments = 6;
        config.max_poll_options = 12;
        config.upload_limits.max_file_bytes = 1_048_576;
        config.registrations_open = Toggle::new(false);
        let document = serde_json::to_value(InstanceV2::build(&config, &[], None)).unwrap();
        assert_eq!(
            document["domain"], "snatter.example:4000",
            "the domain with port"
        );
        assert_eq!(document["title"], "Snatter");
        assert_eq!(document["description"], "Where we all chatter");
        assert_eq!(document["languages"], json!(["en", "de"]));
        assert_eq!(
            document["thumbnail"]["url"],
            "http://snatter.example:4000/images/thumbnail.jpg"
        );
        assert_eq!(
            document["configuration"]["urls"]["streaming"],
            "ws://snatter.example:4000"
        );
        assert_eq!(
            document["configuration"]["statuses"]["max_characters"], 300,
            "the shared status group mirrors the config"
        );
        assert_eq!(
            document["configuration"]["media_attachments"]["image_size_limit"], 1_048_576,
            "the shared media group mirrors the upload limit"
        );
        assert_eq!(
            document["configuration"]["polls"]["max_options"], 12,
            "the shared poll group mirrors the option count"
        );
        assert_eq!(
            document["registrations"]["enabled"], false,
            "REGISTRATIONS_OPEN=false is advertised"
        );
        assert_eq!(document["contact"]["email"], "admin@example.org");
    }

    #[test]
    fn domain_block_severities_follow_the_ticket_mapping() {
        let policy = |action: &str| InstancePolicyDomain {
            domain: "blocked.example".to_owned(),
            action: action.to_owned(),
            reason: "bots".to_owned(),
            inserted_at: stamped(),
        };
        assert_eq!(
            DomainBlock::severity_of("reject"),
            Some("suspend"),
            "a rejected domain reads as suspended"
        );
        assert_eq!(
            DomainBlock::severity_of("silence"),
            Some("silence"),
            "a silenced domain keeps its name"
        );
        assert_eq!(
            DomainBlock::severity_of("media_removal"),
            Some("silence"),
            "media scrubbing reads as silence too"
        );
        assert_eq!(
            DomainBlock::severity_of("defederate"),
            None,
            "an unmapped action renders no entry"
        );
        assert!(
            DomainBlock::from_model(&policy("defederate")).is_none(),
            "the renderer drops unmapped rows"
        );
    }

    #[test]
    fn a_domain_block_holds_the_domain_digest_severity_and_comment() {
        let row = InstancePolicyDomain {
            domain: "blocked.example".to_owned(),
            action: "reject".to_owned(),
            reason: "coordinated abuse".to_owned(),
            inserted_at: stamped(),
        };
        let block = DomainBlock::from_model(&row).expect("reject maps to a severity");
        let rendered = serde_json::to_value(&block).unwrap();
        assert_eq!(
            rendered,
            json!({
                "domain": "blocked.example",
                "digest": sha256_hex("blocked.example"),
                "severity": "suspend",
                "comment": "coordinated abuse",
            }),
            "the entry shape the ticket documents"
        );
        let empty = InstancePolicyDomain {
            reason: String::new(),
            ..row
        };
        let unnamed = DomainBlock::from_model(&empty).expect("reject maps");
        assert_eq!(unnamed.comment, "", "an ungiven reason is the empty string");
    }

    #[test]
    fn the_sha256_hex_digest_is_lowercase_hex() {
        // The published SHA-256 of the ASCII string "abc".
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "the digest matches the SHA-256 test vector"
        );
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "even the empty string hashes"
        );
    }

    #[test]
    fn the_extended_description_wraps_the_config_text() {
        let config = Config::for_tests();
        let now = stamped();
        let description = ExtendedDescription::build(&config, now);
        let rendered = serde_json::to_value(&description).unwrap();
        assert_eq!(
            rendered,
            json!({
                "updated_at": "2026-10-11T12:00:00.000Z",
                "content": "<p>A Pleroma-compatible instance</p>",
            }),
            "the paragraph-wrapped INSTANCE_DESCRIPTION stamped in Mastodon time"
        );
    }
}
