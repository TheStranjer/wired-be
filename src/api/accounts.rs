//! The account endpoints: the single-account reads and listings, the
//! relationship batch, and the notes write.
//!
//! `GET /api/v1/accounts/verify_credentials`, `GET /api/v1/accounts/{id}`,
//! `…/lookup`, `…/relationships` and the relationship listings
//! `…/{id}/followers` and `…/{id}/following` are the reads;
//! `POST /api/v1/accounts/{id}/note` and `…/{id}/remove_from_followers` are
//! the writes.
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
//!
//! `POST /api/v1/accounts` is open-mode registration: Pleroma's
//! `AccountController.create` (`lib/pleroma/web/mastodon_api/controllers/
//! account_controller.ex`) over `Registration.register_user`
//! (`lib/pleroma/web/registration.ex`). Like Pleroma's `OAuthPlug` the
//! route authenticates with [`AppAuth`] — any valid bearer token,
//! app-only (`client_credentials`) or user-bound — plus the
//! `write:accounts` scope, and a caller who already holds a user token
//! may register another account (Pleroma permits it). While
//! `REGISTRATIONS_OPEN` is false the route answers 403 `{"error":
//! "Registration is disabled"}`; Pleroma's 2.10 `create` action carries
//! no such string (closed registrations there mean "return with an invite
//! token"), so this endpoint's ticket pins the wording. The body is
//! `username`, `email`, `password`, `agreement` (which must be `true`,
//! else 422), `locale`, `reason`, `fullname` (the display name) and
//! `bio`; `locale` and `confirm` are accepted and ignored, as Pleroma
//! reads neither on this path. The user lands through
//! [`create_local_user`]
//! — every validation failure arrives as its 422 whose `error` value is
//! the JSON string of the field map — and `reason` is stored in
//! `users.registration_reason` for the later approval flow. Success mints
//! a fresh user token for the issuing app carrying the presenting
//! token's scopes (Pleroma's `OAuthController.login(user, app, …)`) and
//! answers the token JSON: `token_type`, `access_token`, `scope`,
//! `created_at` (unix seconds), the token row's `id`, `expires_in` and
//! `refresh_token`.

use std::{collections::HashSet, time::Duration};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Uri},
    routing::{get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::debug;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::{AppAuth, AuthUser, MaybeAuthUser, require_scope},
    entities::{
        account::{Account, AccountViewer},
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::{
        account_note,
        follow::{self, Follow},
        oauth_app::OauthApp,
        oauth_token::{self, NewToken, OauthToken},
        user::{
            self, User,
            register::{RegisterParams, create_local_user},
        },
    },
    oauth::scopes::{FOLLOW, READ_ACCOUNTS, READ_FOLLOWS, WRITE_ACCOUNTS, WRITE_FOLLOWS},
    pagination::{PageParams, link_header, set_link_header},
    state::AppState,
    util::id::parse_id,
};

/// The 400 body message when the `acct` query parameter is absent or blank
/// (Mastodon's `ParameterMissing` wording).
const MISSING_ACCT: &str = "Missing parameter: acct";

/// The longest `comment` `POST /api/v1/accounts/{id}/note` accepts
/// (Mastodon's `AccountNote` limit).
const MAX_COMMENT_CHARS: usize = 2000;

/// The 422 message `POST /api/v1/accounts/{id}/remove_from_followers`
/// answers when the path names the caller: this endpoint's ticket pins the
/// wording for the action.
const SELF_REMOVE: &str = "Can not remove yourself from followers";

/// The `Link` header's query parameters carry nothing but the cursor: the
/// relationship listings have no filter to repeat on either link (the
/// scrobbles listing's constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// `token_type` of the registration answer: issued tokens are bearer
/// credentials, as on `/oauth/token` (see [`crate::api::oauth`]).
const TOKEN_TYPE_BEARER: &str = "Bearer";

/// The 403 body message `POST /api/v1/accounts` answers while
/// `REGISTRATIONS_OPEN` is false. Pleroma's 2.10 `create` action has no
/// such string (closed registrations there route to invite tokens), so
/// this endpoint's ticket pins this wording.
const REGISTRATIONS_CLOSED: &str = "Registration is disabled";

/// The 422 body message `POST /api/v1/accounts` answers when `agreement`
/// is present but not true (Mastodon's signup terms wording; Pleroma's
/// schema requires only that the field is there).
const AGREEMENT_REQUIRED: &str = "You have to accept the terms to sign up";

/// The `/api/v1/accounts` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/accounts", post(create_account))
        .route("/accounts/verify_credentials", get(verify_credentials))
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

/// The request parameters of `POST /api/v1/accounts`, Pleroma's
/// `AccountCreateRequest` (`lib/pleroma/web/api_spec/operations/
/// account_operation.ex`) minus the captcha and invite fields:
/// `username`, `email`, `password` and `agreement` are read, the first
/// three required and `agreement` required too (an absent one is the
/// extractor's 422). `agreement` deserializes through [`flex::bool`] so
/// the form spelling `true` arrives. Every other key the body carries —
/// `locale`, `confirm`, the captcha pair, Pleroma's own `token` and
/// `birthday` — is accepted and ignored, as
/// [`Registration.register_user`] reads none of them on this path;
/// `reason`, `fullname` and `bio` are the optional registration facts
/// that do reach the row.
///
/// [`Registration.register_user`]: https://hexdocs.pm/pleroma/Pleroma.Web.Registration.html
#[derive(Debug, Deserialize)]
struct CreateAccountParams {
    /// The desired handle, validated by [`create_local_user`] as
    /// `RegisterParams::nickname`.
    username: String,
    /// Contact email; required unless
    /// [`accounts_email_optional`](crate::config::Config::accounts_email_optional).
    #[serde(default)]
    email: Option<String>,
    /// Plaintext password, stored only as a hash.
    password: String,
    /// Whether the applicant accepts the instance's terms; must be true.
    #[serde(deserialize_with = "flex::bool")]
    agreement: bool,
    /// Free-text reason for the later approval flow, stored in
    /// `users.registration_reason`.
    #[serde(default)]
    reason: Option<String>,
    /// Display name (Pleroma's `fullname`); the nickname stands in when
    /// absent or blank.
    #[serde(default)]
    fullname: Option<String>,
    /// Profile note as source text, stored as `raw_bio` and rendered for
    /// `bio`.
    #[serde(default)]
    bio: Option<String>,
}

impl CreateAccountParams {
    /// The registration [`RegisterParams`] these body parameters claim:
    /// `fullname` is Pleroma's display `name` fallback to the username,
    /// and `reason` rides along for storage.
    fn into_register_params(self) -> RegisterParams {
        RegisterParams {
            nickname: self.username,
            email: self.email,
            password: self.password,
            name: self.fullname,
            bio: self.bio,
            reason: self.reason,
        }
    }
}

/// The 200 body of a successful registration: the freshly minted user
/// token in the shape Pleroma's "token.json" view renders on this
/// endpoint (its `me` key belongs to `/oauth/token`; the registration
/// ticket's response does not carry it).
#[derive(Debug, Serialize)]
struct RegistrationToken {
    /// Token type, always `Bearer`.
    token_type: &'static str,
    /// The access token, to present as `Authorization: Bearer`.
    access_token: String,
    /// The granted scopes, space-separated — the presenting token's.
    scope: String,
    /// Issue time, in unix seconds (Pleroma's `format_created_at`).
    created_at: i64,
    /// The token row's `UUIDv7` id, hyphenated (decision 4).
    id: String,
    /// Seconds of validity left at render time.
    expires_in: i64,
    /// The refresh token, to exchange for a fresh pair.
    refresh_token: String,
}

impl RegistrationToken {
    /// Renders the freshly minted `token` (always user-bound here, so
    /// `refresh_token` is always in).
    fn new(token: OauthToken) -> Self {
        Self {
            token_type: TOKEN_TYPE_BEARER,
            access_token: token.token,
            scope: token.scopes,
            created_at: token.inserted_at.timestamp(),
            id: token.id.to_string(),
            expires_in: token
                .valid_until
                .signed_duration_since(Utc::now())
                .num_seconds(),
            refresh_token: token.refresh_token,
        }
    }
}

/// `POST /api/v1/accounts` — register a local account and receive a
/// bearer token for it (Pleroma's `.create`, which ends in
/// `OAuthController.login(user, app, …)` + the token view).
///
/// The gates run in Pleroma's plug order: [`AppAuth`] first rejects a
/// missing or invalid credential (401) — an app-only `client_credentials`
/// token satisfies it exactly as a user token does — then the
/// `write:accounts` scope check (403), then the `REGISTRATIONS_OPEN`
/// gate (403 [`REGISTRATIONS_CLOSED`]), then the agreement (422). The
/// account itself is built by [`create_local_user`], whose errors this
/// endpoint's ticket maps as implemented there: a 422 whose `error`
/// value is the JSON string of the field map. Success mints the new
/// user's token for the presenting token's app with the presenting
/// token's scopes, valid for `OAUTH_TOKEN_TTL_SECS`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AppAuth`] without a valid token;
/// [`AppError::Forbidden`] from [`require_scope`] without `write:accounts`
/// or while registrations are closed; [`AppError::Unprocessable`] when
/// `agreement` is absent (the extractor's) or not true, and
/// [`create_local_user`]'s field-map 422s for every invalid parameter.
/// Any database or key-generation failure arrives through `?` as
/// [`AppError::Database`] or [`AppError::Internal`].
async fn create_account(
    State(state): State<AppState>,
    AppAuth(app, token): AppAuth,
    ApiBody(params): ApiBody<CreateAccountParams>,
) -> AppResult<Json<RegistrationToken>> {
    require_scope(&token, WRITE_ACCOUNTS)?;
    if !state.config.registrations_open.get() {
        return Err(AppError::Forbidden(REGISTRATIONS_CLOSED.to_owned()));
    }
    if !params.agreement {
        return Err(AppError::Unprocessable(AGREEMENT_REQUIRED.to_owned()));
    }
    let user = create_local_user(&state, params.into_register_params()).await?;
    let issued = issue_registration_token(&state, &app, &token.scopes, user.id).await?;
    debug!(
        user = %user.id,
        app = %app.id,
        "registered a new local account"
    );
    Ok(Json(RegistrationToken::new(issued)))
}

/// Mints the new user's bearer token: the presenting token's scopes on
/// the issuing application, valid for the configured
/// `OAUTH_TOKEN_TTL_SECS` (the `/oauth/token` grant's
/// `issue_token`, with the scopes fixed by the presenting credential
/// rather than requested).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn issue_registration_token(
    state: &AppState,
    app: &OauthApp,
    scopes: &str,
    user_id: Uuid,
) -> AppResult<OauthToken> {
    // `OAUTH_TOKEN_TTL_SECS` is a `usize`; on any target where it could
    // exceed `u64::MAX` the absurd upper bound fails the insert instead
    // of silently shortening the lifetime.
    let ttl_secs = u64::try_from(state.config.oauth_token_ttl_secs).unwrap_or(u64::MAX);
    oauth_token::create_token(
        &state.db,
        &NewToken {
            user_id: Some(user_id),
            app_id: app.id,
            scopes: scopes.to_owned(),
            ttl: Some(Duration::from_secs(ttl_secs)),
        },
    )
    .await
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
    let owner = &auth.0;
    let requests = follow::pending_count(&state.db, owner.id).await?;
    let account = Account::for_owner(owner, &state.config, requests);
    Ok(Json(with_moved(&state, account, owner).await?))
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
    use serde_json::json;

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

    /// Parses `value` as the registration endpoint's parameters, as
    /// `ApiBody` would.
    fn registration_params(value: serde_json::Value) -> AppResult<CreateAccountParams> {
        crate::extract::into_params(value)
    }

    /// A stored-looking token row minted right now, for the
    /// [`RegistrationToken`] render.
    fn minted_token() -> OauthToken {
        let now = Utc::now();
        OauthToken {
            id: Uuid::now_v7(),
            token: "access-secret".to_owned(),
            refresh_token: "refresh-secret".to_owned(),
            user_id: Some(Uuid::now_v7()),
            app_id: Uuid::now_v7(),
            scopes: "read write follow push".to_owned(),
            valid_until: now + chrono::Duration::days(30),
            inserted_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn registration_params_take_their_required_fields_and_default_the_rest() {
        let params = registration_params(json!({
            "username": "lain",
            "email": "lain@example.org",
            "password": "PlzDontHackLain",
            "agreement": true,
        }))
        .expect("the required four are there");
        assert_eq!(params.username, "lain", "username");
        assert!(params.agreement, "a real json boolean");
        assert!(
            params.reason.is_none() && params.fullname.is_none() && params.bio.is_none(),
            "every optional field defaults away when absent"
        );
    }

    #[test]
    fn registration_agreement_accepts_the_form_spellings() {
        // The extractor turns `agreement=true` into this shape
        // (`src/extract.rs` covers the parsing itself).
        let params = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": "true",
            "reason": "I'm a cool dude, bro",
            "fullname": "Lain Iwakura",
            "bio": "☕️",
        }))
        .expect("a form-style body");
        assert!(params.agreement, "`\"true\"` is the form spelling");
        assert_eq!(params.reason.as_deref(), Some("I'm a cool dude, bro"));
    }

    #[test]
    fn ignored_fields_are_accepted_and_untyped() {
        // `locale` and `confirm` are on the wire but never read: the
        // endpoint takes them as sent, in any shape, and ignores them.
        let params = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": "on",
            "locale": "en",
            "confirm": "PlzDontHackLain",
            "captcha_response": 9001,
        }))
        .expect("unknown keys do not bind and do not reject");
        assert!(params.agreement, "`\"on\"` is the checkbox spelling");
    }

    #[test]
    fn a_missing_or_unboolean_agreement_is_unprocessable() {
        for value in [
            json!({"username": "lain", "password": "x"}),
            json!({"username": "lain", "password": "x", "agreement": "maybe"}),
        ] {
            let err = registration_params(value).expect_err("`agreement` is required");
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "a bad agreement is a 422, got {err}"
            );
        }
    }

    #[test]
    fn registration_params_map_onto_the_register_model() {
        let params = registration_params(json!({
            "username": "lain",
            "email": "lain@example.org",
            "password": "PlzDontHackLain",
            "agreement": true,
            "fullname": "Lain Iwakura",
            "bio": "wired",
            "reason": "let me in",
            "locale": "ja",
            "confirm": "PlzDontHackLain",
        }))
        .expect("a full body");
        let register = params.into_register_params();
        assert_eq!(register.nickname, "lain", "username is the nickname");
        assert_eq!(register.email.as_deref(), Some("lain@example.org"), "email");
        assert_eq!(register.password, "PlzDontHackLain", "password");
        assert_eq!(register.name.as_deref(), Some("Lain Iwakura"), "fullname");
        assert_eq!(register.bio.as_deref(), Some("wired"), "bio");
        assert_eq!(register.reason.as_deref(), Some("let me in"), "reason");
    }

    #[test]
    fn the_registration_answer_is_the_token_json() {
        let stored = minted_token();
        let expected_id = stored.id;
        let expected_created = stored.inserted_at.timestamp();
        let body = serde_json::to_value(RegistrationToken::new(stored)).unwrap();
        assert_eq!(body["token_type"], "Bearer", "bearer credentials");
        assert_eq!(body["access_token"], "access-secret", "the access token");
        assert_eq!(body["refresh_token"], "refresh-secret", "the refresh token");
        assert_eq!(
            body["scope"], "read write follow push",
            "the presenting token's scopes"
        );
        assert_eq!(body["created_at"], expected_created, "unix seconds");
        assert_eq!(
            body["id"],
            expected_id.to_string().as_str(),
            "hyphenated `UUIDv7`"
        );
        let ttl = i64::try_from(crate::config::DEFAULT_OAUTH_TOKEN_TTL_SECS).unwrap();
        assert!(
            (ttl - 60..=ttl + 1).contains(&body["expires_in"].as_i64().unwrap()),
            "expires_in counts down the month-long lifetime"
        );
        assert_eq!(
            body.as_object().expect("an object").len(),
            7,
            "exactly the ticket's seven keys, no `me`"
        );
    }
}
