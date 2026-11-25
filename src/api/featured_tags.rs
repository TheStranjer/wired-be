//! Featured hashtags over HTTP: the tags an account shows on its profile.
//!
//! The reference is Mastodon's `Api::V1::FeaturedTagsController`,
//! `Api::V1::Accounts::FeaturedTagsController` and
//! `Api::V1::Tags` `feature`/`unfeature` (`app/controllers/api/v1/…`); the
//! storage is [`crate::models::featured_tag`].
//!
//! - `GET /api/v1/featured_tags` (`read:accounts`) lists the caller's
//!   featured tags as `FeaturedTag` entities, by name.
//! - `POST /api/v1/featured_tags {name}` (`write:accounts`) features a tag,
//!   creating the tag row when it is new. A blank or unusable name, a tag
//!   already featured and an eleventh tag are all 422s in Mastodon's
//!   `Validation failed: …` wording.
//! - `DELETE /api/v1/featured_tags/{id}` (`write:accounts`) stops featuring
//!   the row and answers `{}`; a row that is not the caller's is the 404.
//! - `GET /api/v1/featured_tags/suggestions` (`read:accounts`) lists the
//!   tags the caller used recently and does not feature, as `Tag` entities.
//! - `GET /api/v1/accounts/{id}/featured_tags` lists any visible account's
//!   featured tags; authentication is optional, and a presented user token
//!   must grant `read:accounts`. Remote accounts answer what their last
//!   actor refresh stored.
//!
//! Every write that changes a local account's set queues a profile `Update`
//! so remote servers re-read the `featuredTags` collection.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    api::{helpers::visible_account_or_404, tags::render_tag},
    auth::{AuthUser, MaybeAuthUser},
    entities::{featured_tag::FeaturedTag, tag::Tag},
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        featured_tag::{self, Featured},
        hashtag,
        user::User,
    },
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    protocols::events::{AccountEvent, dispatch},
    state::AppState,
    util::id::parse_id,
};

/// 422 body for a missing or blank `name`.
const NAME_BLANK: &str = "Validation failed: Name can't be blank";
/// 422 body for a name that is no usable tag.
const NAME_INVALID: &str = "Validation failed: Name is invalid";
/// 422 body for a tag the caller already features.
const NAME_TAKEN: &str = "Validation failed: Name has already been taken";
/// 422 body for a tag past the [`featured_tag::MAX_PER_USER`] limit.
const LIMIT_REACHED: &str =
    "Validation failed: You have already featured the maximum number of hashtags";

/// The `/api/v1` featured-tags router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/featured_tags", get(own_featured_tags).post(create))
        .route("/featured_tags/suggestions", get(suggestions))
        .route("/featured_tags/{id}", delete(remove))
        .route("/accounts/{id}/featured_tags", get(account_featured_tags))
}

/// The body of `POST /api/v1/featured_tags`.
#[derive(Debug, Deserialize)]
struct CreateParams {
    /// The tag to feature, with or without its `#`.
    #[serde(default)]
    name: Option<String>,
}

/// `GET /api/v1/featured_tags` — the caller's featured tags.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:accounts` check. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn own_featured_tags(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<FeaturedTag>>> {
    auth.require(READ_ACCOUNTS)?;
    Ok(Json(rendered_set(&state, &auth.0).await?))
}

/// `GET /api/v1/accounts/{id}/featured_tags` — the account's featured
/// tags.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`MaybeAuthUser`] for a presented token
/// that does not resolve; [`AppError::Forbidden`] for a user token without
/// `read:accounts`; [`AppError::NotFound`] for a malformed, unknown or
/// invisible account id. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn account_featured_tags(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<FeaturedTag>>> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let account = visible_account_or_404(&state, &id, auth.0.as_ref()).await?;
    Ok(Json(rendered_set(&state, &account).await?))
}

/// `POST /api/v1/featured_tags` — feature a tag and answer its entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:accounts` check; [`AppError::Unprocessable`] for a blank or
/// unusable name, a tag already featured, or the limit. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<CreateParams>,
) -> AppResult<Json<FeaturedTag>> {
    auth.require(WRITE_ACCOUNTS)?;
    let written = params.name.unwrap_or_default();
    if written.trim().trim_start_matches('#').trim().is_empty() {
        return Err(AppError::Unprocessable(NAME_BLANK.to_owned()));
    }
    let tag = hashtag::find_or_create(&state.db, &written)
        .await?
        .ok_or_else(|| AppError::Unprocessable(NAME_INVALID.to_owned()))?;
    match featured_tag::feature(&state.db, auth.0.id, &tag).await? {
        Featured::Added(entry) => {
            debug!(user = %auth.0.id, tag = %tag.name, "tag featured");
            dispatch(&state, &auth.0, AccountEvent::ProfileChanged).await;
            Ok(Json(FeaturedTag::new(
                &state.config.base_url,
                &auth.0.nickname,
                &entry,
            )))
        }
        Featured::Existing(_) => Err(AppError::Unprocessable(NAME_TAKEN.to_owned())),
        Featured::LimitReached => Err(AppError::Unprocessable(LIMIT_REACHED.to_owned())),
    }
}

/// `DELETE /api/v1/featured_tags/{id}` — stop featuring one tag; `{}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:accounts` check; [`AppError::NotFound`] for a malformed id or
/// a row the caller does not own. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn remove(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let row = parse_id(&id).ok_or(AppError::NotFound)?;
    if !featured_tag::unfeature(&state.db, auth.0.id, row).await? {
        return Err(AppError::NotFound);
    }
    debug!(user = %auth.0.id, featured = %row, "tag unfeatured");
    dispatch(&state, &auth.0, AccountEvent::ProfileChanged).await;
    Ok(Json(json!({})))
}

/// `GET /api/v1/featured_tags/suggestions` — the caller's recently used
/// tags that are not featured yet, as `Tag` entities with their history.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:accounts` check. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn suggestions(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<Vec<Tag>>> {
    auth.require(READ_ACCOUNTS)?;
    let tags = featured_tag::suggestions(&state.db, auth.0.id).await?;
    let mut rendered = Vec::with_capacity(tags.len());
    for tag in &tags {
        rendered.push(render_tag(&state, tag, None, None).await?);
    }
    Ok(Json(rendered))
}

/// The entities of every tag `account` features.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn rendered_set(state: &AppState, account: &User) -> AppResult<Vec<FeaturedTag>> {
    let entries = featured_tag::list(&state.db, account.id).await?;
    Ok(entries
        .iter()
        .map(|entry| FeaturedTag::new(&state.config.base_url, &account.nickname, entry))
        .collect())
}
