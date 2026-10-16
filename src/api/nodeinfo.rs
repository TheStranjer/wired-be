//! `NodeInfo`: the `/.well-known/nodeinfo` discovery document and the
//! `/nodeinfo/{version}` statistics (`docs/ARCHITECTURE.md` decision 1).
//!
//! This router is merged at the top level of [`crate::app`], not under
//! `/api`: both addresses are fixed by the `NodeInfo` specification
//! (<https://nodeinfo.diaspora.software>). Pleroma serves them from
//! `lib/pleroma/web/nodeinfo/nodeinfo_controller.ex` and builds the
//! document in `lib/pleroma/web/nodeinfo/nodeinfo.ex`; its
//! `TrailingFormatPlug` (`lib/pleroma/web/plugs/trailing_format_plug.ex`)
//! answers `/nodeinfo/2.0.json` as `/nodeinfo/2.0`, so both spellings of
//! both versions are the same document here.
//!
//! The document reports what this instance is: software identity (name
//! `"pleroma"`, because Pleroma-compatible clients sniff it, with the real
//! crate version in the compatibility comment), federation protocols,
//! registration state, live usage counters read from the database, and the
//! Pleroma-compatible `metadata` block. Where the ticket's shape and
//! Pleroma 2.10.2's defaults meet, the ticket wins; values Pleroma reads
//! from its runtime config take this instance's [`Config`] where it has an
//! equivalent setting and Pleroma's shipped default otherwise. The
//! `features` and `restrict_unauthenticated` lists come from
//! [`crate::services::instance_meta`], the builder the `/api/v1/instance`
//! endpoints share.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{
    config::{Config, Toggle},
    error::{AppError, AppResult},
    models::user::NOT_INSTANCE_ACTOR,
    services::instance_meta,
    state::AppState,
};

/// The `rel` announcing the `NodeInfo` 2.0 schema (Pleroma's `schemas/2`).
const REL_20: &str = "http://nodeinfo.diaspora.software/ns/schema/2.0";
/// The `rel` announcing the `NodeInfo` 2.1 schema.
const REL_21: &str = "http://nodeinfo.diaspora.software/ns/schema/2.1";
/// `software.name` (Pleroma's `Pleroma.Application.name()` downcased):
/// clients key their Pleroma-extension behaviour off exactly this string.
const SOFTWARE_NAME: &str = "pleroma";
/// The Pleroma release whose HTTP interface this server replicates
/// (decision 1), kept in the software version's compatibility comment.
const PLEROMA_COMPATIBILITY_VERSION: &str = "2.10.2";
/// Source repository, reported as `software.repository` and
/// `software.homepage` by the 2.1 document.
const REPOSITORY_URL: &str = "https://github.com/TheStranjer/wired-be";
/// 404 body of `/nodeinfo/{version}` for a schema this server does not
/// serve, Pleroma's `render_error(:not_found, …)` message.
const UNHANDLED_VERSION: &str = "Nodeinfo schema version not handled";
/// `Content-Type` of the 2.0 document: the JSON media type with the
/// schema's own URI as `profile` parameter (Pleroma's
/// `put_resp_header("content-type", …)`).
const CONTENT_TYPE_20: &str =
    "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.0#\"";
/// The same for the 2.1 document.
const CONTENT_TYPE_21: &str =
    "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.1#\"";
/// The only protocol this server federates with.
const PROTOCOLS: [&str; 1] = ["activitypub"];
/// `metadata.postFormats`: the source formats a status may be written in
/// (the three [`crate::util::format`] renders); Pleroma's list names
/// bbcode and Misskey markdown too, which this server has no renderer for.
const POST_FORMATS: [&str; 3] = ["text/plain", "text/html", "text/markdown"];
/// Characters one poll option may hold (Pleroma's
/// `[:instance, :poll_limits]` `max_option_chars`).
const MAX_POLL_OPTION_CHARS: usize = 200;
/// Seconds a poll may run for at least; 0 means "no minimum"
/// (Pleroma's `min_expiration`).
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
/// Profile fields one account may set (Pleroma's `max_account_fields`).
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

/// The top-level `NodeInfo` router, merged by [`crate::app`].
///
/// Both mount points live outside `/api`: `/.well-known/nodeinfo` is the
/// discovery address the `NodeInfo` spec fixes, and `/nodeinfo/{version}` is
/// the address the discovery document hands out (all four spellings the
/// spec and Pleroma's links use — `2.0`, `2.0.json`, `2.1`, `2.1.json` —
/// arrive at the one route).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/.well-known/nodeinfo", get(schemas))
        .route("/nodeinfo/{version}", get(nodeinfo))
}

/// `GET /.well-known/nodeinfo` — the discovery document: the two schema
/// links pointing at this instance's document paths (Pleroma's
/// `NodeinfoController.schemas/2`). No authentication, no parameters, no
/// failure: the document only names this instance's own URLs.
async fn schemas(State(state): State<AppState>) -> Json<Discovery> {
    Json(Discovery::for_base_url(&state.config.base_url))
}

/// `GET /nodeinfo/2.0` (`…/2.0.json`, `…/2.1`, `…/2.1.json`) — the
/// instance statistics document, answered with the schema's URI as `profile`
/// `Content-Type` parameter. No authentication: `NodeInfo` is public.
///
/// # Errors
///
/// [`AppError::NotFoundMessage`] when the path names a schema version this
/// server does not serve (Pleroma's 404 "Nodeinfo schema version not
/// handled"). Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn nodeinfo(
    State(state): State<AppState>,
    Path(segment): Path<String>,
) -> AppResult<Response> {
    let version = SchemaVersion::from_segment(&segment)
        .ok_or_else(|| AppError::NotFoundMessage(UNHANDLED_VERSION.to_owned()))?;
    let users = read_user_counts(&state.db).await?;
    let local_posts = read_local_post_count(&state.db).await?;
    let staff_accounts = read_staff_accounts(&state.db).await?;
    let document = NodeInfo::build(version, &state.config, users, local_posts, staff_accounts);
    document_response(version, &document)
}

/// The two `NodeInfo` schema versions this server documents, the values the
/// `version` field and the `profile` content parameter carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchemaVersion {
    /// `NodeInfo` 2.0: the plain document.
    V20,
    /// `NodeInfo` 2.1: the plain document plus the software links (Pleroma's
    /// "2.1 just adds a repository field under software", extended with
    /// `homepage` per the same schema).
    V21,
}

impl SchemaVersion {
    /// The version a `/nodeinfo/{version}` path segment names. Pleroma's
    /// `TrailingFormatPlug` strips the `.json` format suffix on `/nodeinfo`
    /// paths before routing, so the suffixed and bare spellings are the
    /// same document; anything else — `2.2`, `latest`, a second suffix —
    /// names no schema this server serves.
    fn from_segment(segment: &str) -> Option<Self> {
        match segment.strip_suffix(".json").unwrap_or(segment) {
            "2.0" => Some(Self::V20),
            "2.1" => Some(Self::V21),
            _ => None,
        }
    }

    /// The `version` field of the document.
    const fn as_str(self) -> &'static str {
        match self {
            Self::V20 => "2.0",
            Self::V21 => "2.1",
        }
    }

    /// The full `Content-Type` of the document: JSON carrying this
    /// schema's URI as `profile` parameter.
    const fn content_type(self) -> &'static str {
        match self {
            Self::V20 => CONTENT_TYPE_20,
            Self::V21 => CONTENT_TYPE_21,
        }
    }

    /// Whether the document carries the software `repository` and
    /// `homepage` links, which the 2.0 schema has no keys for.
    const fn has_software_links(self) -> bool {
        matches!(self, Self::V21)
    }
}

/// The discovery document of `/.well-known/nodeinfo`.
#[derive(Debug, Serialize)]
struct Discovery {
    /// The schema links, 2.0 first, exactly as Pleroma orders them.
    links: Vec<SchemaLink>,
}

/// One `links` entry of a [`Discovery`] document.
#[derive(Debug, Serialize)]
struct SchemaLink {
    /// The schema URI this document is written in.
    rel: &'static str,
    /// This instance's document for that schema.
    href: String,
}

impl Discovery {
    /// The document naming the two schema documents of the instance served
    /// at `base_url` (which never ends in a slash).
    fn for_base_url(base_url: &str) -> Self {
        Self {
            links: vec![
                SchemaLink {
                    rel: REL_20,
                    href: format!("{base_url}/nodeinfo/2.0.json"),
                },
                SchemaLink {
                    rel: REL_21,
                    href: format!("{base_url}/nodeinfo/2.1.json"),
                },
            ],
        }
    }
}

/// The whole `NodeInfo` document. `NodeInfo` has no fixed key order, but the
/// schema's own example and Pleroma both lead with `version`, so the
/// fields are declared in that order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeInfo {
    /// The schema this document is written in.
    version: &'static str,
    /// Which software this instance runs.
    software: Software,
    /// The federation protocols this instance speaks.
    protocols: &'static [&'static str],
    /// Third-party sign-in and sharing services; this server integrates
    /// with none (Pleroma's own answer is the same two empty lists).
    services: Services,
    /// Whether anyone may register here, from `REGISTRATIONS_OPEN`.
    open_registrations: Toggle,
    /// The live counters.
    usage: Usage,
    /// The Pleroma-compatible metadata block.
    metadata: Metadata,
}

impl NodeInfo {
    /// The document for `version`, describing the instance `config` serves
    /// with the database-read `users` counts, `local_posts` total and
    /// `staff_accounts` actor ids.
    fn build(
        version: SchemaVersion,
        config: &Config,
        users: UserCounts,
        local_posts: i64,
        staff_accounts: Vec<String>,
    ) -> Self {
        Self {
            version: version.as_str(),
            software: Software::for_version(version),
            protocols: &PROTOCOLS,
            services: Services::empty(),
            open_registrations: config.registrations_open,
            usage: Usage { users, local_posts },
            metadata: Metadata::build(config, staff_accounts),
        }
    }
}

/// The `software` block.
#[derive(Debug, Serialize)]
struct Software {
    /// Always `"pleroma"`: the name Pleroma-compatible clients sniff.
    name: &'static str,
    /// The Pleroma version this server's HTTP interface replicates,
    /// commented with the wired-be crate version.
    version: String,
    /// Source repository — only the 2.1 schema has this key.
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'static str>,
    /// Project homepage — only the 2.1 schema has this key.
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<&'static str>,
}

impl Software {
    /// The software block of a `version` document: the 2.1 schema carries
    /// the repository links, the 2.0 schema has no keys for them (Pleroma's
    /// 2.1 document is its 2.0 document with `repository` added).
    fn for_version(version: SchemaVersion) -> Self {
        let with_links = version.has_software_links();
        Self {
            name: SOFTWARE_NAME,
            version: format!(
                "{PLEROMA_COMPATIBILITY_VERSION} (compatible; wired-be {})",
                env!("CARGO_PKG_VERSION")
            ),
            repository: with_links.then_some(REPOSITORY_URL),
            homepage: with_links.then_some(REPOSITORY_URL),
        }
    }
}

/// The `services` block: third-party services clients may plug into.
#[derive(Debug, Serialize)]
struct Services {
    /// Services users can bring their posts from (external posting).
    inbound: Vec<&'static str>,
    /// Services posts can be shared to.
    outbound: Vec<&'static str>,
}

impl Services {
    /// This server integrates with no third-party service.
    const fn empty() -> Self {
        Self {
            inbound: Vec::new(),
            outbound: Vec::new(),
        }
    }
}

/// The `usage` block.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    /// The user counters.
    users: UserCounts,
    /// Posts this instance owns.
    local_posts: i64,
}

/// The `usage.users` block: the row [`read_user_counts`] reads, the field
/// names renamed for the schema.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct UserCounts {
    /// Local accounts still active.
    total: i64,
    /// Of those, the ones seen in the last 30 days.
    active_month: i64,
    /// Of those, the ones seen in the last 180 days.
    active_halfyear: i64,
}

/// The `metadata` block: Pleroma's extensions, in Pleroma's key order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    /// Instance display name (`INSTANCE_NAME`).
    node_name: String,
    /// Instance description (`INSTANCE_DESCRIPTION`).
    node_description: String,
    /// Whether registration alone does not get a user in; wired-be has no
    /// private-instance mode, so `false` (Pleroma's `public` default).
    private: Toggle,
    /// The account-suggestions feature; disabled, because it reports the
    /// `[:instance, :suggestions]` switch this server has no setting for
    /// (Pleroma answers its `false` default the same way, and the
    /// `is_suggested` pool is admin-curated, not an instance feature).
    suggestions: Suggestions,
    /// Actor ids of the admin and moderator accounts.
    staff_accounts: Vec<String>,
    /// The federation state block.
    federation: Federation,
    /// The poll limits this instance enforces.
    poll_limits: PollLimits,
    /// Source formats a status may be written in.
    post_formats: &'static [&'static str],
    /// Upload size limits, in bytes.
    upload_limits: UploadLimits,
    /// Profile-field limits.
    fields_limits: FieldsLimits,
    /// Whether a new account needs an email confirmation before it works;
    /// wired-be activates on registration (Pleroma's
    /// `account_activation_required` default).
    account_activation_required: Toggle,
    /// Whether registration invites exist; they do not here (Pleroma's
    /// `invites_enabled` default).
    invites_enabled: Toggle,
    /// Whether the instance sends mail; no mailer exists, so `false`
    /// (Pleroma's `Pleroma.Emails.Mailer` `enabled` default).
    mailer_enabled: Toggle,
    /// Feature flags, shared with the instance endpoints'
    /// `pleroma.metadata.features` ([`instance_meta::features`]).
    features: &'static [&'static str],
    /// Which reads unauthenticated callers must not make, shared with the
    /// instance endpoints ([`instance_meta::restrict_unauthenticated`]).
    /// Snake-cased on purpose: Pleroma writes this config block out under
    /// its own atom names, not the `NodeInfo` schema's camelCase.
    #[serde(rename = "restrict_unauthenticated")]
    restrict_unauthenticated: Value,
    /// Whether threads from other instances keep only what addresses this
    /// one; Pleroma 2.10 ships `skip_thread_containment: true`.
    skip_thread_containment: Toggle,
    /// The markup allowances behind the scrubbing policy ([`crate::util::sanitize`]).
    markup: Markup,
}

impl Metadata {
    /// The metadata block of the instance `config` serves, with
    /// `staff_accounts` as its staff actor ids.
    fn build(config: &Config, staff_accounts: Vec<String>) -> Self {
        Self {
            node_name: config.instance_name.clone(),
            node_description: config.instance_description.clone(),
            private: Toggle::new(false),
            suggestions: Suggestions {
                enabled: Toggle::new(false),
            },
            staff_accounts,
            federation: Federation::federating(),
            poll_limits: PollLimits::for_config(config),
            post_formats: &POST_FORMATS,
            upload_limits: UploadLimits::for_config(config),
            fields_limits: FieldsLimits::pleroma_defaults(),
            account_activation_required: Toggle::new(false),
            invites_enabled: Toggle::new(false),
            mailer_enabled: Toggle::new(false),
            features: instance_meta::features(),
            restrict_unauthenticated: instance_meta::restrict_unauthenticated(),
            skip_thread_containment: Toggle::new(true),
            markup: Markup::pleroma_defaults(),
        }
    }
}

/// The `metadata.suggestions` block.
#[derive(Debug, Serialize)]
struct Suggestions {
    /// Whether the suggested-accounts endpoints serve anything.
    enabled: Toggle,
}

/// The `metadata.federation` block.
#[derive(Debug, Serialize)]
struct Federation {
    /// Whether this instance federates.
    enabled: bool,
    /// The moderation rewrite filters in force; this server runs none
    /// (Pleroma's `mrf, transparency` answer lists its policy modules).
    mrf_policies: Vec<&'static str>,
    /// Instances whose posts are held back; none is configured.
    quarantined_instances: Vec<String>,
    /// Whether the MRF transparency lists exclude anything; with no MRF
    /// there is nothing to exclude (Pleroma derives this from
    /// `:mrf_simple`).
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

/// The `metadata.pollLimits` block; its keys keep Pleroma's `snake_case`
/// config names, which is what a `NodeInfo` consumer of this block reads.
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
    /// The limits this instance enforces: its configured option count with
    /// Pleroma's shipped values around it.
    const fn for_config(config: &Config) -> Self {
        Self {
            max_options: config.max_poll_options,
            max_option_chars: MAX_POLL_OPTION_CHARS,
            min_expiration: MIN_POLL_EXPIRATION_SECS,
            max_expiration: MAX_POLL_EXPIRATION_SECS,
        }
    }
}

/// The `metadata.uploadLimits` block, in bytes.
#[derive(Debug, Serialize)]
struct UploadLimits {
    /// One attachment (`UPLOAD_LIMIT_BYTES`).
    general: usize,
    /// One avatar.
    avatar: usize,
    /// One banner.
    banner: usize,
    /// One profile background.
    background: usize,
}

impl UploadLimits {
    /// The configured general limit with Pleroma's shipped per-purpose
    /// defaults for the other three (wired-be has no separate settings for
    /// them).
    const fn for_config(config: &Config) -> Self {
        Self {
            general: config.upload_limits.max_file_bytes,
            avatar: AVATAR_UPLOAD_LIMIT,
            banner: BANNER_UPLOAD_LIMIT,
            background: BACKGROUND_UPLOAD_LIMIT,
        }
    }
}

/// The `metadata.fieldsLimits` block (camelCase keys, as Pleroma renders
/// them from its `snake_case` config names).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
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

/// The `metadata.markup` block: the formatting Pleroma's
/// `[:markup]` config allows by default, which [`crate::util::sanitize`]
/// implements to the same effect. The field names drop Pleroma's `allow_`
/// config prefix for Clippy's `struct_field_names`; `serde` restores it on
/// the wire, so consumers still read `allow_inline_images` and friends.
#[derive(Debug, Serialize)]
struct Markup {
    /// Whether a status may show remote images inline (Pleroma's default
    /// yes, because custom emoji are inline images).
    #[serde(rename = "allow_inline_images")]
    inline_images: Toggle,
    /// Whether heading markup survives; it does not.
    #[serde(rename = "allow_headings")]
    headings: Toggle,
    /// Whether tables survive; they do not.
    #[serde(rename = "allow_tables")]
    tables: Toggle,
}

impl Markup {
    /// Pleroma 2.10's shipped `[:markup]` defaults.
    const fn pleroma_defaults() -> Self {
        Self {
            inline_images: Toggle::new(true),
            headings: Toggle::new(false),
            tables: Toggle::new(false),
        }
    }
}

/// The `usage.users` counters: every active local account counts toward
/// `total`, and the two activity windows count the accounts whose
/// `last_active_at` — falling back to their newest post's time, as Pleroma
/// treats "active" — is inside them. The instance actor never counts: the
/// shared [`NOT_INSTANCE_ACTOR`] fragment closes the predicate.
///
/// [`NOT_INSTANCE_ACTOR`]: crate::models::user::NOT_INSTANCE_ACTOR
const USER_COUNTS_SQL: &str = "SELECT
       count(*) AS total,
       count(*) FILTER (
           WHERE coalesce(last_active_at, last_status_at) >= now() - interval '30 days'
       ) AS active_month,
       count(*) FILTER (
           WHERE coalesce(last_active_at, last_status_at) >= now() - interval '180 days'
       ) AS active_halfyear
   FROM users
   WHERE local AND is_active";

/// The `usage.localPosts` counter: posts this instance owns, live (soft
/// deletes excluded) and not reblogs — a reblog shares its original's
/// post, so counting both would double it (Pleroma's `Stats` sums the
/// local `note_count`, which excludes announces the same way).
const LOCAL_POSTS_SQL: &str = "SELECT count(*) FROM statuses
   WHERE local AND deleted_at IS NULL AND reblog_of_id IS NULL";

/// The `metadata.staffAccounts` list: actor ids of the local staff
/// accounts, ordered by id text so the document is stable (Pleroma's
/// `User.all_superusers/0`).
const STAFF_ACCOUNTS_SQL: &str = "SELECT ap_id FROM users
   WHERE (is_admin OR is_moderator) AND local AND is_active
ORDER BY ap_id";

/// The [`usage`](NodeInfo::usage) user counters, read live. The builder
/// exists only to splice the shared [`NOT_INSTANCE_ACTOR`] fragment onto
/// the constant [`USER_COUNTS_SQL`] — no request data enters the text.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn read_user_counts(pool: &PgPool) -> AppResult<UserCounts> {
    let mut builder = QueryBuilder::<Postgres>::new(USER_COUNTS_SQL);
    let _exclude: &mut _ = builder.push(NOT_INSTANCE_ACTOR);
    let counts = builder
        .build_query_as::<UserCounts>()
        .fetch_one(pool)
        .await?;
    Ok(counts)
}

/// The [`usage`](NodeInfo::usage) post counter, read live.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn read_local_post_count(pool: &PgPool) -> AppResult<i64> {
    let (count,) = sqlx::query_as::<_, (i64,)>(LOCAL_POSTS_SQL)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// The [`metadata`](NodeInfo::metadata) staff actor ids, read live.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn read_staff_accounts(pool: &PgPool) -> AppResult<Vec<String>> {
    let accounts = sqlx::query_scalar::<_, String>(STAFF_ACCOUNTS_SQL)
        .fetch_all(pool)
        .await?;
    Ok(accounts)
}

/// The [`NodeInfo`] document as a response with the `profile` typed
/// `Content-Type` its schema requires.
///
/// # Errors
///
/// [`AppError::Internal`] if serialization fails, which these plain
/// structs of database-read values never do.
fn document_response(version: SchemaVersion, document: &NodeInfo) -> AppResult<Response> {
    let body = serde_json::to_vec(document)
        .map_err(|err| AppError::Internal(format!("NodeInfo serialization failed: {err}")))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(version.content_type()),
        )],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// The empty-usage document for `version`, built on the test config.
    fn document(version: SchemaVersion) -> Value {
        let config = Config::for_tests();
        let document = NodeInfo::build(
            version,
            &config,
            UserCounts {
                total: 0,
                active_month: 0,
                active_halfyear: 0,
            },
            0,
            Vec::new(),
        );
        serde_json::to_value(&document).unwrap()
    }

    #[test]
    fn path_segments_name_the_two_schemas_with_or_without_suffix() {
        for (segment, want) in [
            ("2.0", SchemaVersion::V20),
            ("2.0.json", SchemaVersion::V20),
            ("2.1", SchemaVersion::V21),
            ("2.1.json", SchemaVersion::V21),
        ] {
            assert_eq!(
                SchemaVersion::from_segment(segment),
                Some(want),
                "{segment} is the {want:?} document"
            );
        }
    }

    #[test]
    fn other_segments_name_no_served_schema() {
        for segment in [
            "",
            "json",
            ".json",
            "2.0.json.json",
            "2.2",
            "2.0.htm",
            "3.0",
            "latest",
            "2",
        ] {
            assert_eq!(
                SchemaVersion::from_segment(segment),
                None,
                "{segment} names no schema this server serves"
            );
        }
    }

    #[test]
    fn each_version_carries_its_own_profile() {
        assert!(
            SchemaVersion::V20
                .content_type()
                .ends_with("/schema/2.0#\""),
            "2.0 profiles itself"
        );
        assert!(
            SchemaVersion::V21
                .content_type()
                .ends_with("/schema/2.1#\""),
            "2.1 profiles itself"
        );
        assert_eq!(SchemaVersion::V20.as_str(), "2.0", "version field");
        assert_eq!(SchemaVersion::V21.as_str(), "2.1", "version field");
    }

    #[test]
    fn the_discovery_document_links_both_json_suffixed_docs() {
        let value = serde_json::to_value(Discovery::for_base_url("https://test.example")).unwrap();
        assert_eq!(
            value,
            json!({
                "links": [
                    {
                        "rel": "http://nodeinfo.diaspora.software/ns/schema/2.0",
                        "href": "https://test.example/nodeinfo/2.0.json",
                    },
                    {
                        "rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
                        "href": "https://test.example/nodeinfo/2.1.json",
                    },
                ],
            }),
            "the two links, in Pleroma's order"
        );
    }

    /// The object keys of `value`, for comparing key sets.
    fn keys_of(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .expect("a document is an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn both_documents_share_their_key_sets_except_version_and_software() {
        let v20 = document(SchemaVersion::V20);
        let v21 = document(SchemaVersion::V21);
        assert_eq!(keys_of(&v20), keys_of(&v21), "the same top-level keys");
        assert_eq!(
            keys_of(&v20["software"]),
            vec!["name", "version"],
            "2.0 software has no link keys"
        );
        assert_eq!(
            keys_of(&v21["software"]),
            vec!["homepage", "name", "repository", "version"],
            "2.1 adds repository and homepage (Value maps sort keys)"
        );
    }

    #[test]
    fn the_document_holds_the_pleroma_compatibility_identity() {
        let v20 = document(SchemaVersion::V20);
        assert_eq!(v20["version"], "2.0", "the version field");
        assert_eq!(v20["software"]["name"], "pleroma", "clients sniff it");
        assert_eq!(
            v20["software"]["version"],
            format!(
                "2.10.2 (compatible; wired-be {})",
                env!("CARGO_PKG_VERSION")
            ),
            "the compatibility comment carries the crate version"
        );
        assert_eq!(v20["protocols"], json!(["activitypub"]), "one protocol");
        assert_eq!(
            v20["services"],
            json!({"inbound": [], "outbound": []}),
            "no third-party services"
        );
    }

    #[test]
    fn the_metadata_block_holds_the_pleroma_defaults() {
        let v21 = document(SchemaVersion::V21);
        let metadata = &v21["metadata"];
        assert_eq!(metadata["nodeName"], "Wired", "from the test config");
        assert_eq!(
            metadata["nodeDescription"], "A Pleroma-compatible instance",
            "from the test config"
        );
        assert_eq!(metadata["private"], false, "a public instance");
        assert_eq!(metadata["suggestions"], json!({"enabled": false}));
        assert_eq!(metadata["accountActivationRequired"], false);
        assert_eq!(metadata["invitesEnabled"], false);
        assert_eq!(metadata["mailerEnabled"], false);
        assert_eq!(metadata["skipThreadContainment"], true, "Pleroma 2.10's");
        assert_eq!(
            metadata["postFormats"],
            json!(["text/plain", "text/html", "text/markdown"]),
            "the three formats this server renders"
        );
        assert_eq!(
            metadata["federation"],
            json!({
                "enabled": true,
                "mrf_policies": [],
                "quarantined_instances": [],
                "exclusions": false,
            }),
            "plain federation, no filters"
        );
        assert_eq!(
            metadata["pollLimits"],
            json!({"max_options": 20, "max_option_chars": 200, "min_expiration": 0, "max_expiration": 31_536_000}),
            "the configured option count with Pleroma's window"
        );
        assert_eq!(
            metadata["uploadLimits"],
            json!({"general": 16_000_000, "avatar": 2_000_000, "banner": 4_000_000, "background": 4_000_000}),
            "the configured general limit with Pleroma's per-purpose defaults"
        );
        assert_eq!(
            metadata["fieldsLimits"],
            json!({"maxFields": 10, "maxRemoteFields": 20, "nameLength": 512, "valueLength": 2048}),
            "Pleroma's shipped field limits"
        );
        assert_eq!(
            metadata["markup"],
            json!({"allow_inline_images": true, "allow_headings": false, "allow_tables": false}),
            "Pleroma's shipped markup"
        );
        assert_eq!(
            metadata["features"],
            json!(instance_meta::features()),
            "the shared feature list"
        );
        assert_eq!(
            metadata["restrict_unauthenticated"],
            instance_meta::restrict_unauthenticated(),
            "the shared access restrictions, under its snake_case key"
        );
        assert_eq!(metadata["staffAccounts"], json!([]), "no staff fixture");
    }

    #[test]
    fn usage_counts_serialize_under_the_schema_names() {
        let config = Config::for_tests();
        let document = NodeInfo::build(
            SchemaVersion::V20,
            &config,
            UserCounts {
                total: 12,
                active_month: 4,
                active_halfyear: 9,
            },
            77,
            vec!["https://test.example/users/alice".to_owned()],
        );
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(
            value["usage"],
            json!({
                "users": {"total": 12, "activeMonth": 4, "activeHalfyear": 9},
                "localPosts": 77,
            }),
            "the schema's camelCase counters"
        );
        assert_eq!(value["openRegistrations"], true, "the test config default");
        assert_eq!(
            value["metadata"]["staffAccounts"],
            json!(["https://test.example/users/alice"]),
            "staff actor ids pass through"
        );
    }
}
