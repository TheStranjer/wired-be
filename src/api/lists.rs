//! Lists over HTTP: one account's named collections of accounts, the
//! Mastodon v1 CRUD plus the "which of my lists hold this account" lookup
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `ListController`
//! (`lib/pleroma/web/mastodon_api/controllers/list_controller.ex`) and
//! `Pleroma.List` (`lib/pleroma/list.ex`). Pleroma's `following` address
//! array, its per-list `ap_id` and its `pleroma.emoji` entity block all
//! stay behind the model ([`crate::models::list`]) or belong to the
//! membership ticket; this module is the six routes and the parameter
//! rules in front of them.
//!
//! - `GET /api/v1/lists` answers the caller's lists as an array in
//!   creation order (id ascending, decision 4; [`list::for_user`]).
//! - `GET /api/v1/lists/{id}` answers one list; [`crate::api::helpers`]'s
//!   one 404 for foreign, unknown and malformed ids.
//! - `POST /api/v1/lists` creates, `PUT /api/v1/lists/{id}` changes only
//!   the parameters it names, and `DELETE /api/v1/lists/{id}` removes a
//!   list and answers `{}`.
//! - `GET /api/v1/accounts/{id}/lists` answers the *caller's* lists that
//!   hold that account ([`list::lists_containing`]), a 404 when the
//!   account is unknown.
//!
//! Both writes take the same parameters (decision 10: through [`ApiBody`],
//! so form spellings work): `title` (required on create, 1 to 100
//! characters; absent or blank is the 422 `{"error":"Validation failed:
//! Title can't be blank"}`, too long Mastodon's limit wording),
//! `replies_policy` (`none`, `list` or `followed`, default `list`; any
//! other value the 422 `{"error":"Validation failed: Replies policy is not
//! included in the list"}`) and `exclusive` (a [`flex`] boolean, default
//! `false`). On `PUT` every parameter is present-only: an absent one keeps
//! the stored column; a present one runs the same validation. As
//! Pleroma's `CastAndValidate` plug precedent, validation runs ahead of
//! any lookup.
//!
//! The members of one list are the three routes on
//! `/api/v1/lists/{id}/accounts`, Pleroma's `list_accounts`, `add_to_list`
//! and `remove_from_list`:
//!
//! - `GET` answers the members as [`Account`] entities, newest membership
//!   first, [`OrdinalPageParams`] keyset pagination over the membership
//!   ordinal (`limit` default 20, `limit=0` every member) and a `Link` header
//!   whose cursors are [`list::MemberEntry::position`] values — the order
//!   accounts were added in, the one order a list's members have.
//! - `POST` takes `account_ids[]` (decision 10: [`ApiBody`], so the JSON and
//!   the form spelling both work) and adds those accounts, answering `{}`.
//!   Mastodon's and Pleroma's rule applies: an account the caller does not
//!   follow (no *accepted* `follows` row) may not be added — the 422
//!   `{"error":"Validation failed: Account is not followed"}` — with the
//!   caller's own account always listable (Mastodon's silent self-follow,
//!   which lets an owner put themselves on their own list), and an id that
//!   names no account is the 404, checked for the whole batch before any
//!   of it is written. Adding a member twice writes nothing the second time.
//! - `DELETE` takes `account_ids[]` in the body *or* the query
//!   ([`ApiParams`], the spellings Pleroma accepts for this action) and
//!   removes those members, answering `{}`; an id that is not a member of
//!   that list — account or not — is ignored.
//! - Neither write federates: a list is its owner's private bookmark (as in
//!   Mastodon and Pleroma), so the listed account, local or remote, is never
//!   told and no federation job is queued.
//!
//! Every route of both groups requires a user token ([`AuthUser`]) and, per
//! the ticket's scope pair, `read:lists` to look and `write:lists` to change.
//! The `{id}` of a list that is malformed, unknown or someone else's is one
//! and the same 404 everywhere, including on the three member routes.

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::{account_or_404, distinct_ids},
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        list::List,
    },
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams, flex},
    models::{
        follow::{self, FollowState},
        list, user,
    },
    oauth::scopes::{READ_LISTS, WRITE_LISTS},
    pagination::{MAX_LIMIT, OrdinalPageParams, link_for_page, set_link_header},
    state::AppState,
    util::id::parse_id,
};

/// The longest title the endpoint accepts (the ticket's 1–100).
const MAX_TITLE_CHARS: usize = 100;
/// The replies policies the endpoint accepts (Mastodon's `List`).
const VALID_REPLIES_POLICIES: [&str; 3] = ["none", "list", "followed"];
/// The policy a create without one gets (Mastodon's column default).
const DEFAULT_REPLIES_POLICY: &str = "list";
/// 422 body for an absent or blank title (Mastodon's changeset wording).
const TITLE_BLANK: &str = "Validation failed: Title can't be blank";
/// 422 body for a policy outside [`VALID_REPLIES_POLICIES`] (likewise).
const POLICY_INVALID: &str = "Validation failed: Replies policy is not included in the list";
/// 422 body for an account the caller does not follow (Mastodon's changeset
/// wording for `List`'s `account_ids` validation; the ticket's exact text).
const NOT_FOLLOWED: &str = "Validation failed: Account is not followed";

/// The `/api/v1/lists` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/lists", get(list_lists).post(create_list))
        .route(
            "/lists/{id}",
            get(show_list).put(update_list).delete(delete_list),
        )
        .route(
            "/lists/{id}/accounts",
            get(list_list_accounts)
                .post(add_list_accounts)
                .delete(remove_list_accounts),
        )
        .route("/accounts/{id}/lists", get(lists_for_account))
}

/// `GET /api/v1/lists` — the caller's lists, oldest first (creation
/// order).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:lists` check. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn list_lists(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<Vec<List>>> {
    auth.require(READ_LISTS)?;
    let rows = list::for_user(&state.db, auth.0.id).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "lists listed"
    );
    Ok(Json(rows.iter().map(List::from_row).collect()))
}

/// `GET /api/v1/lists/{id}` — one of the caller's lists.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:lists` check; [`AppError::NotFound`] for an id that is
/// malformed, names no list or names one of another account. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn show_list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<List>> {
    auth.require(READ_LISTS)?;
    let Some(list_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = list::get_owned(&state.db, auth.0.id, list_id).await? else {
        return Err(AppError::NotFound);
    };
    Ok(Json(List::from_row(&row)))
}

/// `POST /api/v1/lists` — create a list for the caller and answer it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:lists` check; [`AppError::Unprocessable`] when `title` or
/// `replies_policy` fails validation (see the [module docs]). Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn create_list(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ListParams>,
) -> AppResult<Json<List>> {
    auth.require(WRITE_LISTS)?;
    let new = params.into_new_list()?;
    let row = list::create(&state.db, auth.0.id, &new).await?;
    debug!(
        list = %row.id,
        user = %auth.0.id,
        "list created"
    );
    Ok(Json(List::from_row(&row)))
}

/// `PUT /api/v1/lists/{id}` — change the named columns of one owned list
/// and answer it. Parameters are those of `POST`, present-only: an absent
/// one keeps the stored value.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:lists` check; [`AppError::Unprocessable`] for invalid
/// parameters — ahead of the lookup, as Pleroma's validation plug does;
/// [`AppError::NotFound`] when the id is malformed, unknown or foreign. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn update_list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<ListParams>,
) -> AppResult<Json<List>> {
    auth.require(WRITE_LISTS)?;
    // Validation first, lookup second: Pleroma's `CastAndValidate` plug
    // rejects a bad body before the action ever reaches `List.get`.
    let changes = params.into_changes()?;
    let Some(list_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = list::update(&state.db, auth.0.id, list_id, &changes).await? else {
        return Err(AppError::NotFound);
    };
    Ok(Json(List::from_row(&row)))
}

/// `DELETE /api/v1/lists/{id}` — remove one owned list and answer `{}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:lists` check; [`AppError::NotFound`] when the id is
/// malformed, unknown or foreign. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn delete_list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_LISTS)?;
    let Some(list_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    if !list::delete(&state.db, auth.0.id, list_id).await? {
        return Err(AppError::NotFound);
    }
    Ok(empty_object())
}

/// `GET /api/v1/accounts/{id}/lists` — the caller's lists that hold that
/// account, oldest first. Pleroma's `AccountController.lists`
/// (`Pleroma.List.get_lists_account_belongs(owner, user)`): only the
/// viewer's own lists ever appear.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:lists` check; [`AppError::NotFound`] when the id is malformed,
/// names no user or names a deactivated one (see [`account_or_404`]). Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn lists_for_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<List>>> {
    auth.require(READ_LISTS)?;
    let found = account_or_404(&state, &id).await?;
    let rows = list::lists_containing(&state.db, auth.0.id, found.id).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        member = %found.id,
        "account memberships listed"
    );
    Ok(Json(rows.iter().map(List::from_row).collect()))
}

/// `GET /api/v1/lists/{id}/accounts` — the members of one of the caller's
/// lists as [`Account`] entities, newest membership first, one
/// [`OrdinalPageParams`] page at a time (default 20, at most
/// [`MAX_LIMIT`], `limit=0` for every member) under a `Link` header keyed on
/// [`list::MemberEntry::position`] — the order accounts were added in.
/// Pleroma's `list_accounts` answers its whole `following` array at once;
/// Mastodon's page size and cursor spelling is what the ticket pins here.
///
/// The members are rendered as the caller sees them ([`AccountViewer`]);
/// their relationship to the caller is not embedded — Pleroma's
/// `AccountView` index render does not carry one for this action either.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:lists` check; [`AppError::NotFound`] when the id is malformed,
/// names no list or names one of another account (see [`owned_list`]);
/// [`AppError::BadRequest`] from [`OrdinalPageParams`] for a cursor that is
/// not a number. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value that
/// is not header-safe as [`AppError::Internal`].
async fn list_list_accounts(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require(READ_LISTS)?;
    let list_id = owned_list(&state, auth.0.id, &id).await?;
    let page = OrdinalPageParams::from_query(uri.query(), MAX_LIMIT)?;
    let entries = list::members_page(&state.db, list_id, page).await?;
    debug!(
        count = entries.len(),
        list = %list_id,
        user = %auth.0.id,
        "list members listed"
    );
    // The cursors key on the membership ordinal
    // ([`list::MemberEntry::position`]), not on the members' ids.
    let link = link_for_page(&state.config.base_url, uri.path(), &entries, |e| e.position);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let viewer: AccountViewer<'_> = Some(&auth.0);
    let accounts = entries
        .iter()
        .map(|entry| Account::from_user_for(viewer, &entry.user, &state.config, None))
        .collect();
    Ok((headers, Json(accounts)))
}

/// `POST /api/v1/lists/{id}/accounts` — add `account_ids[]` to one of the
/// caller's lists and answer `{}`.
///
/// The whole batch is settled before any of it is written: every id has to
/// name an account (else the 404) and every one of them has to be an account
/// the caller follows with an *accepted* relationship (else the 422
/// [`NOT_FOLLOWED`]) — Mastodon's `List` validation and the rule Pleroma's
/// `add_to_list` enforces by only ever looking at followed accounts, with the
/// caller themselves excepted as their own silent follow. A member
/// who is already on the list is left exactly where they are
/// ([`list::add_members`]), so adding twice is one membership; an empty
/// `account_ids` adds nobody and still answers `{}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:lists` check; [`AppError::Unprocessable`] when the body has no
/// `account_ids` array (the extractor's 422) or names an account the caller
/// does not follow; [`AppError::NotFound`] for a list that is malformed,
/// unknown or foreign, and for an account id that is malformed or names no
/// account. Any database failure arrives through `?` as [`AppError::Database`].
async fn add_list_accounts(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(body): ApiBody<MemberParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_LISTS)?;
    let list_id = owned_list(&state, auth.0.id, &id).await?;
    let members = requested_accounts(&state, &body.account_ids).await?;
    ensure_followed(&state, auth.0.id, &members).await?;
    let added = list::add_members(&state.db, list_id, &members).await?;
    debug!(
        added,
        list = %list_id,
        user = %auth.0.id,
        "list members added"
    );
    Ok(empty_object())
}

/// `DELETE /api/v1/lists/{id}/accounts` — remove `account_ids[]` from one of
/// the caller's lists and answer `{}`.
///
/// Pleroma's `remove_from_list` takes the ids from the query *or* the body,
/// which is what [`ApiParams`] is for; the removal is the plain delete
/// ([`list::remove_members`]), so an id that is not a member of that list —
/// an unknown account, a malformed id, an account on another owner's list —
/// matches nothing and is ignored rather than refused.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:lists` check; [`AppError::Unprocessable`] when neither the body
/// nor the query carries an `account_ids` array; [`AppError::NotFound`] for a
/// list that is malformed, unknown or foreign. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn remove_list_accounts(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<MemberParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_LISTS)?;
    let list_id = owned_list(&state, auth.0.id, &id).await?;
    let (members, _unknown_ignored) = distinct_ids(params.account_ids.iter().map(String::as_str));
    let removed = list::remove_members(&state.db, list_id, &members).await?;
    debug!(
        removed,
        list = %list_id,
        user = %auth.0.id,
        "list members removed"
    );
    Ok(empty_object())
}

/// The `{id}` path segment of a list route, resolved to the list `owner`
/// owns: [`AppError::NotFound`] for an id that is malformed, names no list or
/// names one of another account — Pleroma's `list_by_id_and_user` plug, which
/// answers all three the same way before any action runs. The list timeline
/// resolves its `{list_id}` the same way.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives through
/// `?` as [`AppError::Database`].
pub(crate) async fn owned_list(state: &AppState, owner: Uuid, id: &str) -> AppResult<Uuid> {
    let Some(list_id) = parse_id(id) else {
        return Err(AppError::NotFound);
    };
    if list::get_owned(&state.db, owner, list_id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(list_id)
}

/// The accounts an `add` names: every id of `account_ids[]` must be parseable
/// and must name an account this instance knows, or the whole batch is the
/// 404 — nothing is added for a request that names somebody who does not
/// exist.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives through
/// `?` as [`AppError::Database`].
async fn requested_accounts(state: &AppState, texts: &[String]) -> AppResult<Vec<Uuid>> {
    let (ids, all_ids) = distinct_ids(texts.iter().map(String::as_str));
    if !all_ids {
        return Err(AppError::NotFound);
    }
    let found = user::get_many(&state.db, &ids).await?;
    if found.len() != ids.len() {
        return Err(AppError::NotFound);
    }
    Ok(ids)
}

/// The rule [`add_list_accounts`] validates the batch against: every account
/// named must be one `follower` follows with an *accepted* relationship — a
/// pending request is not a follow, the same fact [`follow::is_following`]
/// states for a pair of rows, read here off [`follow::get`] because the
/// endpoint has ids, not rows. The one exception is `follower` naming
/// themselves: Mastodon gives every account a silent self-follow, so an owner
/// is always member-worth of their own list, and that pair never appears in
/// `follows` here.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`NOT_FOLLOWED`] for the first account
/// that fails the rule; any database failure arrives through `?` as
/// [`AppError::Database`].
async fn ensure_followed(state: &AppState, follower: Uuid, targets: &[Uuid]) -> AppResult<()> {
    for target in targets {
        if *target == follower {
            continue;
        }
        let granted = follow::get(&state.db, follower, *target).await?;
        if !granted.is_some_and(|row| row.state == FollowState::Accepted) {
            return Err(AppError::Unprocessable(NOT_FOLLOWED.to_owned()));
        }
    }
    Ok(())
}

/// The body both membership writes answer: `{}`, Pleroma's
/// `json(conn, %{})` for `add_to_list` and `remove_from_list`.
fn empty_object() -> Json<Value> {
    Json(Value::Object(Map::new()))
}

/// The request parameters of `POST` and `PUT /api/v1/lists`.
///
/// Every field is `Option` at the serde layer: `POST` demands the title and
/// defaults the rest ([`into_new_list`](ListParams::into_new_list)), `PUT`
/// changes only what is present ([`into_changes`](ListParams::into_changes)).
#[derive(Debug, Deserialize)]
struct ListParams {
    /// The title; required on create, 1 to [`MAX_TITLE_CHARS`] characters.
    #[serde(default)]
    title: Option<String>,
    /// The replies policy; one of [`VALID_REPLIES_POLICIES`], `list` when
    /// absent.
    #[serde(default)]
    replies_policy: Option<String>,
    /// Whether members leave the Home feed; `false` when absent, like the
    /// column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    exclusive: Option<bool>,
}

impl ListParams {
    /// Validates the parameters as a whole new list, applying the defaults
    /// for the optional policy and flag.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with Mastodon's changeset wording for a
    /// blank or too-long title or an unknown replies policy.
    fn into_new_list(self) -> AppResult<list::NewList> {
        Ok(list::NewList {
            title: self.title.map_or_else(default_title, validate_title)?,
            replies_policy: match self.replies_policy {
                Some(policy) => validate_policy(policy)?,
                None => DEFAULT_REPLIES_POLICY.to_owned(),
            },
            exclusive: self.exclusive.unwrap_or_default(),
        })
    }

    /// Validates the parameters as a present-only change: each field that
    /// arrived is checked, each that did not stays `None` and keeps its
    /// stored column.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] as [`into_new_list`](ListParams::into_new_list),
    /// for the fields that are present.
    fn into_changes(self) -> AppResult<list::ListChanges> {
        Ok(list::ListChanges {
            title: self.title.map(validate_title).transpose()?,
            replies_policy: self.replies_policy.map(validate_policy).transpose()?,
            exclusive: self.exclusive,
        })
    }
}

/// The request parameters of the two membership writes: Mastodon's
/// `account_ids[]`.
///
/// The field is required — a `POST`/`DELETE` without it anywhere is the
/// extractor's 422 — and every id arrives as text, which is both decision 10's
/// form spelling (`account_ids[]=1&account_ids[]=2`) and what a JSON body of
/// id strings gives.
#[derive(Debug, Deserialize)]
struct MemberParams {
    /// The accounts to add to, or remove from, the list.
    account_ids: Vec<String>,
}

/// The title a create without one validates — and fails on.
fn default_title() -> AppResult<String> {
    validate_title(String::new())
}

/// The title, checked: blank (absent counts as blank on create) is 422
/// [`TITLE_BLANK`], longer than [`MAX_TITLE_CHARS`] characters is 422 with
/// Mastodon's limit wording; a kept title is stored verbatim.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validate_title(title: String) -> AppResult<String> {
    if title.trim().is_empty() {
        return Err(AppError::Unprocessable(TITLE_BLANK.to_owned()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Title is too long (maximum is {MAX_TITLE_CHARS} characters)"
        )));
    }
    Ok(title)
}

/// The replies policy, checked: any name outside
/// [`VALID_REPLIES_POLICIES`] is 422 [`POLICY_INVALID`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validate_policy(policy: String) -> AppResult<String> {
    if !VALID_REPLIES_POLICIES.contains(&policy.as_str()) {
        return Err(AppError::Unprocessable(POLICY_INVALID.to_owned()));
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Parses `value` as the endpoint's parameters and validates them as a
    /// create would.
    fn new_list(value: Value) -> AppResult<list::NewList> {
        let params: ListParams = crate::extract::into_params(value)?;
        params.into_new_list()
    }

    /// Parses `value` as the endpoint's parameters and validates them as a
    /// change would.
    fn changes(value: Value) -> AppResult<list::ListChanges> {
        let params: ListParams = crate::extract::into_params(value)?;
        params.into_changes()
    }

    /// The 422 message a create of `value` is rejected with.
    fn rejection(value: Value) -> String {
        new_list(value)
            .expect_err("the fixture is invalid")
            .to_string()
    }

    /// The title `value` is stored as (after the create path's checks).
    fn created_title(value: Value) -> String {
        new_list(value).expect("the fixture validates").title
    }

    #[test]
    fn a_title_only_body_applies_every_default() {
        let new = new_list(json!({"title": "Friends"})).expect("the minimal body validates");
        assert_eq!(new.title, "Friends", "the title is kept verbatim");
        assert_eq!(
            new.replies_policy, "list",
            "the policy defaults to `list`, like the column"
        );
        assert!(!new.exclusive, "and exclusiveness defaults to false");
    }

    #[test]
    fn every_known_policy_passes() {
        for policy in VALID_REPLIES_POLICIES {
            let new = new_list(json!({"title": "x", "replies_policy": policy}))
                .unwrap_or_else(|err| panic!("`{policy}` is valid: {err}"));
            assert_eq!(new.replies_policy, policy, "`{policy}` goes through");
        }
    }

    #[test]
    fn an_unknown_policy_is_rejected_with_mastodon_wording() {
        for value in [json!(""), json!("everyone"), json!("List"), json!(" list")] {
            assert_eq!(
                rejection(json!({"title": "x", "replies_policy": value})),
                POLICY_INVALID,
                "`{value}` is not a policy"
            );
        }
    }

    #[test]
    fn a_blank_or_absent_title_is_rejected_with_mastodon_wording() {
        for value in [json!(null), json!(""), json!("   ")] {
            assert_eq!(
                rejection(json!({"title": value})),
                TITLE_BLANK,
                "`{value}` is a blank title"
            );
        }
        assert_eq!(
            rejection(json!({})),
            TITLE_BLANK,
            "absent is as blank as empty"
        );
    }

    #[test]
    fn a_title_of_exactly_the_maximum_length_passes() {
        let edge = "x".repeat(MAX_TITLE_CHARS);
        assert_eq!(
            created_title(json!({"title": edge.clone()})),
            edge,
            "100 characters fit"
        );

        let over = "x".repeat(MAX_TITLE_CHARS + 1);
        assert_eq!(
            rejection(json!({"title": over})),
            "Validation failed: Title is too long (maximum is 100 characters)",
            "101 do not"
        );
    }

    #[test]
    fn a_non_ascii_title_is_counted_in_characters() {
        let dots = "…".repeat(MAX_TITLE_CHARS);
        assert_eq!(
            created_title(json!({"title": dots.clone()})),
            dots,
            "100 three-byte characters fit, verbatim"
        );
    }

    #[test]
    fn the_flag_is_read_in_its_client_spellings() {
        let new = new_list(json!({"title": "x", "exclusive": "true"}))
            .expect("the string spelling deserializes");
        assert!(new.exclusive, "`\"true\"` means true");
        let off =
            new_list(json!({"title": "x", "exclusive": "0"})).expect("and so does the numeric one");
        assert!(!off.exclusive, "`\"0\"` means false");
    }

    #[test]
    fn a_change_names_only_what_arrived() {
        let empty = changes(json!({})).expect("an empty change is valid");
        assert!(empty.title.is_none(), "absent title keeps the stored one");
        assert!(
            empty.replies_policy.is_none(),
            "absent policy keeps the stored one"
        );
        assert!(empty.exclusive.is_none(), "and so does the flag");

        let titled = changes(json!({"title": "new"})).expect("a lone title is valid");
        assert_eq!(
            titled.title.as_deref(),
            Some("new"),
            "the title came through"
        );
        assert!(titled.replies_policy.is_none(), "nothing else did");
    }

    #[test]
    fn a_change_validates_what_it_names() {
        assert_eq!(
            changes(json!({"title": ""}))
                .expect_err("an empty title is blank")
                .to_string(),
            TITLE_BLANK,
            "with Mastodon's wording"
        );
        assert_eq!(
            changes(json!({"replies_policy": "bogus"}))
                .expect_err("bogus is no policy")
                .to_string(),
            POLICY_INVALID,
            "and its own for the policy"
        );
    }

    #[test]
    fn a_flag_that_is_not_a_boolean_is_a_parameter_error() {
        let err = crate::extract::into_params::<ListParams>(json!({
            "title": "x",
            "exclusive": "maybe",
        }))
        .expect_err("maybe is not a boolean");
        assert!(
            err.to_string().contains("expected a boolean"),
            "and the flex message says so: {err}"
        );
    }

    /// The account ids the `account_ids` of `params` name, asserted to be
    /// ids all the way through.
    fn requested(texts: &[String]) -> Vec<Uuid> {
        let (ids, all_ids) = distinct_ids(texts.iter().map(String::as_str));
        assert!(all_ids, "the fixture names only ids");
        ids
    }

    #[test]
    fn membership_ids_arrive_in_either_body_spelling() {
        let first = Uuid::now_v7().to_string();
        let second = Uuid::now_v7().to_string();
        let from_json: MemberParams =
            crate::extract::into_params(json!({"account_ids": [first, second]}))
                .expect("a JSON array of id strings");
        let form = crate::extract::parse_pairs([
            ("account_ids[]", first.as_str()),
            ("account_ids[]", second.as_str()),
        ])
        .expect("the keys nest shallowly");
        let from_form: MemberParams =
            crate::extract::into_params(form).expect("the repeated-bracket form spelling");

        assert_eq!(
            from_form.account_ids, from_json.account_ids,
            "decision 10's two spellings reach the same parameter"
        );
        assert_eq!(
            requested(&from_json.account_ids),
            vec![parse_id(&first).unwrap(), parse_id(&second).unwrap()],
            "and the ids come out in the order the client sent them"
        );
    }

    #[test]
    fn a_membership_body_without_account_ids_is_unprocessable() {
        for value in [json!({}), json!({"ids": []})] {
            let err = crate::extract::into_params::<MemberParams>(value.clone())
                .expect_err("`account_ids` is required");
            assert!(
                err.to_string().contains("missing field `account_ids`"),
                "{value} is refused by name: {err}"
            );
        }
        let not_an_array = crate::extract::into_params::<MemberParams>(json!({"account_ids": "1"}))
            .expect_err("one id is not the parameter");
        assert!(
            not_an_array.to_string().contains("expected a sequence"),
            "and the shape it wanted is stated: {not_an_array}"
        );
    }

    #[test]
    fn repeated_membership_ids_are_one_request_and_garbage_is_reported() {
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let texts: Vec<String> = vec![
            first.to_string(),
            first.to_string(),
            "not-a-uuid".to_owned(),
            second.to_string(),
        ];
        let (ids, all_ids) = distinct_ids(texts.iter().map(String::as_str));
        assert!(!all_ids, "one of the four entries is no id at all");
        assert_eq!(
            ids,
            vec![first, second],
            "the lenient read folds the repeat, keeps the order and drops the \
             garbage"
        );
    }

    #[test]
    fn an_empty_membership_parameter_names_nobody() {
        assert_eq!(
            distinct_ids([]),
            (Vec::new(), true),
            "no ids is not the same as a bad id: an empty batch is complete"
        );
    }
}
