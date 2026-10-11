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
//! All routes require a user token ([`AuthUser`]) and, per the ticket's
//! scope pair, `read:lists` to look and `write:lists` to change.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;

use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::list::List,
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::list,
    oauth::scopes::{READ_LISTS, WRITE_LISTS},
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

/// The `/api/v1/lists` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/lists", get(list_lists).post(create_list))
        .route(
            "/lists/{id}",
            get(show_list).put(update_list).delete(delete_list),
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
    Ok(Json(Value::Object(Map::new())))
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
                rejection(json!({"title": "x", "replies_policy": value.clone()})),
                POLICY_INVALID,
                "`{value}` is not a policy"
            );
        }
    }

    #[test]
    fn a_blank_or_absent_title_is_rejected_with_mastodon_wording() {
        for value in [json!(null), json!(""), json!("   ")] {
            assert_eq!(
                rejection(json!({"title": value.clone()})),
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
}
