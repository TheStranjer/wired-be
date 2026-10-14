//! The account endpoints: the single-account reads and listings, the
//! relationship batch, the notes write, and the profile write.
//!
//! `GET /api/v1/accounts/verify_credentials`, `GET /api/v1/accounts/{id}`,
//! `…/lookup`, `…/relationships` and the relationship listings
//! `…/{id}/followers` and `…/{id}/following` are the reads;
//! `PATCH /api/v1/accounts/update_credentials`,
//! `POST /api/v1/accounts/{id}/note` and `…/{id}/remove_from_followers`
//! are the writes.
//!
//! Pleroma's `AccountController.verify_credentials`, `.show`, `.lookup`,
//! `.relationships`, `.followers` and `.following`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`)
//! are the reference.
//!
//! All of these answer the core [`Account`] entity
//! (decision 3: rendered from the row by [`Account::from_user_for`]); they
//! serve local and remote mirrors alike (decision 7) and treat an
//! account that is unknown, unresolvable or deactivated as a 404 "Record
//! not found" — the answer Pleroma's `visible_for` check renders through
//! its fallback controller.
//!
//! `verify_credentials` is the account's own view of itself: [`AuthUser`]
//! plus the `read:accounts` scope, and the entity rendered with the
//! caller as its [`AccountViewer`], so the owner-only keys of the
//! `pleroma` block are in and nothing is hidden. Its additions over `show`
//! are the [`source`](crate::entities::account::AccountSource) object — the
//! raw note and fields a client edits, the posting defaults, and the size
//! of the pending follow-request queue, the one value here that is not on
//! the row and so arrives from
//! [`pending_count`](crate::models::follow::pending_count) — and the
//! top-level `also_known_as` alias list the account manages through
//! `/api/pleroma/aliases` (see
//! [`crate::api::pleroma_account_settings`]). Pleroma gates the `source`
//! block on the viewer being the account itself
//! (`AccountView.maybe_put_settings`), which is why no other route on this
//! module carries it.
//!
//! `PATCH /api/v1/accounts/update_credentials` (and the `POST` alias older
//! clients send) is Pleroma's `AccountController.update_credentials`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`):
//! [`AuthUser`] with the `write:accounts` scope, and only the fields the
//! body actually carries are written — [`MultipartForm`] parses JSON,
//! urlencoded and multipart bodies alike (decision 10) and every parameter
//! here is optional, a JSON `null` counting as absent as in Pleroma's
//! `Enum.filter(fn {_, value} -> not is_nil(value) end)`. `display_name`
//! is length-checked against
//! [`max_display_name_chars`](Config::max_display_name_chars) (422 with
//! Mastodon's wording on overflow) and stored through
//! [`sanitize_display_name`]; an empty one is stored empty, not reset to
//! the nickname. `note` is length-checked against
//! [`max_bio_chars`](Config::max_bio_chars), stored verbatim as the row's
//! `raw_bio` and rendered to `bio` by [`text_to_html`] plus
//! [`linkify_entities`] — bare-URL links, hashtag links, and mention
//! links only for mentions naming a local user of this instance (unknown
//! and remote mentions stay plain text). `locked`, `bot` and
//! `discoverable` are [`flex::opt_bool`] booleans; an unrecognized
//! boolean string is the extractor's 422. `fields_attributes` is the
//! profile metadata: a JSON array of `{name, value}` objects, an index
//! map (`{"0": {…}}`) or the form spelling
//! `fields_attributes[0][name]=…` alike — [`index_map_to_vec`] folds all
//! three shapes into the submitted list (see `stored_fields`). An
//! entry whose name and value are both empty is dropped; more than
//! [`max_profile_fields`](Config::max_profile_fields) remaining entries
//! are the 422 "Too many fields", and a name over
//! `MAX_PROFILE_FIELD_NAME_CHARS` or a value over
//! `MAX_PROFILE_FIELD_VALUE_CHARS` characters is a length 422. Two
//! jsonb columns store the pair: `raw_fields` keeps the entries as
//! typed, `fields` the names reduced to plain text and the values
//! rendered by [`text_to_html`] — so a bare URL becomes a link with the
//! standard `rel`. An empty submission clears the metadata. The answer
//! is a 200 [`Account::for_owner`] render — the owner view with its
//! `source` block, the same one `verify_credentials` answers — and every
//! accepted request runs the `after_profile_updated` hook, which the
//! later ticket that federates the actor's `Update` fills in.
//!
//! Authentication on `show` and `lookup` is optional, as in Pleroma's
//! `OAuthScopesPlug … fallback: :proceed_unauthenticated`: the routes
//! answer without a token. A token is not ignored, though: it names the
//! [`AccountViewer`] the entity is
//! rendered for, so the account's own user sees what the row hides from
//! strangers (hidden follow counts, hidden role flags, and the owner-only
//! keys of the `pleroma` block) while anyone else sees the restricted view.
//! An authenticated viewer looking at *another* account also gets their
//! [`Relationship`] embedded in `pleroma.relationship`, preloaded per
//! request; anonymous viewers and self-views keep Pleroma's empty object.
//! The [`MaybeAuthUser`] extractor is attached to both routes, so a request
//! presenting an invalid, expired or deactivated token is answered 401 "The
//! access token is invalid" instead of being silently treated as anonymous.
//!
//! `followers` and `following` are the account's relationship listings,
//! Pleroma's `.followers` and `.following`: [`MaybeAuthUser`] with the
//! `read:accounts` scope enforced on a presented user token
//! ([`require_user_scope`](MaybeAuthUser::require_user_scope)) and
//! [`PageParams`] keyset pagination (default limit 20, at most
//! [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)). They answer [`Account`]
//! entities — the accounts of the *accepted* follows, newest follow first,
//! local and remote alike — and paginate on the `follows` row: the
//! [`Link`](crate::pagination::link_header) header's `max_id`/`min_id`
//! carry [`Follow::id`](crate::models::follow::Follow::id), which is why
//! [`followers_page`](crate::models::follow::followers_page) returns
//! [`FollowPageEntry`](crate::models::follow::FollowPageEntry) rather than
//! bare rows. An authenticated viewer also gets their relationship to
//! every listed account embedded, one batched
//! [`Relationship::load_many`] per request. Privacy per Pleroma's
//! `cond`: an account with `hide_followers` (resp. `hide_follows`) answers
//! the empty array to everyone but itself — the count-hiding flags alone
//! never hide the list — while a locked account's lists stay visible,
//! pending requests excluded, to any viewer.
//!
//! `lookup` resolves its `acct` parameter through
//! [`get_by_acct`](crate::models::user::get_by_acct): a stored nickname,
//! case-insensitively, with this instance's own domain stripped for local
//! accounts. Resolving a handle this instance has never seen is left to
//! the `WebFinger` federation ticket.
//!
//! `relationships` requires auth: [`AuthUser`] plus the `read:follows`
//! scope, exactly Pleroma's plug pipeline for that action
//! (`%{scopes: ["read:follows"]}`). It reads Mastodon's `id[]` array
//! parameter — a single `id=` is accepted too, Pleroma's `List.wrap(id)` —
//! and answers one [`Relationship`] per *known* id in the order requested:
//! values that are not `UUID` text and ids that name no stored user are
//! skipped (as Pleroma's `User.get_all_by_ids` skips them), and an absent,
//! empty or wholly-unknown `id` is the empty array, never an error
//! (Pleroma returns `[]` when the parameter is missing). The flags come
//! from [`Relationship::load_many`], one batched query set per request.
//!
//! `POST /api/v1/accounts/{id}/note` is Pleroma's `AccountController.note`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`) over
//! `Pleroma.UserNote` (`lib/pleroma/user_note.ex`): [`AuthUser`] plus the
//! `write:accounts` scope, and the whole answer is the caller's
//! [`Relationship`] to the account with [`note`](Relationship::note)
//! carrying what the write left stored. The `comment` parameter is
//! required, at most 2000 characters (Mastodon's `AccountNote` limit; longer is 422 with Mastodon's changeset wording)
//! and stored verbatim — but an empty or all-whitespace one *deletes* the
//! note (the answered `note` is then `""`), which is how Mastodon clears
//! it too. An unknown, malformed or deactivated account id is the same
//! 404 as every other route here; noting *yourself* is allowed, unlike
//! Pleroma's follow-side checks.
//!
//! `POST /api/v1/accounts/{id}/remove_from_followers` is Pleroma's
//! `AccountController.remove_from_followers` over `User.remove_follower`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`,
//! `lib/pleroma/user.ex`): [`AuthUser`] plus the `follow` / `write:follows`
//! scope, and the write takes the account's accepted follow of the caller
//! out of the table through [`remove_follower`](crate::models::follow::remove_follower),
//! counters adjusted in the same transaction. The answer is the caller's
//! [`Relationship`] to the account, so `followed_by` reads `false`
//! afterwards. An account that is not a follower — no row, a `pending`
//! request of its own (those wait on the `/api/v1/follow_requests` answers,
//! this route never touches them), or one the caller follows only — is no
//! error: the unchanged [`Relationship`] answers it with 200. The malformed,
//! unknown or deactivated id is the usual 404, resolved ahead of the guard
//! that answers the caller's *own* id with 422 `{"error":"Can not remove
//! yourself from followers"}`. A follower actually removed is the moment
//! federation will owe that account a `Reject` for its `Follow`; the later
//! federation ticket wires the delivery into this module's
//! `after_follower_removed` hook.

use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Uri},
    routing::{get, patch, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::debug;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::{AuthUser, MaybeAuthUser},
    config::Config,
    entities::{
        account::{Account, AccountViewer, local_url},
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    extract::{ApiBody, flex, index_map_to_vec, multipart::MultipartForm},
    models::{
        account_note,
        follow::{self, Follow},
        user::{self, Flag, ProfileFields, User},
    },
    oauth::scopes::{FOLLOW, READ_ACCOUNTS, READ_FOLLOWS, WRITE_ACCOUNTS, WRITE_FOLLOWS},
    pagination::{PageParams, link_header, set_link_header},
    state::AppState,
    util::{
        entities::{MentionRef, extract_mentions, linkify_entities},
        format::text_to_html,
        id::parse_id,
        sanitize::{sanitize_display_name, strip_html},
    },
};

/// The 400 body message when the `acct` query parameter is absent or blank
/// (Mastodon's `ParameterMissing` wording).
const MISSING_ACCT: &str = "Missing parameter: acct";

/// The longest `comment` `POST /api/v1/accounts/{id}/note` accepts
/// (Mastodon's `AccountNote` limit).
const MAX_COMMENT_CHARS: usize = 2000;

/// The longest `fields_attributes` name accepted, in characters
/// (Pleroma's `[:instance, :account_field_name_length]` default, the
/// same cap Mastodon stores); longer is the 422 in [`check_field_len`]'s
/// wording.
const MAX_PROFILE_FIELD_NAME_CHARS: usize = 255;

/// The longest `fields_attributes` value accepted, in characters
/// (Mastodon's `account_field_value_length`); longer is the 422 in
/// [`check_field_len`]'s wording.
const MAX_PROFILE_FIELD_VALUE_CHARS: usize = 2047;

/// The 422 body `PATCH /api/v1/accounts/update_credentials` answers when
/// `fields_attributes` carries more entries than
/// [`max_profile_fields`](Config::max_profile_fields); this endpoint's
/// ticket pins the wording.
const TOO_MANY_FIELDS: &str = "Too many fields";

/// The 422 message `POST /api/v1/accounts/{id}/remove_from_followers`
/// answers when the path names the caller: this endpoint's ticket pins the
/// wording for the action.
const SELF_REMOVE: &str = "Can not remove yourself from followers";

/// The `Link` header's query parameters carry nothing but the cursor: the
/// relationship listings have no filter to repeat on either link (the
/// scrobbles listing's constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1/accounts` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/accounts/verify_credentials", get(verify_credentials))
        .route(
            "/accounts/update_credentials",
            patch(update_credentials).post(update_credentials),
        )
        .route("/accounts/relationships", get(relationships))
        .route("/accounts/lookup", get(lookup_account))
        .route("/accounts/{id}/followers", get(list_followers))
        .route("/accounts/{id}/following", get(list_following))
        .route("/accounts/{id}/note", post(set_note))
        .route(
            "/accounts/{id}/remove_from_followers",
            post(remove_from_followers),
        )
        .route("/accounts/{id}", get(show_account))
}

/// `GET /api/v1/accounts/verify_credentials` — the caller's own account,
/// rendered as they see it: the owner view of [`Account::from_user_for`]
/// (hidden counters visible, the owner-only `pleroma` keys present) with
/// the `source` block [`Account::for_owner`] attaches.
///
/// The only thing that block needs beyond the token's row is the pending
/// follow-request queue, counted by
/// [`pending_count`](crate::models::follow::pending_count).
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token is app-only or does not
/// grant `read:accounts`. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn verify_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Account>> {
    auth.require(READ_ACCOUNTS)?;
    owner_view(&state, &auth.0).await
}

/// The owner render both credentials endpoints answer — the self view of
/// [`Account::for_owner`] with its [`source`](Account::source) block (the
/// pending queue counted by
/// [`pending_count`](crate::models::follow::pending_count) included) and a
/// stored move target attached as the nested `moved` entity (see
/// [`with_moved`]).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn owner_view(state: &AppState, owner: &User) -> AppResult<Json<Account>> {
    let requests = follow::pending_count(&state.db, owner.id).await?;
    let account = Account::for_owner(owner, &state.config, requests);
    Ok(Json(with_moved(state, account, owner).await?))
}

/// Resolves [`moved_to_ap_id`](User::moved_to_ap_id) for one rendered
/// [`Account`]: when the row names a move target this instance has a row
/// for, [`Account::with_moved`] nests the target's plain entity. A move
/// to an actor id not mirrored here keeps `moved: null` (the alias and
/// move flows are what create that row; federation of a remote `Move`
/// arrives with a later ticket).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn with_moved(state: &AppState, account: Account, user: &User) -> AppResult<Account> {
    let Some(ap_id) = user.moved_to_ap_id.as_deref() else {
        return Ok(account);
    };
    Ok(match user::get_by_ap_id(&state.db, ap_id).await? {
        Some(target) => account.with_moved(&target, &state.config),
        None => account,
    })
}

/// The request parameters of `PATCH /api/v1/accounts/update_credentials`.
///
/// Every field is optional and only the ones present in the body are
/// written; a JSON `null` reads as absent (Pleroma drops nil-valued
/// parameters before the changeset). The booleans arrive through
/// [`flex::opt_bool`], so a form's `"true"`/`"on"`/`"1"` spells the same
/// as JSON `true`, and an unknown string is the extractor's 422.
#[derive(Debug, Deserialize)]
struct ProfileParams {
    /// The display name to store: at most
    /// [`max_display_name_chars`](Config::max_display_name_chars)
    /// characters, then reduced to one line of plain text (see
    /// [`sanitized_name`]).
    #[serde(default)]
    display_name: Option<String>,
    /// The profile note: source text for `raw_bio`, at most
    /// [`max_bio_chars`](Config::max_bio_chars) characters, rendered to
    /// `bio` (see [`render_note`]).
    #[serde(default)]
    note: Option<String>,
    /// Whether follow requests need manual approval.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    locked: Option<bool>,
    /// Whether the account is flagged as automated.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    bot: Option<bool>,
    /// Whether the account opts into discovery.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    discoverable: Option<bool>,
    /// The profile metadata: Mastodon's `fields_attributes`, a JSON array
    /// of `{name, value}` objects, an object keyed by index or the
    /// `fields_attributes[0][name]=…` spelling of form and multipart
    /// bodies alike — read as `Value` so every shape survives the
    /// extractor and [`stored_fields`] folds them into one list.
    #[serde(default)]
    fields_attributes: Option<Value>,
}

/// `PATCH /api/v1/accounts/update_credentials` (and the `POST` alias of
/// the older clients) — change the caller's own profile, the fields the
/// body carries and nothing else, answering the updated account as
/// [`verify_credentials`] answers it.
///
/// [`MultipartForm`] takes JSON, urlencoded and multipart bodies alike
/// (decision 10); file parts (`avatar` and friends) belong to later
/// tickets and are ignored. [`profile_fields`] validates what is present
/// and merges it over the token's row,
/// [`update_profile`](crate::models::user::update_profile) writes the
/// eight columns in one statement, and the fresh row answers through
/// [`owner_view`] — the write-behind render that states the stored fact
/// rather than trusting the request. A body that carries none of these
/// fields is the accepted no-op: 200 with the unchanged entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] when
/// `display_name` or `note` is too long, when `fields_attributes` breaks
/// one of its limits, or from the extractor for an invalid boolean
/// string. Any database failure arrives through `?` as
/// [`AppError::Database`], and the [`AppError::Internal`] arm only
/// documents that the row behind a just-authenticated token is gone.
async fn update_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<ProfileParams>,
) -> AppResult<Json<Account>> {
    auth.require(WRITE_ACCOUNTS)?;
    let fields = profile_fields(&state, &auth.0, form.fields).await?;
    let updated = user::update_profile(&state.db, auth.0.id, &fields)
        .await?
        .ok_or_else(|| AppError::Internal("profile update matched no row".to_owned()))?;
    after_profile_updated(&state, &updated);
    owner_view(&state, &updated).await
}

/// Validates the present profile parameters and merges them over the
/// stored row: the eight columns
/// [`update_profile`](crate::models::user::update_profile) will write,
/// with fields absent from the request carrying the value they already
/// hold and fields present-but-blank carrying the blank (an empty
/// `display_name` is stored as `""`, not reset to the nickname — this
/// endpoint's ticket pins that against Pleroma's registration-side
/// fallback). A `note` over [`max_bio_chars`](Config::max_bio_chars)
/// characters (counted in characters, not bytes) is the 422 mirroring the
/// name message; a submitted `fields_attributes` is validated and
/// rendered by [`stored_fields`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] from [`sanitized_name`], the note length
/// check or [`stored_fields`]; [`render_note`] adds any database failure
/// as [`AppError::Database`].
async fn profile_fields(
    state: &AppState,
    current: &User,
    params: ProfileParams,
) -> AppResult<ProfileFields> {
    let name = params
        .display_name
        .as_deref()
        .map(|raw| sanitized_name(raw, state.config.max_display_name_chars))
        .transpose()?;
    if params
        .note
        .as_deref()
        .is_some_and(|text| text.chars().count() > state.config.max_bio_chars)
    {
        return Err(AppError::Unprocessable(format!(
            "Bio is too long (maximum is {} characters)",
            state.config.max_bio_chars
        )));
    }
    let (bio, raw_bio) = match params.note.as_deref() {
        Some(raw) => (render_note(state, raw).await?, raw.to_owned()),
        None => (current.bio.clone(), current.raw_bio.clone()),
    };
    let (fields, raw_fields) = match params.fields_attributes {
        Some(submitted) => stored_fields(submitted, state.config.max_profile_fields)?,
        None => (current.fields.clone(), current.raw_fields.clone()),
    };
    Ok(ProfileFields {
        name: name.unwrap_or_else(|| current.name.clone()),
        bio,
        raw_bio,
        locked: params.locked.map_or(current.locked, Flag::from),
        bot: params.bot.map_or(current.bot, Flag::from),
        discoverable: params.discoverable.map_or(current.discoverable, Flag::from),
        fields,
        raw_fields,
    })
}

/// The `fields` and `raw_fields` jsonb a submitted `fields_attributes`
/// stores: [`index_map_to_vec`] folds the JSON array, index-object and
/// form-spelled bodies into one list of `{name, value}` entries (a key
/// neither holds reads as the empty string, so entries that are not
/// objects are empty ones), and an entry with both name and value empty
/// is dropped — Pleroma's `put_fields` filtering, widened to the whole
/// entry. What survives must be at most `max_fields` entries (the 422
/// [`TOO_MANY_FIELDS`]) with each name within
/// [`MAX_PROFILE_FIELD_NAME_CHARS`] and each value within
/// [`MAX_PROFILE_FIELD_VALUE_CHARS`] characters, counted on the raw text
/// like Pleroma's `valid_field?`. The stored pair is the raw entry in
/// `raw_fields` and, in `fields`, the name reduced to plain text by
/// [`strip_html`] and the value rendered by [`text_to_html`] — bare URLs
/// become links carrying the standard `rel`. An empty submission is an
/// empty pair: the metadata, cleared.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the count and both length limits.
fn stored_fields(submitted: Value, max_fields: usize) -> AppResult<(Value, Value)> {
    let mut entries = Vec::new();
    for entry in index_map_to_vec(submitted) {
        let name = field_text(&entry, "name");
        let value = field_text(&entry, "value");
        if name.is_empty() && value.is_empty() {
            continue;
        }
        entries.push((name, value));
    }
    if entries.len() > max_fields {
        return Err(AppError::Unprocessable(TOO_MANY_FIELDS.to_owned()));
    }
    let mut rendered = Vec::with_capacity(entries.len());
    let mut raw = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        check_field_len("Field name", &name, MAX_PROFILE_FIELD_NAME_CHARS)?;
        check_field_len("Field value", &value, MAX_PROFILE_FIELD_VALUE_CHARS)?;
        rendered.push(json!({
            "name": strip_html(&name),
            "value": text_to_html(&value),
        }));
        raw.push(json!({"name": name, "value": value}));
    }
    Ok((Value::Array(rendered), Value::Array(raw)))
}

/// The string one submitted `fields_attributes` entry holds under `key`:
/// a missing key or a non-string value is the empty string, which is how
/// a form that spelled only `fields_attributes[1][name]` leaves the
/// value.
fn field_text(entry: &Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The 422 mirroring the name and bio messages when one profile field's
/// `text` is longer than `max_chars` characters (counted in characters,
/// not bytes).
fn check_field_len(what: &str, text: &str, max_chars: usize) -> AppResult<()> {
    if text.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "{what} is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(())
}

/// The display name to store: at most `max_chars` characters (in
/// characters, not bytes) of the submitted text — the limit is checked
/// before sanitizing, like Pleroma's changeset checks the parameter —
/// and then [`sanitize_display_name`]'s one line of plain text. An empty
/// string survives as empty.
fn sanitized_name(raw: &str, max_chars: usize) -> AppResult<String> {
    if raw.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "Name is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(sanitize_display_name(raw))
}

/// Renders one submitted note into the `bio` column: [`text_to_html`]
/// (escape, bare-URL links, paragraphs) and then [`linkify_entities`]
/// over its output, so hashtags link to `{base_url}/tags/{tag}` and
/// mentions naming a local user of this instance become their `h-card`
/// anchors — unknown and remote mentions stay the plain text they were
/// written as.
async fn render_note(state: &AppState, raw: &str) -> AppResult<String> {
    let html = text_to_html(raw);
    let urls = local_mention_urls(&state.db, &state.config, &html).await?;
    let resolver = |mention: &MentionRef| urls.get(&mention_key(mention)).cloned();
    Ok(linkify_entities(&html, &resolver, &state.config.base_url))
}

/// The profile URLs of every mention [`extract_mentions`] finds in
/// `text` that names a local user, keyed by [`mention_key`]. Lookups are
/// citext, so `@ALICE` finds `alice`; the [`local`](User::local) flag is
/// the guard that keeps a remote mirror out of the table whatever its
/// stored nickname looks like.
async fn local_mention_urls(
    db: &PgPool,
    config: &Config,
    text: &str,
) -> AppResult<HashMap<String, String>> {
    let mut urls = HashMap::new();
    for mention in extract_mentions(text) {
        if !names_own_domain(config, &mention) {
            continue;
        }
        if let Some(found) = user::get_by_nickname(db, &mention.nickname)
            .await?
            .filter(|found| found.local.get())
        {
            let _seen = urls.insert(mention_key(&mention), local_url(config, &found.nickname));
        }
    }
    Ok(urls)
}

/// The lookup key of one mention: the account it names, lowercased —
/// `nick` for a bare one, `nick@domain` when the reference spelled the
/// domain out (the case-fold
/// [`mention_html`](crate::util::entities::mention_html) never needs
/// because it renders the written spelling).
fn mention_key(mention: &MentionRef) -> String {
    mention
        .domain
        .as_ref()
        .map_or_else(
            || mention.nickname.clone(),
            |domain| format!("{}@{}", mention.nickname, domain),
        )
        .to_lowercase()
}

/// Whether a mention can name a local user at all: a bare `@nick`, or a
/// `@nick@domain` whose domain is this instance's own
/// ([`Config::domain`], compared case-insensitively). Any other domain
/// names a remote account, which this render never links.
fn names_own_domain(config: &Config, mention: &MentionRef) -> bool {
    mention
        .domain
        .as_deref()
        .is_none_or(|domain| domain.eq_ignore_ascii_case(config.domain()))
}

/// Post-update hook of [`update_credentials`], run once per accepted
/// profile write with the updated row: the federation ticket that answers
/// a profile change with an `Update` of the actor wires the delivery in
/// here — the actor is [`User::ap_id`], and the queueing happens through
/// `state` (decision 11).
///
/// Intentionally empty until then, like [`after_follower_removed`]:
/// Pleroma's `update_credentials` federates through the pipeline inline,
/// and this server's delivery queue is a later ticket.
const fn after_profile_updated(_state: &AppState, _user: &User) {}

/// `GET /api/v1/accounts/{id}` — the account whose id is `id`, local or
/// remote. An id that is not `UUID` text is a 404, not a 400, like every
/// other miss (see [`account_or_404`]).
///
/// The viewer behind an optional token is the account the entity is
/// rendered for; without one the anonymous view is answered. A viewer who
/// is looking at someone else gets their relationship to the account
/// embedded (see [`render_account`]).
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown or deactivated id;
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn show_account(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Account>> {
    let found = account_or_404(&state, &id).await?;
    Ok(Json(render_account(&state, viewer.as_ref(), &found).await?))
}

/// The query parameters of `GET /api/v1/accounts/lookup`. The absent and
/// blank cases are the documented 400 and 404, which the type cannot
/// express, so the field is optional here (the `webfinger` handler's
/// pattern).
#[derive(Debug, Deserialize)]
struct LookupQuery {
    /// The handle to resolve: `nick` or `nick@domain`.
    acct: Option<String>,
}

/// `GET /api/v1/accounts/lookup?acct=…` — the account a handle names,
/// resolved by [`get_by_acct`](crate::models::user::get_by_acct). The
/// viewer behind an optional token is the account the entity is rendered
/// for, as in `show`.
///
/// # Errors
///
/// [`AppError::BadRequest`] when `acct` is absent or blank;
/// [`AppError::NotFound`] when it names no live account (unknown, on a
/// foreign domain not mirrored here, or deactivated);
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn lookup_account(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    Query(query): Query<LookupQuery>,
) -> AppResult<Json<Account>> {
    let Some(acct) = query
        .acct
        .as_deref()
        .map(str::trim)
        .filter(|acct| !acct.is_empty())
    else {
        return Err(AppError::BadRequest(MISSING_ACCT.to_owned()));
    };
    let found = user::get_by_acct(&state.db, &state.config, acct)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)?;
    Ok(Json(render_account(&state, viewer.as_ref(), &found).await?))
}

/// `GET /api/v1/accounts/relationships?id[]=a&id[]=b` — the caller's
/// relationship to each requested account, as
/// [`Relationship::load_many`] computes it, in the order requested.
///
/// The `id` values are read by [`requested_ids`]; ones that are not `UUID`
/// text and ones that name no stored user are dropped (Pleroma's
/// `User.get_all_by_ids` simply finds nothing for them), so the response
/// holds only the known ids in requested order. No usable `id` at all is
/// the empty array, Pleroma's explicit `json(conn, [])` for a missing
/// parameter, not a `Missing parameter` error.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:follows`. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn relationships(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Relationship>>> {
    auth.require(READ_FOLLOWS)?;
    let requested = requested_ids(uri.query());
    if requested.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let targets = known_ids(&state, &requested).await?;
    Ok(Json(
        Relationship::load_many(&state.db, auth.0.id, &targets).await?,
    ))
}

/// The `id` parameters of a raw query string, as parsed values in
/// occurrence order: both the bracketed array form Mastodon clients send
/// (`id[]=a&id[]=b`, percent-encodings included) and a single `id=a`
/// (Pleroma's `List.wrap(id)`), or any mix of the two. Values that are not
/// `UUID` text are dropped here, already skipped.
fn requested_ids(query: Option<&str>) -> Vec<Uuid> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _value)| key == "id" || key == "id[]")
        .filter_map(|(_key, value)| parse_id(&value))
        .collect()
}

/// The subset of `requested` that names a stored user, kept in the
/// requested order ([`user::get_many`] answers in id order, so membership
/// is decided from its rows). One query per request whatever the batch
/// size.
async fn known_ids(state: &AppState, requested: &[Uuid]) -> AppResult<Vec<Uuid>> {
    let known: HashSet<Uuid> = user::get_many(&state.db, requested)
        .await?
        .into_iter()
        .map(|found| found.id)
        .collect();
    Ok(requested
        .iter()
        .copied()
        .filter(|id| known.contains(id))
        .collect())
}

/// The request parameters of `POST /api/v1/accounts/{id}/note`: Pleroma's
/// `%{comment: comment}` pattern match — the field is required (an absent
/// one is the extractor's 422), and the empty string is the documented
/// delete, so the type carries no default.
#[derive(Debug, Deserialize)]
struct NoteParams {
    /// The note text, verbatim up to [`MAX_COMMENT_CHARS`] characters;
    /// empty or all whitespace clears the note.
    comment: String,
}

/// `POST /api/v1/accounts/{id}/note` — write the caller's private note on
/// the account and answer their [`Relationship`] to it, with
/// [`note`](Relationship::note) carrying what the write left stored.
///
/// [`account_note::set`] is the write: a non-blank `comment` replaces the
/// caller's note on the target, a blank one deletes it (Pleroma's
/// `UserNote.create` stores blanks, this endpoint's ticket deletes on
/// them; both read back as the same relationship). The account is resolved
/// by [`account_or_404`], and noting yourself is allowed — unlike
/// Pleroma's follow-side checks, `note` has no self guard. The
/// [`Relationship::load_many`] after the write is the render of Pleroma's
/// `relationship.json`: it reads the row back, so the answer states the
/// stored fact rather than trusting the request.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] when
/// `comment` is absent or longer than [`MAX_COMMENT_CHARS`] characters;
/// [`AppError::NotFound`] for a malformed, unknown or deactivated account
/// id. Any database failure arrives through `?` as [`AppError::Database`].
async fn set_note(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<NoteParams>,
) -> AppResult<Json<Relationship>> {
    auth.require(WRITE_ACCOUNTS)?;
    // Validation before the lookup, as the list endpoints do: a
    // too-long comment is the 422 whatever the id names.
    if params.comment.chars().count() > MAX_COMMENT_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Comment is too long (maximum is {MAX_COMMENT_CHARS} characters)"
        )));
    }
    let target = account_or_404(&state, &id).await?;
    account_note::set(&state.db, auth.0.id, target.id, &params.comment).await?;
    one_relationship(&state, auth.0.id, target.id).await
}

/// `POST /api/v1/accounts/{id}/remove_from_followers` — drop the account
/// from the caller's followers: the accepted follow of the caller it holds
/// is removed ([`remove_follower`](crate::models::follow::remove_follower),
/// counters adjusted in the same transaction) and the caller's
/// [`Relationship`] to it answered, `followed_by: false` afterwards.
///
/// Not being a follower is not an error: an account with no row, one whose
/// follow of the caller is still `pending` (requests are answered through
/// `/api/v1/follow_requests`, never removed here) or one the caller follows
/// without being followed back changes nothing, and the unchanged
/// [`Relationship`] answers the call with 200. The account is resolved
/// before the self guard, as Pleroma's plug order does: a malformed,
/// unknown or deactivated id is the shared 404 whatever it would have
/// meant, and only a live account that turns out to be the caller is the
/// 422.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::NotFound`] for a
/// malformed, unknown or deactivated account id; [`AppError::Unprocessable`]
/// with [`SELF_REMOVE`] when the id names the caller. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn remove_from_followers(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let follower = account_or_404(&state, &id).await?;
    if follower.id == auth.0.id {
        return Err(AppError::Unprocessable(SELF_REMOVE.to_owned()));
    }
    let removed = follow::remove_follower(&state.db, auth.0.id, follower.id).await?;
    if let Some(row) = removed.as_ref() {
        debug!(
            follow = %row.id,
            user = %auth.0.id,
            "follower removed"
        );
        after_follower_removed(&state, &auth.0, row);
    }
    one_relationship(&state, auth.0.id, follower.id).await
}

/// The caller's [`Relationship`] to one account, rendered after a write:
/// [`Relationship::load_many`] over the single target, so the answer states
/// the stored fact rather than trusting the request (the write-behind read
/// of Pleroma's `relationship.json`). Shared by [`set_note`] and
/// [`remove_from_followers`].
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`]; the
/// [`AppError::Internal`] arm only documents that the loader answers one row
/// per requested id, which a single-target batch always does.
async fn one_relationship(
    state: &AppState,
    viewer_id: Uuid,
    target_id: Uuid,
) -> AppResult<Json<Relationship>> {
    let loaded = Relationship::load_many(&state.db, viewer_id, &[target_id]).await?;
    let relationship = loaded.into_iter().next().ok_or_else(|| {
        AppError::Internal("the relationship loader skipped its only target".to_owned())
    })?;
    Ok(Json(relationship))
}

/// Post-removal hook of [`remove_from_followers`], run once per actually
/// removed follower: the federation ticket that answers a dropped follower
/// with a `Reject` for its `Follow` wires the delivery in here — the
/// activity to reject is [`Follow::ap_id`], its sender [`Follow::follower_id`],
/// and the queueing happens through `state` (decision 11).
///
/// Intentionally empty until then, like the follow-request answers'
/// [`after_follow_request_resolved`](crate::api::follow_requests): Pleroma's
/// `User.remove_follower` delivers the `Reject` inline, and this server's
/// delivery queue is a later ticket.
const fn after_follower_removed(_state: &AppState, _followee: &User, _removed: &Follow) {}

/// `GET /api/v1/accounts/{id}/followers` — the accounts that follow `id`
/// with an accepted relationship, newest follow first, one [`PageParams`]
/// page at a time (limit 20 by default, at most
/// [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)). Auth is optional; a
/// presented user token must grant `read:accounts`, as in Pleroma's plug
/// for this action, and the viewer it names decides whose [`Relationship`]
/// the entries embed.
///
/// Pleroma's `followers` `cond`: an account that hides its followers is
/// answered the empty array — 200, no `Link` header — unless the viewer is
/// that account itself; `hide_followers_count` alone does not hide the
/// list, only the counter the entities carry. A locked account's follower
/// list is listed as any other's, its unanswered requests left out by the
/// model's `accepted` filter.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the authenticated token lacks `read:accounts`;
/// [`AppError::Unauthorized`] from the extractor for an invalid token;
/// [`AppError::NotFound`] for a malformed, unknown or deactivated account
/// id; [`AppError::BadRequest`] from [`PageParams`] for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_followers(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    if target.hide_followers.get() && !viewer_is(viewer, &target) {
        return Ok(empty_page());
    }
    let entries = follow::followers_page(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// `GET /api/v1/accounts/{id}/following` — the accounts `id` follows with
/// an accepted relationship, newest follow first. The exact counterpart
/// of [`list_followers`]: the same optional auth and `read:accounts`
/// gate, the same [`PageParams`] keyset pagination on the `follows` row,
/// and Pleroma's `following` `cond` hiding the list behind
/// [`hide_follows`](User::hide_follows) from everyone but the account
/// itself.
///
/// # Errors
///
/// Exactly those of [`list_followers`].
async fn list_following(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    if target.hide_follows.get() && !viewer_is(viewer, &target) {
        return Ok(empty_page());
    }
    let entries = follow::following_page(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// Renders `found` for `viewer` (who may be `None`), preloading the
/// viewer's relationship to the account when there is one to embed: a
/// viewer who is not the account itself, answered by a single-target
/// [`Relationship::load_many`]. Anyone else — anonymous, self — renders
/// with no preload, which keeps `pleroma.relationship` the empty object.
/// A stored move target is attached as the nested `moved` entity (see
/// [`with_moved`]).
async fn render_account(
    state: &AppState,
    viewer: AccountViewer<'_>,
    found: &User,
) -> AppResult<Account> {
    let relationships = preload_relationship(state, viewer, found).await?;
    let account = Account::from_user_for(viewer, found, &state.config, relationships.as_ref());
    with_moved(state, account, found).await
}

/// The single-account [`RelationshipMap`] for the `show`/`lookup` view:
/// [`Some`] only for a viewer looking at someone else, `None` when there
/// is nothing to embed (and so nothing to query).
async fn preload_relationship(
    state: &AppState,
    viewer: AccountViewer<'_>,
    found: &User,
) -> AppResult<Option<RelationshipMap>> {
    let Some(who) = viewer.filter(|who| who.id != found.id) else {
        return Ok(None);
    };
    let loaded =
        Relationship::load_many(&state.db, who.id, std::slice::from_ref(&found.id)).await?;
    let relationship = loaded.into_iter().next();
    Ok(relationship.map(|rel| RelationshipMap::from([(found.id, rel)])))
}

/// Whether `viewer` is looking at the account `target` itself — Pleroma's
/// `user.id == for_user.id` check in the `followers`/`following` `cond`,
/// by primary key. An anonymous request is never the target.
fn viewer_is(viewer: AccountViewer<'_>, target: &User) -> bool {
    viewer.is_some_and(|who| who.id == target.id)
}

/// The answer to a hidden relationship listing: the empty array as a plain
/// 200 with no `Link` header at all — Pleroma's `[]` branch, where the
/// page never got a cursor to link from.
fn empty_page() -> (HeaderMap, Json<Vec<Account>>) {
    (HeaderMap::new(), Json(Vec::new()))
}

/// Renders one fetched relationship page for `viewer`: every entry's
/// [`User`] as an [`Account`] with the viewer's relationship embedded
/// where one is preloaded (see [`preload_page_relationships`]), under the
/// `Link` header [`page_link`] derives from the relationship ids.
async fn page_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[follow::FollowPageEntry],
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let relationships = preload_page_relationships(state, viewer, entries).await?;
    let link = page_link(&state.config.base_url, path, entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = entries
        .iter()
        .map(|entry| {
            Account::from_user_for(viewer, &entry.user, &state.config, relationships.as_ref())
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// The [`RelationshipMap`] for one relationship page: the viewer's
/// relationship to every listed account in a single batched
/// [`Relationship::load_many`], keyed by target id (which is how
/// [`Account::from_user_for`] reads its preload). [`None`] only for an
/// anonymous viewer, whose entries keep Pleroma's empty
/// `pleroma.relationship` object and so need no query at all.
async fn preload_page_relationships(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[follow::FollowPageEntry],
) -> AppResult<Option<RelationshipMap>> {
    let Some(who) = viewer else {
        return Ok(None);
    };
    let targets: Vec<Uuid> = entries.iter().map(|entry| entry.user.id).collect();
    let loaded = Relationship::load_many(&state.db, who.id, &targets).await?;
    let map = loaded
        .into_iter()
        .zip(targets)
        .map(|(relationship, target)| (target, relationship))
        .collect();
    Ok(Some(map))
}

/// The `Link` header value for a fetched relationship page: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`follow::FollowPageEntry::follow_id`] — the cursors key on the
/// `follows` row, not on the users — and `None` for an empty page.
fn page_link(base_url: &str, path: &str, entries: &[follow::FollowPageEntry]) -> Option<String> {
    let newest = entries.first()?.follow_id.to_string();
    let oldest = entries.last()?.follow_id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of the ids the fixtures pass around.
    const ID_A: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// A second id, distinct from [`ID_A`].
    const ID_B: &str = "01934567-89ab-7cde-f012-3456789abcdf";

    /// The ids [`requested_ids`] found in `query`, as strings, so the
    /// assertions read like the query they came from.
    fn parsed(query: Option<&str>) -> Vec<String> {
        requested_ids(query)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn array_params_are_read_in_order() {
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "Mastodon's repeated bracketed params, in the order sent"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&other=x&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "unrelated params in between are ignored"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_A}"))),
            vec![ID_A, ID_A],
            "a repeated id is kept twice: the response answers what was asked"
        );
    }

    #[test]
    fn a_single_id_param_is_accepted() {
        assert_eq!(
            parsed(Some(&format!("id={ID_B}"))),
            vec![ID_B],
            "Pleroma's List.wrap accepts the single form"
        );
        assert_eq!(
            parsed(Some(&format!("id={ID_B}&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "and the two forms merge in occurrence order"
        );
    }

    #[test]
    fn values_are_percent_decoded_and_garbage_is_skipped() {
        assert_eq!(
            parsed(Some(&format!("id%5B%5D={ID_A}"))),
            vec![ID_A],
            "the bracketed key may arrive percent-encoded"
        );
        assert_eq!(
            parsed(Some("id[]=not-an-id")),
            Vec::<String>::new(),
            "a value that is not id text is skipped, not an error"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_B}&id=oops&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "one bad value does not sink its neighbours"
        );
    }

    #[test]
    fn nothing_usable_is_the_empty_list() {
        for query in [None, Some(""), Some("id="), Some("other=1"), Some("id")] {
            assert_eq!(
                parsed(query),
                Vec::<String>::new(),
                "`{query:?}` carries no usable id"
            );
        }
    }

    /// Builds a `MentionRef` for the key and domain helpers.
    fn mention(nickname: &str, domain: Option<&str>) -> MentionRef {
        MentionRef {
            nickname: nickname.to_owned(),
            domain: domain.map(str::to_owned),
        }
    }

    #[test]
    fn display_name_limit_is_counted_in_characters() {
        assert_eq!(
            sanitized_name("abcdé", 5).unwrap(),
            "abcdé",
            "five characters fit, bytes are not counted"
        );
        let err = sanitized_name("abcdeé", 5).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Name is too long (maximum is 5 characters)",
            "the pinned wording carries the configured limit"
        );
    }

    #[test]
    fn display_name_is_reduced_to_one_line_of_text() {
        assert_eq!(
            sanitized_name("  <b>Bob</b><br>the Builder ", 50).unwrap(),
            "Bob the Builder",
            "markup and control characters collapse to spaces"
        );
        assert_eq!(
            sanitized_name("", 50).unwrap(),
            "",
            "the empty string is stored empty, not reset to the nickname"
        );
    }

    #[test]
    fn mention_keys_fold_case_across_domains() {
        assert_eq!(mention_key(&mention("Alice", None)), "alice", "a bare nick");
        assert_eq!(
            mention_key(&mention("Bob", Some("Remote.Example"))),
            "bob@remote.example",
            "the spelled-out domain joins the key, lowercased"
        );
    }

    #[test]
    fn only_bare_or_own_domain_mentions_can_be_local() {
        let config = Config::for_tests();
        assert!(
            names_own_domain(&config, &mention("alice", None)),
            "a bare nick may name a local user"
        );
        assert!(
            names_own_domain(&config, &mention("alice", Some("TEST.example"))),
            "and so may one spelled with this instance's own domain"
        );
        assert!(
            !names_own_domain(&config, &mention("bob", Some("elsewhere.example"))),
            "another domain never names a local user"
        );
    }
}
