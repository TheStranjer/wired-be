//! What `POST /api/v1/statuses` accepts: a request body as a client sends it,
//! checked and resolved into the values a status is built from.
//!
//! The reference is Pleroma's `StatusController.create`
//! (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`) and the
//! one limit check it reaches through `CommonAPI.post`,
//! `Utils.validate_character_limit` (`lib/pleroma/web/common_api/utils.ex`):
//! trimmed text that is empty is refused with "Cannot post an empty status
//! without attachments", text past the instance's limit with "The status is
//! over the character limit", and every parameter the body leaves out falls
//! back to a stored or configured default instead of being an error.
//!
//! Two differences from Pleroma, both deliberate. The measure is
//! [`status_length`], which charges each bare URL `URL_WEIGHT` characters the
//! way Mastodon's `charactersReservedPerUrl` does where Pleroma counts the raw
//! text; and the author's two defaults arrive as plain arguments to
//! [`check_body`] rather than being read off a row, so the whole ruleset is
//! testable without a database. [`create_status`] is the database-facing
//! half the endpoint calls: it supplies those two values from the caller's
//! row, renders the checked text, resolves its mentions, stores the row and
//! its mention and hashtag links through [`status::insert`], and answers the
//! [`Status`] entity rendered for the author — Pleroma's `for: user`. The
//! endpoint's remaining parameters — `poll`, `in_reply_to_id` and
//! `scheduled_at` — join [`CreateStatusParams`] as extra `Option`s with
//! their own tickets (decision 15), and until then a body that sends them is
//! only a body whose known fields are checked here.
//!
//! The module is also the shared half of `PUT /api/v1/statuses/{id}`:
//! [`edit_status`] merges the edit body over the stored row, pushes the
//! merge through the very same [`check_body`], and renders the result
//! through [`render_status_fields`] — the function that turns checked text
//! into its stored HTML, its mention links and its hashtags, and the reason
//! an edit renders a post exactly the way a creation would. The edit's own
//! `media_ids` parameter (which creation does not carry yet) resolves to the
//! ids the author may attach, Pleroma's `attachments_from_ids` dropping the
//! ids that name nothing;
//! [`crate::models::status_edit::update_status`] writes the whole edit as
//! one transaction.
//!
//! [`Status`]: crate::entities::status::Status
//! [`status::insert`]: crate::models::status::insert
//!
//! Every refusal is an [`AppError::Unprocessable`] — a 422 whose body is
//! `{"error":"…"}` (decision 8) — in Pleroma's or Mastodon's own wording,
//! pinned by the tests below: the two limit messages, `Invalid visibility` for
//! a submitted scope outside the five [`Visibility`] names, `Invalid
//! content_type` for a MIME type outside `CONTENT_TYPES`, and Rails'
//! too-long sentence for a content warning or a language tag past its cap.

use std::collections::HashMap;

use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::Config,
    entities::status::Status,
    error::{AppError, AppResult},
    models::{
        oauth_app::OauthApp,
        status::{self, DEFAULT_CONTENT_TYPE, NewStatus, Visibility},
        status_edit,
        user::User,
    },
    services::{mentions::resolve_mentions, render_status::render_status},
    state::AppState,
    util::{
        entities::{MentionRef, extract_hashtags, linkify_entities},
        format::{render_content, status_length},
        id::parse_id,
    },
};

/// What one bare URL costs in a status: Mastodon's
/// `charactersReservedPerUrl`, passed to [`status_length`].
const URL_WEIGHT: usize = 23;

/// Longest content warning the endpoint accepts, in characters.
const MAX_SPOILER_CHARS: usize = 200;

/// Longest language tag the endpoint accepts, in characters.
const MAX_LANGUAGE_CHARS: usize = 10;

/// The MIME types a status may declare, matched verbatim and
/// case-sensitively — the list the instance and nodeinfo endpoints advertise
/// as this server's `post_formats`.
const CONTENT_TYPES: [&str; 3] = ["text/plain", "text/html", "text/markdown"];

/// 422 body for text that leaves nothing behind once trimmed: Pleroma's
/// `validate_character_limit` wording, which an attachments-only post will be
/// allowed to send once a later ticket adds `media_ids`.
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";

/// 422 body for text over the instance's limit (Pleroma's wording again).
const OVER_LIMIT: &str = "The status is over the character limit";

/// 422 body for a submitted visibility outside the five names, Pleroma's
/// message for its `:visibility_error`.
const INVALID_VISIBILITY: &str = "Invalid visibility";

/// 422 body for a submitted MIME type outside [`CONTENT_TYPES`].
const INVALID_CONTENT_TYPE: &str = "Invalid content_type";

/// The body of `POST /api/v1/statuses`, as the client wrote it.
///
/// Every field is an `Option` with a serde `default`, so a key the body omits
/// or sends as `null` stays this module's decision instead of becoming a
/// deserialization error; a later ticket joins the struct with `media_ids`,
/// `poll`, `in_reply_to_id` and `scheduled_at` as further `Option`s, the same
/// way. The `sensitive` field is a [`flex`] boolean, so a form body's
/// `sensitive=on` spells what a JSON body's `true` spells.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize)]
pub struct CreateStatusParams {
    /// The post text; trimmed, and required to leave something behind.
    #[serde(default)]
    status: Option<String>,
    /// The content warning shown in place of the post; at most
    /// `MAX_SPOILER_CHARS` characters, and the empty string when absent.
    #[serde(default)]
    spoiler_text: Option<String>,
    /// Whether the post starts folded; the author's own default when the body
    /// does not say.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    sensitive: Option<bool>,
    /// The requested audience, one of the five [`Visibility`] names; the
    /// author's default scope when absent.
    #[serde(default)]
    visibility: Option<String>,
    /// The content language tag; at most `MAX_LANGUAGE_CHARS` characters,
    /// and no tag at all when absent or empty.
    #[serde(default)]
    language: Option<String>,
    /// The MIME type of the `status` text; [`DEFAULT_CONTENT_TYPE`] when
    /// absent.
    #[serde(default)]
    content_type: Option<String>,
}

/// A checked status body: what [`check_body`] resolved, borrowed from the
/// [`CreateStatusParams`] it read, ready for a caller to build a row from.
#[derive(Debug)]
pub struct Checked<'a> {
    /// The post text, trimmed and within the instance's limit as
    /// [`status_length`] measures it.
    pub text: &'a str,
    /// The content warning exactly as submitted, or the empty string when the
    /// body carried none; never trimmed, because a warning is stored verbatim.
    pub spoiler_text: &'a str,
    /// The submitted flag, or the author's default the caller passed in.
    pub sensitive: bool,
    /// The submitted audience, or the author's default scope the caller passed
    /// in, or [`Visibility::Public`] behind a stored scope that names nothing.
    pub visibility: Visibility,
    /// The submitted language tag; `None` when the body gave none or gave the
    /// empty string, which is how a client clears a tag it posted before.
    pub language: Option<&'a str>,
    /// The declared MIME type, always one of `CONTENT_TYPES` —
    /// [`DEFAULT_CONTENT_TYPE`] when the body did not say.
    pub content_type: &'static str,
}

/// Checks a status body against the instance's limits and the author's two
/// defaults, or refuses it.
///
/// `config` supplies the length cap ([`Config::max_status_chars`],
/// `docs/ARCHITECTURE.md` decision 12). `default_scope` and
/// `default_sensitive` are the author's own preferences — Pleroma reads the
/// same two off the user as `default_scope` and `default_sensitive`, and here
/// they come in as plain values so that this module needs no database.
/// Whatever the body does not say, they answer; whatever it does say, they
/// lose to, except that a stored scope naming no [`Visibility`] falls back to
/// [`Visibility::Public`] rather than failing somebody's post.
///
/// The rules run in this order — the text (empty, then too long), the content
/// warning, the language, the visibility, then the content type — so a body
/// that breaks several is refused for the first of them.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `EMPTY_STATUS`, `OVER_LIMIT`,
/// `INVALID_VISIBILITY` or `INVALID_CONTENT_TYPE`, or with Rails' too-long
/// wording for a content warning or a language tag past its cap.
pub fn check_body<'a>(
    config: &Config,
    default_scope: &str,
    default_sensitive: bool,
    params: &'a CreateStatusParams,
) -> AppResult<Checked<'a>> {
    let text = check_text(config, params.status.as_deref().unwrap_or_default())?;
    let spoiler_text = checked_spoiler(params.spoiler_text.as_deref())?;
    let language = checked_language(params.language.as_deref())?;
    let visibility = checked_visibility(params.visibility.as_deref(), default_scope)?;
    let content_type = checked_content_type(params.content_type.as_deref())?;
    Ok(Checked {
        text,
        spoiler_text,
        sensitive: params.sensitive.unwrap_or(default_sensitive),
        visibility,
        language,
        content_type,
    })
}

/// The post text: trimmed, non-empty, and no longer than
/// [`Config::max_status_chars`] measured with [`status_length`]'s weighting.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`EMPTY_STATUS`] or [`OVER_LIMIT`].
fn check_text<'a>(config: &Config, text: &'a str) -> AppResult<&'a str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(AppError::Unprocessable(EMPTY_STATUS.to_owned()));
    }
    if status_length(trimmed, URL_WEIGHT) > config.max_status_chars {
        return Err(AppError::Unprocessable(OVER_LIMIT.to_owned()));
    }
    Ok(trimmed)
}

/// The content warning: [`MAX_SPOILER_CHARS`] characters at most, and the
/// empty string when the body carried none — what the stored column holds for
/// a post without a warning.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with the too-long wording of [`checked_len`].
fn checked_spoiler(spoiler_text: Option<&str>) -> AppResult<&str> {
    checked_len(
        spoiler_text.unwrap_or_default(),
        MAX_SPOILER_CHARS,
        "Spoiler text",
    )
}

/// The language tag, or `None`: absent and empty both mean no tag, and a
/// present one must fit [`MAX_LANGUAGE_CHARS`] characters.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with the too-long wording of [`checked_len`].
fn checked_language(language: Option<&str>) -> AppResult<Option<&str>> {
    let Some(tag) = language.filter(|tag| !tag.is_empty()) else {
        return Ok(None);
    };
    checked_len(tag, MAX_LANGUAGE_CHARS, "Language").map(Some)
}

/// The audience: the submitted name, which must be one of the five
/// [`Visibility`] spellings; or, absent, the author's `default_scope` read the
/// same way, with [`Visibility::Public`] behind a stored value that names
/// nothing.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_VISIBILITY`] for a *submitted*
/// name outside the five; an unusable default is never the poster's fault.
fn checked_visibility(submitted: Option<&str>, default_scope: &str) -> AppResult<Visibility> {
    submitted.map_or_else(
        || Ok(default_scope.parse().unwrap_or(Visibility::Public)),
        |name| {
            name.parse()
                .ok()
                .ok_or_else(|| AppError::Unprocessable(INVALID_VISIBILITY.to_owned()))
        },
    )
}

/// The MIME type: the submitted one, which must equal an entry of
/// [`CONTENT_TYPES`] case-sensitively, or [`DEFAULT_CONTENT_TYPE`] when the
/// body did not name one.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_CONTENT_TYPE`] for anything
/// else, including Pleroma's own `text/x-markdown`.
fn checked_content_type(submitted: Option<&str>) -> AppResult<&'static str> {
    submitted.map_or(Ok(DEFAULT_CONTENT_TYPE), |name| {
        CONTENT_TYPES
            .into_iter()
            .find(|known| *known == name)
            .ok_or_else(|| AppError::Unprocessable(INVALID_CONTENT_TYPE.to_owned()))
    })
}

/// The text, when it fits `max_chars` scalar values (not bytes); otherwise
/// the 422 Rails puts in its changeset errors, naming `what` and the cap.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with
/// `Validation failed: {what} is too long (maximum is {max_chars} characters)`.
fn checked_len<'a>(text: &'a str, max_chars: usize, what: &str) -> AppResult<&'a str> {
    if text.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {what} is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(text)
}

/// What reading a status text came to: the HTML to store, and the two
/// entity lists that go with it in the order the text carries them.
#[derive(Debug)]
pub struct RenderedFields {
    /// The rendered, linkified HTML for the `content` column.
    pub content: String,
    /// The accounts the text @-mentioned, in mention order.
    pub mentioned_user_ids: Vec<Uuid>,
    /// The hashtags the text carries, as [`extract_hashtags`] folded them.
    pub hashtags: Vec<String>,
}

/// Creates the status `params` describes as a local post by `user` and
/// answers its [`Status`] entity as the author sees it.
///
/// Nothing is written before [`check_body`] has accepted the body: it
/// supplies the author's own two defaults — the row's
/// [`default_scope`](User::default_scope) and
/// [`sensitive_default`](User::sensitive_default), Pleroma reads the same
/// pair off the user — and [`create_status`] does the rest: the render and
/// the entity lists (the [`render_status_fields`] step below), the stored row
/// with its mention and hashtag links and the author's moved counters
/// ([`status::insert`]), and the render for the author. `app` — the client
/// that issued the presenting
/// token, `None` when the caller names none — is recorded as the post's
/// `application` pair, Pleroma's `put_application`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the body's own refusals, through
/// [`check_body`]; [`AppError::Internal`] for the one row this function
/// wrote that the renderer refuses to answer (unreachable for a fresh local
/// post, which is neither a tombstone nor a reblog). Any database failure
/// arrives through `?` as [`AppError::Database`].
pub async fn create_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: CreateStatusParams,
) -> AppResult<Status> {
    let checked = check_body(
        &state.config,
        &user.default_scope,
        user.sensitive_default.get(),
        &params,
    )?;
    let entities = render_status_fields(state, checked.text, checked.content_type).await?;
    let row = status::insert(
        &state.db,
        &state.config.base_url,
        &new_status(user, app, &checked, &entities),
    )
    .await?;
    debug!(
        status = %row.id,
        user = %user.id,
        visibility = %row.visibility,
        "status created"
    );
    let entity = render_status(state, Some(user), &row)
        .await?
        // Unreachable for what was written a moment ago — live, local and no
        // reblog, so the renderer has nothing to drop it for — and a 500
        // rather than a panic if it ever happens.
        .ok_or_else(|| AppError::Internal(format!("the status {} did not render", row.id)))?;
    after_status_created(state, &entity);
    Ok(entity)
}

/// Renders one status text the way this instance stores it.
///
/// The shared render half of both status writes — [`create_status`] runs it
/// on the checked body's text, [`edit_status`] on the merged one — so an
/// edit linkifies, mention-resolves and tags a post exactly the way posting
/// the same text would. The answer is the HTML with every known
/// `@-mention` and every hashtag linked, the mentioned accounts in mention
/// order, and the hashtags as [`extract_hashtags`] folded them.
///
/// The lookup is [`resolve_mentions`], so a bare `@nick`, a
/// `@nick@our-domain` and a mirrored `@nick@their.domain` are linked from
/// the stored rows, an unknown `@nick@their.domain` is fetched over the
/// network (under that function's shared budget and per-status cap, see
/// [`crate::services::mentions`]) and linked when it answers, while what
/// stays unknown remains the plain text it was written as. Both sides of
/// the map are keyed case-folded, because the text spells the account
/// however its author typed it and the link still carries that spelling.
///
/// # Errors
///
/// Any database failure behind the mention lookups arrives as
/// [`AppError::Database`]. A remote lookup's own failures are not errors:
/// those mentions just stay plain text.
pub async fn render_status_fields(
    state: &AppState,
    text: &str,
    content_type: &str,
) -> AppResult<RenderedFields> {
    let found = resolve_mentions(state, text).await?;
    let urls: HashMap<String, String> = found
        .iter()
        .map(|(mention, account)| (mention_key(mention), profile_url(account)))
        .collect();
    let link = |mention: &MentionRef| urls.get(&mention_key(mention)).cloned();
    let html = render_content(text, content_type);
    Ok(RenderedFields {
        content: linkify_entities(&html, &link, &state.config.base_url),
        mentioned_user_ids: found.iter().map(|(_, account)| account.id).collect(),
        hashtags: extract_hashtags(text),
    })
}

/// The URL a mention links to: the stored [`profile_url`](User::profile_url)
/// that Pleroma's `Formatter.mention_from_user` prefers as `user.uri`, or the
/// actor id every row is guaranteed to carry — the same fallback the
/// [`Mention`](crate::entities::mention::Mention) entity renders.
fn profile_url(user: &User) -> String {
    user.profile_url
        .clone()
        .unwrap_or_else(|| user.ap_id.clone())
}

/// The lookup key of one mention: the account it names, case-folded — `nick`
/// for a bare reference, `nick@domain` when the text spelled the domain out.
fn mention_key(mention: &MentionRef) -> String {
    mention.acct().to_lowercase()
}

/// The [`NewStatus`] of one checked body: a `local` post by `user` with its
/// rendered content and source text, its warning and flags, the two entity
/// lists [`status::insert`] links in its transaction, and the client `app`
/// recorded as the post's application — Pleroma's `put_application`, which
/// is what makes the entity's `application` block come back.
fn new_status(
    user: &User,
    app: Option<&OauthApp>,
    checked: &Checked<'_>,
    entities: &RenderedFields,
) -> NewStatus {
    NewStatus {
        user_id: user.id,
        content: entities.content.clone(),
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        visibility: checked.visibility,
        language: checked.language.map(str::to_owned),
        mentioned_user_ids: entities.mentioned_user_ids.clone(),
        hashtags: entities.hashtags.clone(),
        local: true,
        application_name: app.map(|found| found.client_name.clone()),
        application_website: app.and_then(|found| found.website.clone()),
        ..NewStatus::default()
    }
}

/// The post-write hook of [`create_status`]: the tickets that let a fresh
/// local status out of the database it was just written to — the streaming
/// `update` broadcast to the author's and the mentioned users' sockets and
/// to the public and hashtag timelines, the notifications for the mentions
/// and the author's followers, and the federation of the `Create` activity
/// through the job queue (see [`crate::models::job`], decision 11) — wire
/// themselves in here, and only here.
///
/// Intentionally empty until then, like the reports'
/// [`after_report_created`](crate::api::reports) hook: the write path owns
/// the row and the render, and the entity it hands over is the one all three
/// of those consumers are built from.
const fn after_status_created(_state: &AppState, _status: &Status) {}

/// The body of `PUT /api/v1/statuses/{id}`, as the client wrote it.
///
/// The five text parameters mirror [`CreateStatusParams`]'s without
/// `visibility` — an edit never moves a post to a different audience
/// (Pleroma's `make_update_data` carries the stored visibility forward) —
/// and every one is optional: only the parameters the body presents change
/// the post ([`edit_status`]). `media_ids` is decision 10's id-array —
/// JSON's `["a","b"]` or the form's `media_ids[]=a&media_ids[]=b`, both
/// read as a list of id text — and `sensitive` the same
/// [`flex`] boolean the create body accepts.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize)]
pub struct EditStatusParams {
    /// The new post text; trimmed and capped the way creation caps it.
    #[serde(default)]
    status: Option<String>,
    /// The new content warning, stored verbatim; at most
    /// `MAX_SPOILER_CHARS` characters.
    #[serde(default)]
    spoiler_text: Option<String>,
    /// Whether the post starts folded.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    sensitive: Option<bool>,
    /// The new content language tag; empty clears it.
    #[serde(default)]
    language: Option<String>,
    /// The `MIME` type [`status`](Self::status) was written in.
    #[serde(default)]
    content_type: Option<String>,
    /// The post's whole new attachment set, as id text; ids that name no
    /// attachment the author may attach drop out — see [`edit_status`].
    #[serde(default)]
    media_ids: Option<Vec<String>>,
}

/// The edit body merged over the stored row: every [`CreateStatusParams`]
/// field carrying the submitted value where the body presented one and the
/// row's own value where it did not. The merged body is what [`check_body`]
/// reads, so the create path's checks run on the post as it will stand —
/// and a field nobody edited can never fail a check it already passed.
fn merged_body(row: &status::Status, params: &EditStatusParams) -> CreateStatusParams {
    CreateStatusParams {
        status: kept(params.status.clone(), || row.raw_text.clone()),
        spoiler_text: kept(params.spoiler_text.clone(), || row.spoiler_text.clone()),
        sensitive: Some(params.sensitive.unwrap_or(row.sensitive)),
        visibility: Some(row.visibility.as_str().to_owned()),
        language: params.language.clone().or_else(|| row.language.clone()),
        content_type: kept(params.content_type.clone(), || row.content_type.clone()),
    }
}

/// One optional edit parameter against the value the stored row keeps in
/// its place: the submitted one wins outright — even the empty spellings
/// that mean "clear this field" — and only its absence asks for `stored`.
fn kept<T, F: FnOnce() -> T>(submitted: Option<T>, stored: F) -> Option<T> {
    submitted.or_else(|| Some(stored()))
}

/// Edits the status `row` as `params` describes and answers the [`Status`]
/// entity as `user` — its author, which is the endpoint's precondition for
/// reaching this function at all — sees the post after the write.
///
/// `merged_body` resolves which parameters the body presented,
/// [`check_body`] refuses the merge exactly as it refuses a creation
/// (422 `Cannot post an empty status without attachments`,
/// `The status is over the character limit`, the spoiler and language
/// length wordings, `Invalid content_type`), and [`render_status_fields`]
/// renders the merged text into the content, mention list and tag list
/// creation would have written for it. The whole new state then travels to
/// [`status_edit::update_status`], whose one transaction records the
/// superseded version (and the `edited_at` stamp) only when content,
/// warning, sensitive flag or media actually changed, and replaces the
/// post's mentions, hashtags and media.
///
/// Of the edit's own `media_ids`, only the ids the author may attach to
/// this post survive to the write: rows nobody owns, rows another status
/// claims and text that is no id at all name nothing and drop out, as
/// Pleroma's `CommonAPI.Utils.attachments_from_ids` rejects its nils.
///
/// `None` is the row a delete took between the endpoint's gate and this
/// write; the endpoint answers that its shared 404.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the merged body's refusals, through
/// [`check_body`]; the database failures of the write and of the render
/// arrive through `?` as [`AppError::Database`].
pub async fn edit_status(
    state: &AppState,
    user: &User,
    row: &status::Status,
    params: EditStatusParams,
) -> AppResult<Option<Status>> {
    let merged = merged_body(row, &params);
    let checked = check_body(
        &state.config,
        row.visibility.as_str(),
        row.sensitive,
        &merged,
    )?;
    let fields = render_status_fields(state, checked.text, checked.content_type).await?;
    let media_ids = params
        .media_ids
        .map(|ids| ids.iter().filter_map(|id| parse_id(id)).collect());
    let edit = status_edit::EditParams {
        content: fields.content,
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        language: checked.language.map(str::to_owned),
        mentioned_user_ids: fields.mentioned_user_ids,
        hashtags: fields.hashtags,
        media_ids,
    };
    let Some(updated) = status_edit::update_status(&state.db, row.id, edit).await? else {
        return Ok(None);
    };
    debug!(
        status = %updated.id,
        user = %user.id,
        edited = updated.edited_at.is_some(),
        "status edited"
    );
    render_status(state, Some(user), &updated).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        extract::{Path, Query, State},
        http::{StatusCode, header::CONTENT_TYPE},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        http_client::HttpClient,
        mailer::MemoryTransport,
        models::user::{NewUser, insert as insert_user, local_ap_id},
        uploads::UploaderBackend,
        util::entities::{hashtag_html, mention_html},
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// The state under test: `pool` and the test config, with the
    /// collaborators the service never touches.
    fn state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(MemoryTransport::new()),
        }
    }

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A body carrying `text` as its only parameter.
    fn posted(text: &str) -> CreateStatusParams {
        CreateStatusParams {
            status: Some(text.to_owned()),
            ..CreateStatusParams::default()
        }
    }

    /// The status and the `{"error": …}` body one refusal renders: what the
    /// client sees, read off the response rather than the variant.
    async fn wire(err: AppError) -> (StatusCode, Value) {
        let response: Response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1_024)
            .await
            .expect("an error body is collectable");
        (
            status,
            serde_json::from_slice(&bytes).expect("errors are JSON"),
        )
    }

    /// The one body every validation refusal owes: 422, `{"error": message}`.
    fn refused(answer: &(StatusCode, Value), message: &str) {
        assert_eq!(
            answer.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the status a refusal answers with"
        );
        assert_eq!(
            answer.1,
            json!({ "error": message }),
            "the body that refusal carries"
        );
    }

    /// A plain post through the service, as `author` with no application.
    async fn created(state: &AppState, author: &User, params: CreateStatusParams) -> Status {
        create_status(state, author, None, params)
            .await
            .expect("the post creates")
    }

    /// The stored row of a rendered entity, live and unabridged.
    async fn stored(pool: &PgPool, entity: &Status) -> status::Status {
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the created status is stored live")
    }

    /// The parameters a JSON body deserializes to: the same
    /// `into_params::<CreateStatusParams>` call an [`ApiBody`] makes, so these
    /// tests see a body exactly as the endpoint will.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    fn params(value: Value) -> CreateStatusParams {
        crate::extract::into_params::<CreateStatusParams>(value)
            .expect("every body in these tests fits the parameter type")
    }

    /// The reading of `body` by [`check_body`] under the stock test
    /// configuration and an author who defaults to `public` and to not
    /// marking posts sensitive.
    fn accepted(body: &CreateStatusParams) -> Checked<'_> {
        check_body(&Config::for_tests(), "public", false, body)
            .unwrap_or_else(|err| panic!("this body should be accepted: {err}"))
    }

    /// The 422 message [`check_body`] answers for `body` under the same
    /// configuration and defaults.
    fn rejection(body: &CreateStatusParams) -> String {
        match check_body(&Config::for_tests(), "public", false, body) {
            Err(err) => err.to_string(),
            Ok(checked) => panic!("this body should be refused, got `{}`", checked.text),
        }
    }

    #[test]
    fn a_plain_post_is_read_with_the_author_defaults() {
        let body = params(json!({"status": "  hi  "}));
        let checked = accepted(&body);
        assert_eq!(checked.text, "hi", "the text is trimmed");
        assert_eq!(checked.spoiler_text, "", "no warning means no text");
        assert!(!checked.sensitive, "absent takes the author's `false`");
        assert_eq!(checked.visibility, Visibility::Public, "and their scope");
        assert_eq!(checked.language, None, "and no language at all");
        assert_eq!(
            checked.content_type, DEFAULT_CONTENT_TYPE,
            "and the default content type"
        );
        assert!(
            CONTENT_TYPES.contains(&DEFAULT_CONTENT_TYPE),
            "which is one of the advertised types, so the fallback can never \
             be a value `checked_content_type` would refuse"
        );
    }

    #[test]
    fn the_author_defaults_answer_what_the_body_leaves_out() {
        let body = params(json!({"status": "hi"}));
        let checked = check_body(&Config::for_tests(), "local", true, &body)
            .unwrap_or_else(|err| panic!("an author default is always usable: {err}"));
        assert_eq!(
            checked.visibility,
            Visibility::Local,
            "Pleroma's extra scope comes from the author's row too"
        );
        assert!(checked.sensitive, "and so does the folded flag");
    }

    #[test]
    fn what_the_body_sends_wins_over_the_author_defaults() {
        let body = params(json!({
            "status": "hi",
            "visibility": "direct",
            "sensitive": false,
            "language": "en",
            "spoiler_text": "spoilers",
            "content_type": "text/html",
        }));
        // The defaults of `accepted` are `public` / not-sensitive, so every
        // one of these has to have come from the body.
        let checked = accepted(&body);
        assert_eq!(checked.visibility, Visibility::Direct, "submitted scope");
        assert!(!checked.sensitive, "a submitted `false` is not absent");
        assert_eq!(checked.language, Some("en"), "submitted tag");
        assert_eq!(checked.spoiler_text, "spoilers", "submitted warning");
        assert_eq!(checked.content_type, "text/html", "submitted type");
    }

    #[test]
    fn a_sensitive_flag_accepts_the_form_spellings() {
        let on = params(json!({"status": "hi", "sensitive": "on"}));
        assert!(accepted(&on).sensitive, "`\"on\"` means true");
        let zero = params(json!({"status": "hi", "sensitive": "0"}));
        assert!(!accepted(&zero).sensitive, "`\"0\"` means false");
        let null = params(json!({"status": "hi", "sensitive": null}));
        assert!(!accepted(&null).sensitive, "a null is as absent as no key");
    }

    #[test]
    fn a_status_with_nothing_to_say_is_refused() {
        for value in [
            json!({}),
            json!({"status": null}),
            json!({"status": ""}),
            json!({"status": "   \n\t "}),
        ] {
            let body = params(value);
            assert_eq!(
                rejection(&body),
                EMPTY_STATUS,
                "a status with no text and no attachments is Pleroma's message"
            );
        }
    }

    #[test]
    fn a_status_over_the_character_limit_is_refused() {
        let limit = Config::for_tests().max_status_chars;
        let exact = params(json!({"status": "x".repeat(limit)}));
        assert_eq!(
            accepted(&exact).text.chars().count(),
            limit,
            "exactly the limit is still within it"
        );
        let over = params(json!({"status": "x".repeat(limit + 1)}));
        assert_eq!(
            rejection(&over),
            OVER_LIMIT,
            "and one character more is the over-limit message"
        );
    }

    #[test]
    fn the_limit_counts_a_bare_url_as_twenty_three_characters() {
        // 49 characters as written, 23 as a client counts them: over a
        // 40-character limit by length, under it by weighting.
        let url = "https://example.com/a/very/long/path?query=string";
        assert_eq!(
            status_length(url, URL_WEIGHT),
            URL_WEIGHT,
            "one bare URL costs exactly `URL_WEIGHT`, which is what this \
             module pins as Mastodon's `charactersReservedPerUrl`"
        );
        let mut config = Config::for_tests();
        config.max_status_chars = 40;
        let body = params(json!({"status": url}));
        let checked = check_body(&config, "public", false, &body)
            .unwrap_or_else(|_| panic!("a 49-character URL weighs {URL_WEIGHT} here"));
        assert_eq!(checked.text, url, "and the URL is stored verbatim");

        let plain = params(json!({"status": "x".repeat(41)}));
        let refused = check_body(&config, "public", false, &plain)
            .err()
            .unwrap_or_else(|| panic!("41 plain characters do not fit a 40-character limit"));
        assert_eq!(
            refused.to_string(),
            OVER_LIMIT,
            "which is the same refusal the weighting saves the URL from"
        );
    }

    #[test]
    fn a_content_warning_is_capped_at_two_hundred_characters() {
        let edge = "x".repeat(MAX_SPOILER_CHARS);
        let body = params(json!({"status": "hi", "spoiler_text": edge}));
        assert_eq!(
            accepted(&body).spoiler_text,
            edge,
            "the cap itself fits, and is stored verbatim"
        );
        let padded = params(json!({"status": "hi", "spoiler_text": "  cats  "}));
        assert_eq!(
            accepted(&padded).spoiler_text,
            "  cats  ",
            "a warning is never trimmed, only the text is"
        );
        let over = params(json!({
            "status": "hi",
            "spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1),
        }));
        assert_eq!(
            rejection(&over),
            "Validation failed: Spoiler text is too long (maximum is 200 characters)",
            "one character past the cap is refused in Rails' own wording"
        );
    }

    #[test]
    fn a_language_is_capped_at_ten_characters_and_emptied_by_nothing() {
        let edge = params(json!({
            "status": "hi",
            "language": "a".repeat(MAX_LANGUAGE_CHARS),
        }));
        assert_eq!(
            accepted(&edge).language,
            Some("aaaaaaaaaa"),
            "the cap itself fits"
        );
        for value in [
            json!({"status": "hi", "language": ""}),
            json!({"status": "hi", "language": null}),
            json!({"status": "hi"}),
        ] {
            let body = params(value);
            assert_eq!(
                accepted(&body).language,
                None,
                "an empty, null or absent tag is no tag at all"
            );
        }
        let over = params(json!({
            "status": "hi",
            "language": "a".repeat(MAX_LANGUAGE_CHARS + 1),
        }));
        assert_eq!(
            rejection(&over),
            "Validation failed: Language is too long (maximum is 10 characters)",
            "eleven characters are one too many, in the same wording"
        );
    }

    #[test]
    fn the_two_caps_count_characters_not_bytes() {
        let warning = "…".repeat(MAX_SPOILER_CHARS);
        let body = params(json!({"status": "hi", "spoiler_text": warning}));
        assert_eq!(
            accepted(&body).spoiler_text,
            warning,
            "200 three-byte characters are 600 bytes and still fit"
        );
        let tag = "日本語言語タグ";
        let tagged = params(json!({"status": "hi", "language": tag}));
        assert_eq!(
            accepted(&tagged).language,
            Some(tag),
            "and so do 7 of them in a 10-character tag"
        );
    }

    #[test]
    fn every_known_visibility_name_is_accepted_as_itself() {
        for name in Visibility::ALL {
            let body = params(json!({
                "status": "hi",
                "visibility": name.as_str(),
            }));
            assert_eq!(
                accepted(&body).visibility,
                name,
                "`{name}` is its own audience, whatever the author defaults to"
            );
        }
    }

    #[test]
    fn an_unknown_submitted_visibility_is_refused() {
        for name in ["", "bogus", "Public", "published", " private"] {
            let body = params(json!({"status": "hi", "visibility": name}));
            assert_eq!(
                rejection(&body),
                INVALID_VISIBILITY,
                "`{name}` is not one of the five, and parsing them is \
                 case-sensitive"
            );
        }
    }

    #[test]
    fn an_unusable_stored_scope_falls_back_to_public() {
        for stored in ["", "bogus", "Public"] {
            let body = params(json!({"status": "hi"}));
            let checked = check_body(&Config::for_tests(), stored, false, &body)
                .unwrap_or_else(|err| panic!("a broken default is not the poster's fault: {err}"));
            assert_eq!(
                checked.visibility,
                Visibility::Public,
                "a row holding `{stored}` still posts publicly"
            );
        }
    }

    #[test]
    fn the_advertised_content_types_pass_and_every_other_is_refused() {
        for name in CONTENT_TYPES {
            let body = params(json!({"status": "hi", "content_type": name}));
            assert_eq!(
                accepted(&body).content_type,
                name,
                "`{name}` is one of the formats the instance advertises"
            );
        }
        let null = params(json!({"status": "hi", "content_type": null}));
        assert_eq!(
            accepted(&null).content_type,
            DEFAULT_CONTENT_TYPE,
            "a null type is as absent as no key"
        );
        for name in [
            "text/x-markdown",
            "TEXT/PLAIN",
            "text/plain ",
            "markdown",
            "",
        ] {
            let body = params(json!({"status": "hi", "content_type": name}));
            assert_eq!(
                rejection(&body),
                INVALID_CONTENT_TYPE,
                "`{name}` is not on the list: the match is exact and \
                 case-sensitive, so even Pleroma's own `text/x-markdown` \
                 alias is refused"
            );
        }
    }

    #[tokio::test]
    async fn a_plain_post_stores_every_column_the_body_decided() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;

        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                spoiler_text: Some("cats".to_owned()),
                sensitive: Some(true),
                visibility: Some("local".to_owned()),
                language: Some("eo".to_owned()),
                content_type: Some("text/markdown".to_owned()),
                ..posted("  hello **world**  ")
            },
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(row.user_id, alice.id, "the author is the row's");
        assert!(row.local, "a posted status is a local one");
        assert_eq!(row.uri, format!("{BASE}/objects/{}", row.id));
        assert_eq!(
            row.url,
            Some(format!("{BASE}/notice/{}", row.id)),
            "and the human URL derives from the same id"
        );
        assert_eq!(
            row.raw_text, "hello **world**",
            "the source text is the trimmed one"
        );
        assert_eq!(
            row.content, "<p>hello <strong>world</strong></p>",
            "the content is the format rendered"
        );
        assert_eq!(row.content_type, "text/markdown");
        assert_eq!(row.spoiler_text, "cats", "the warning is stored");
        assert!(row.sensitive, "so is the flag");
        assert_eq!(row.visibility, Visibility::Local, "and the scope");
        assert_eq!(row.language.as_deref(), Some("eo"), "and the tag");
        assert_eq!(row.conversation_id, row.id, "a root post is its own thread");
        assert!(
            row.application_name.is_none() && row.application_website.is_none(),
            "no application is named, so the block stays empty"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(value["id"], row.id.to_string(), "the entity is the row");
        assert_eq!(value["uri"], row.uri, "with its object id");
        assert_eq!(value["url"], json!(row.url), "and its human URL");
        assert_eq!(value["content"], row.content, "and the render");
        assert_eq!(value["visibility"], "local", "as stored, verbatim");
        assert_eq!(value["sensitive"], true);
        assert_eq!(value["spoiler_text"], "cats");
        assert_eq!(value["language"], "eo");
        assert_eq!(value["application"], Value::Null);
        assert_eq!(value["account"]["acct"], "alice", "rendered as the author");
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "hello **world**" }),
            "the source text answers in the Pleroma block"
        );

        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(
            counted.statuses_count, 1,
            "the author counts the post they made"
        );
    }

    #[tokio::test]
    async fn mentions_and_hashtags_are_linked_stored_and_answered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "linker").await;
        let bob = user(pool, "bob").await;

        let entity = created(
            &app_state,
            &alice,
            posted("hi @BOB and @nobody, see #Rust #rust #123"),
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(
            entity.content,
            format!(
                "<p>hi {} and @nobody, see {} {} #123</p>",
                mention_html(&bob.ap_id, "BOB"),
                hashtag_html(BASE, "rust"),
                hashtag_html(BASE, "rust"),
            ),
            "the known mention becomes its h-card in the written case, the \
             unknown one stays text, the tag links in both spellings and the \
             digits-only one is no tag"
        );
        assert_eq!(
            row.raw_text, "hi @BOB and @nobody, see #Rust #rust #123",
            "the source text keeps what was typed"
        );

        let mentioned: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the mention links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(
            mentioned,
            vec![bob.id],
            "only the account this instance knows is linked"
        );

        let tagged: Vec<String> = sqlx::query_as::<_, (String,)>(
            "SELECT h.name FROM status_hashtags sh JOIN hashtags h ON h.id = sh.hashtag_id \
              WHERE sh.status_id = $1 ORDER BY h.name",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the tag links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(tagged, vec!["rust"], "the tag is stored once, folded");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the mentioned account answers in the entity"
        );
        assert_eq!(
            value["tags"],
            json!([{ "name": "rust", "url": format!("{BASE}/tags/rust") }]),
            "and so does the tag"
        );
    }

    /// The mock remote peer's `WebFinger` answer: any `acct:` handle gets a
    /// JRD whose self link points at this peer's own `/users/{nick}`.
    async fn mock_webfinger(
        State(base): State<String>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        let nick = params
            .get("resource")
            .and_then(|resource| resource.strip_prefix("acct:"))
            .and_then(|handle| handle.split_once('@').map(|(nick, _)| nick))
            .unwrap_or("alice");
        let jrd = json!({
            "subject": format!("acct:{nick}@mock.example"),
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{base}/users/{nick}"),
            }],
        });
        ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
    }

    /// The mock remote peer's actor document: self-identifying, named for
    /// the path segment.
    async fn mock_actor(State(base): State<String>, Path(nick): Path<String>) -> Response {
        let id = format!("{base}/users/{nick}");
        let document = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": nick,
            "name": "Mock",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
            },
        });
        (
            [(CONTENT_TYPE, "application/activity+json")],
            document.to_string(),
        )
            .into_response()
    }

    /// Serve the mock remote peer; answer with its origin and the task to
    /// abort when the test is done.
    async fn mock_peer() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/.well-known/webfinger", get(mock_webfinger))
            .route("/users/{nick}", get(mock_actor))
            .with_state(base.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        (base, server)
    }

    /// A remote mention fetched from the mock peer is linked into
    /// `status_mentions` and linkified in the HTML with its remote profile
    /// URL. The fetch itself (`WebFinger` + actor, budget and cap) is
    /// [`resolve_mentions`]'s tested behaviour; the loopback mock answers
    /// on `127.0.0.1`, so the posted spelling is the mirror's own handle
    /// (decision 7) and the mention resolves from the row the fetch stored.
    #[tokio::test]
    async fn a_resolved_remote_mention_is_stored_and_rendered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "remote-author").await;
        let (base, server) = mock_peer().await;

        let fetched = crate::services::mentions::resolve_mentions_with_base(
            &app_state,
            "hi @alice@mock.example",
            Some(&base),
        )
        .await
        .expect("the mock peer resolves")
        .pop()
        .expect("the remote mention resolved");
        server.abort();
        assert_eq!(fetched.1.nickname, "alice@127.0.0.1", "mirror handle");

        let entity = created(
            &app_state,
            &alice,
            posted(&format!("hi @{}!", fetched.1.nickname)),
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(
            entity.content,
            format!("<p>hi {}!</p>", mention_html(&fetched.1.ap_id, "alice")),
            "the remote mention is linkified to its actor URL"
        );
        let mentioned: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the mention links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(
            mentioned,
            vec![fetched.1.id],
            "the remote mirror is linked in status_mentions"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": fetched.1.id.to_string(), "username": "alice",
                     "url": fetched.1.ap_id, "acct": "alice@127.0.0.1" }]),
            "and answered in the entity"
        );
    }

    /// A mention of a domain that cannot be reached at all (`.invalid`
    /// never resolves) leaves the mention as plain text, resolves the
    /// known mentions beside it, and the status is created as always.
    #[tokio::test]
    async fn a_status_mentioning_an_unreachable_domain_still_creates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "patient-author").await;
        let bob = user(pool, "known-bob").await;

        let entity = created(
            &app_state,
            &alice,
            posted("hi @ghost@nowhere.invalid and @known-bob"),
        )
        .await;

        assert_eq!(
            entity.content,
            format!(
                "<p>hi @ghost@nowhere.invalid and {}</p>",
                mention_html(&bob.ap_id, "known-bob")
            ),
            "the unreachable mention stays exactly as written"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "known-bob",
                     "url": bob.ap_id, "acct": "known-bob" }]),
            "only the reachable account is answered"
        );
    }

    #[tokio::test]
    async fn a_bare_post_links_nothing_and_stores_no_entities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "plain").await;

        let entity = created(
            &app_state,
            &alice,
            posted("just text, with an email a@b.c and a #123"),
        )
        .await;
        let row = stored(pool, &entity).await;
        let value = serde_json::to_value(&entity).expect("the entity serializes");

        assert_eq!(
            value["content"], "<p>just text, with an email a@b.c and a #123</p>",
            "nothing in this text is an entity"
        );
        assert_eq!(value["mentions"], json!([]), "no mention, no entry");
        assert_eq!(value["tags"], json!([]), "no tag, no entry");
        let mentions: (i64,) =
            sqlx::query_as("SELECT count(*) FROM status_mentions WHERE status_id = $1")
                .bind(row.id)
                .fetch_one(pool)
                .await
                .expect("the mention links read");
        let tags: (i64,) =
            sqlx::query_as("SELECT count(*) FROM status_hashtags WHERE status_id = $1")
                .bind(row.id)
                .fetch_one(pool)
                .await
                .expect("the tag links read");
        assert_eq!(
            (mentions.0, tags.0),
            (0, 0),
            "and neither link table gained a row"
        );
    }

    #[tokio::test]
    async fn the_application_of_the_token_is_recorded_and_rendered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "client-author").await;
        let app = OauthApp {
            id: Uuid::now_v7(),
            client_name: "Wired Client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "write:statuses".to_owned(),
            website: Some("https://client.example".to_owned()),
            client_id: "client-id".to_owned(),
            client_secret: "client-secret".to_owned(),
            trusted: false,
            user_id: Some(alice.id),
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let entity = create_status(&app_state, &alice, Some(&app), posted("from a client"))
            .await
            .expect("the post creates");
        let row = stored(pool, &entity).await;
        assert_eq!(
            row.application_name.as_deref(),
            Some("Wired Client"),
            "the client name is stored"
        );
        assert_eq!(
            row.application_website.as_deref(),
            Some("https://client.example"),
            "and its website"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["application"],
            json!({ "name": "Wired Client", "website": "https://client.example" }),
            "both answer as the entity's application block"
        );

        let nameless = created(&app_state, &alice, posted("no client at all")).await;
        let bare = serde_json::to_value(&nameless).expect("the entity serializes");
        assert_eq!(
            bare["application"],
            Value::Null,
            "a post with no named client renders no block"
        );
    }

    #[tokio::test]
    async fn a_refused_body_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "refused-author").await;

        let err = create_status(&app_state, &alice, None, CreateStatusParams::default())
            .await
            .expect_err("an absent text is refused");
        refused(&wire(err).await, EMPTY_STATUS);
        refused(
            &wire(
                create_status(
                    &app_state,
                    &alice,
                    None,
                    CreateStatusParams {
                        visibility: Some("sideways".to_owned()),
                        ..posted("hi")
                    },
                )
                .await
                .expect_err("an unknown scope is refused"),
            )
            .await,
            INVALID_VISIBILITY,
        );
        refused(
            &wire(
                create_status(
                    &app_state,
                    &alice,
                    None,
                    CreateStatusParams {
                        content_type: Some("text/rtf".to_owned()),
                        ..posted("hi")
                    },
                )
                .await
                .expect_err("an unknown format is refused"),
            )
            .await,
            INVALID_CONTENT_TYPE,
        );

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM statuses")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 0, "not one request above wrote a row");
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "and no count moved");
    }

    #[tokio::test]
    async fn the_rich_formats_render_and_the_plain_one_escapes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "formatter").await;

        let markdown = created(
            &app_state,
            &alice,
            CreateStatusParams {
                content_type: Some("text/markdown".to_owned()),
                ..posted("a **bold** claim")
            },
        )
        .await;
        assert_eq!(
            markdown.content, "<p>a <strong>bold</strong> claim</p>",
            "markdown becomes the tag it means"
        );

        let html = created(
            &app_state,
            &alice,
            CreateStatusParams {
                content_type: Some("text/html".to_owned()),
                ..posted("<p>marked up</p><script>steal()</script>")
            },
        )
        .await;
        assert_eq!(
            html.content, "<p>marked up</p>",
            "submitted HTML keeps the allowed tags and loses the script \
             element with its text, as this instance's scrubber does"
        );

        let plain = created(
            &app_state,
            &alice,
            posted("<script>steal()</script> & friends"),
        )
        .await;
        assert_eq!(
            plain.content, "<p>&lt;script&gt;steal()&lt;/script&gt; &amp; friends</p>",
            "and plain text is escaped, so it can say those characters"
        );
    }

    /// An edit body as `ApiBody` hands it over, from the JSON shape the
    /// extractor builds for either accepted content type.
    fn body(value: Value) -> EditStatusParams {
        crate::extract::into_params::<EditStatusParams>(value)
            .expect("every body in these tests fits the edit parameter type")
    }

    #[test]
    fn every_edit_parameter_is_optional() {
        let empty = body(json!({}));
        assert_eq!(empty.status, None, "no text means the text stays");
        assert_eq!(empty.spoiler_text, None, "as does the warning");
        assert_eq!(empty.sensitive, None, "the flag");
        assert_eq!(empty.language, None, "the tag");
        assert_eq!(empty.content_type, None, "the type");
        assert_eq!(empty.media_ids, None, "and nothing is said about media");

        let full = body(json!({
            "status": "new words",
            "spoiler_text": "careful",
            "sensitive": "on",
            "language": "eo",
            "content_type": "text/markdown",
            "media_ids": ["a", "b"],
        }));
        assert_eq!(full.status.as_deref(), Some("new words"));
        assert_eq!(full.spoiler_text.as_deref(), Some("careful"));
        assert_eq!(full.sensitive, Some(true), "the form spellings ride along");
        assert_eq!(full.language.as_deref(), Some("eo"));
        assert_eq!(full.content_type.as_deref(), Some("text/markdown"));
        assert_eq!(
            full.media_ids,
            Some(vec!["a".to_owned(), "b".to_owned()]),
            "decision 10's array arrives as id text either way"
        );
    }

    #[tokio::test]
    async fn the_merge_carries_the_submitted_field_and_the_stored_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "merger").await;
        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                spoiler_text: Some("kept warning".to_owned()),
                sensitive: Some(true),
                visibility: Some("private".to_owned()),
                language: Some("eo".to_owned()),
                content_type: Some("text/markdown".to_owned()),
                ..posted("the stored text")
            },
        )
        .await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let merged = merged_body(&row, &body(json!({"status": "new words"})));
        assert_eq!(
            merged.status.as_deref(),
            Some("new words"),
            "the submitted field wins"
        );
        assert_eq!(
            merged.spoiler_text.as_deref(),
            Some("kept warning"),
            "and every field the body left out arrives as the row has it"
        );
        assert_eq!(merged.sensitive, Some(true));
        assert_eq!(merged.visibility.as_deref(), Some("private"));
        assert_eq!(merged.language.as_deref(), Some("eo"));
        assert_eq!(merged.content_type.as_deref(), Some("text/markdown"));

        let checked = check_body(&Config::for_tests(), "public", false, &merged)
            .expect("a merge of a stored post is always checkable");
        assert_eq!(checked.text, "new words", "the new text goes through");
        assert_eq!(checked.visibility, Visibility::Private, "the scope stays");
        assert!(checked.sensitive, "so does the flag");
    }

    #[tokio::test]
    async fn an_empty_language_submitted_clears_where_absence_keeps() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "untagger").await;
        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                language: Some("de".to_owned()),
                ..posted("hallo")
            },
        )
        .await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let kept = merged_body(&row, &body(json!({"status": "hi again"})));
        assert_eq!(kept.language.as_deref(), Some("de"), "absent keeps the tag");
        let cleared = merged_body(&row, &body(json!({"language": ""})));
        assert_eq!(
            cleared.language.as_deref(),
            Some(""),
            "a submitted empty tag merges as the empty string, which \
             `check_body` reads as no tag at all"
        );
        assert_eq!(
            check_body(&Config::for_tests(), "public", false, &cleared)
                .expect("clearing is always checkable")
                .language,
            None
        );
    }

    #[tokio::test]
    async fn editing_through_the_service_supersedes_a_version_and_rewrites_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "editor-service").await;
        let entity = created(&app_state, &alice, posted("first version #keep")).await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let edited = edit_status(
            &app_state,
            &alice,
            &row,
            body(json!({"status": "second version"})),
        )
        .await
        .expect("the edit writes")
        .expect("the status is live");
        assert_eq!(edited.content, "<p>second version</p>", "the new render");
        assert!(
            edited.edited_at.is_some(),
            "a superseded version stamps the edit"
        );

        let stored = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the edited status is stored");
        assert_eq!(stored.raw_text, "second version");
        assert_eq!(
            stored.spoiler_text, "",
            "the fields nobody sent are the fields nobody moved"
        );
        let history = status_edit::list_for_status(pool, id)
            .await
            .expect("the revisions read");
        assert_eq!(history.len(), 1, "the first version is on record");
        assert_eq!(history[0].raw_text, "first version #keep");
        assert_eq!(
            history[0].content, entity.content,
            "the snapshot carries the old render, linkified and all — not \
             just the old source"
        );
    }

    #[tokio::test]
    async fn a_rejected_edit_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "refused-editor").await;
        let entity = created(&app_state, &alice, posted("untouched")).await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let long = format!(
            "Validation failed: Spoiler text is too long (maximum is {MAX_SPOILER_CHARS} characters)"
        );
        for (value, message) in [
            (json!({"status": "   "}), EMPTY_STATUS),
            (json!({"content_type": "text/rtf"}), INVALID_CONTENT_TYPE),
            (
                json!({"spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1)}),
                long.as_str(),
            ),
        ] {
            let err = edit_status(&app_state, &alice, &row, body(value))
                .await
                .expect_err("the body above is refused before anything is written");
            assert_eq!(err.to_string(), message, "the refusal wording");
        }

        let history = status_edit::list_for_status(pool, id)
            .await
            .expect("the revisions read");
        assert!(history.is_empty(), "no refused edit recorded a revision");
        let stored = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");
        assert_eq!(stored.content, "<p>untouched</p>", "or moved the row");
        assert!(stored.edited_at.is_none(), "or stamped it either");
    }
}
