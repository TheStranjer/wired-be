//! The account endpoints: the single-account reads and listings, the
//! relationship batch, the notes and follow writes, and the profile write.
//!
//! The routes are split by the rule in [`crate::api`]. Under `/api/v1`:
//!
//! | Route | Class |
//! |---|---|
//! | `POST /accounts` | core |
//! | `GET /accounts/verify_credentials` | core |
//! | `PATCH`/`POST /accounts/update_credentials` | core |
//! | `GET /accounts` | `ActivityPub`-backed |
//! | `GET /accounts/{id}` | `ActivityPub`-backed |
//! | `GET /accounts/lookup` | `ActivityPub`-backed |
//! | `GET /accounts/relationships` | `ActivityPub`-backed |
//! | `GET /accounts/{id}/followers` | `ActivityPub`-backed |
//! | `GET /accounts/{id}/following` | `ActivityPub`-backed |
//! | `GET /accounts/{id}/statuses` | `ActivityPub`-backed |
//! | `POST /accounts/{id}/note` | `ActivityPub`-backed |
//! | `POST /accounts/{id}/remove_from_followers` | `ActivityPub`-backed |
//! | `POST /accounts/{id}/follow` | `ActivityPub`-backed |
//! | `POST /accounts/{id}/unfollow` | `ActivityPub`-backed |
//! | `POST /follows` | `ActivityPub`-backed |
//!
//! [`core_router`] holds the core routes and [`activitypub_router`] the rest.
//! `GET /api/account/confirm_email/{user_id}/{token}`
//! ([`email_confirmation_router`]) is core too.
//!
//! Pleroma's `AccountController.verify_credentials`, `.show`, `.lookup`,
//! `.relationships`, `.followers`, `.following` and `.statuses`
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
//! This module is a tree. `register` and `registration` hold the
//! account-creation flow and its answer shapes, `credentials` with
//! `credentials_merge` and `credentials_pleroma` the profile read and
//! write and its merge helpers, `profile_images` with
//! `profile_image_store` and `profile_text` the image writes and the
//! text renders, `lookup` the single-account reads and the
//! relationship batch, `notes_followers` and `follow` the relationship
//! writes, `follow_lists` the relationship listings and their page
//! render, and `account_statuses` with `pinned_statuses` the status
//! listing. This root keeps the routers, the shared constants and the
//! single-account render `render_account`; `test_support` holds the
//! unit-test fixtures the children share.
//!

use axum::{
    Router,
    routing::{get, patch, post},
};

use crate::{
    entities::{
        account::{Account, AccountViewer},
        relationship::{Relationship, RelationshipMap},
    },
    error::AppResult,
    extract::multipart::upload_body_limit,
    models::user::{DEFAULT_ACTOR_TYPE, User},
    services::account_emojis::AccountEmojis,
    state::AppState,
};

use self::{
    account_statuses::list_statuses,
    credentials::{update_credentials, verify_credentials, with_moved},
    follow::{follow_account, follow_by_uri, unfollow_account},
    follow_lists::{list_followers, list_following},
    lookup::{list_accounts, lookup_account, relationships, show_account},
    notes_followers::{remove_from_followers, set_note},
    register::{confirm_email, create_account},
};

pub(crate) use self::{
    follow::after_follow_changed,
    profile_text::{render_note, sanitized_name, stored_fields},
};

mod account_statuses;
mod credentials;
mod credentials_merge;
mod credentials_pleroma;
pub mod follow;
mod follow_lists;
mod lookup;
mod notes_followers;
mod pinned_statuses;
mod profile_image_store;
mod profile_images;
pub mod profile_text;
mod register;
mod registration;
#[cfg(test)]
mod test_support;

/// The four `default_scope` values `source[privacy]` accepts: the
/// visibilities new statuses may default to (the same list a scrobble's
/// `visibility` picks from).
pub(crate) const SCOPES: [&str; 4] = ["public", "unlisted", "private", "direct"];

/// The two `actor_type` values `update_credentials` accepts: a human actor
/// and an automated one (`Service` is what Pleroma stores for `bot=true`).
pub(crate) const ACTOR_TYPES: [&str; 2] = [PERSON_ACTOR_TYPE, SERVICE_ACTOR_TYPE];

/// The [`ACTOR_TYPES`] entry of a human actor: the column default, which is
/// what `bot=false` stores.
const PERSON_ACTOR_TYPE: &str = DEFAULT_ACTOR_TYPE;

/// The [`ACTOR_TYPES`] entry of an automated account.
pub(crate) const SERVICE_ACTOR_TYPE: &str = "Service";

/// The 200 body message `POST /api/v1/accounts` answers when the account
/// landed awaiting staff approval (`REGISTRATIONS_APPROVAL_REQUIRED`),
/// beside the `awaiting_approval` identifier (Pleroma's `create` action,
/// `{:login, {:account_status, :approval_pending}}`). The external
/// registration page shows the same words as its HTML message.
pub(crate) const REGISTERED_AWAITING_APPROVAL: &str =
    "You have been registered. You'll be able to log in once your account is approved.";

/// The 200 body message `POST /api/v1/accounts` answers when the account
/// landed unconfirmed (`ACCOUNT_ACTIVATION_REQUIRED`) and no approval waits
/// ahead of it, beside the `missing_confirmed_email` identifier (Pleroma's
/// `create` action, `{:login, {:account_status, :confirmation_pending}}`).
/// The external registration page shows the same words as its HTML message.
pub(crate) const REGISTERED_AWAITING_CONFIRMATION: &str =
    "You have been registered. Please check your email for further instructions.";

/// The core `/api/v1/accounts` routes, merged into [`crate::api::v1::core_router`].
///
/// They work for the signed-in local account with no `ActivityPub` data:
/// registration, `verify_credentials` and `update_credentials`.
///
/// [`state.config`](AppState::config) is what [`upload_body_limit`] sizes
/// the routes' body limit from: `update_credentials` takes the three
/// profile images as multipart file parts, and axum's multipart extractor
/// refuses a body over its 2 MiB default unless the route raises that —
/// which for an avatar capped at [`DEFAULT_AVATAR_LIMIT_BYTES`] it must,
/// or a legal upload would never reach the handler.
///
/// `POST /accounts` runs in the limiter's `app_account_creation` bucket
/// (Pleroma throttles registration there; see [`crate::rate_limit`]), so
/// the whole `state` rides along for the middleware layer.
///
/// [`DEFAULT_AVATAR_LIMIT_BYTES`]: crate::config::DEFAULT_AVATAR_LIMIT_BYTES
pub fn core_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/accounts",
            post(create_account)
                .layer(crate::rate_limit::rate_limit(state, "app_account_creation")),
        )
        .route("/accounts/verify_credentials", get(verify_credentials))
        .route(
            "/accounts/update_credentials",
            patch(update_credentials).post(update_credentials),
        )
        .layer(upload_body_limit(&state.config))
}

/// The `ActivityPub`-backed `/api/v1/accounts` routes, merged into
/// [`crate::api::v1::activitypub_router`].
///
/// They read or write other accounts, relationships, follows and statuses.
///
/// The follow routes (`POST /accounts/{id}/follow`, `…/unfollow` and
/// `POST /follows`) run in `relations_actions`, the bucket Pleroma throttles
/// every relationship write through, so the whole `state` rides along for
/// the middleware layer.
pub fn activitypub_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/follows",
            post(follow_by_uri).layer(crate::rate_limit::rate_limit(state, "relations_actions")),
        )
        .route("/accounts", get(list_accounts))
        .route("/accounts/relationships", get(relationships))
        .route("/accounts/lookup", get(lookup_account))
        .route("/accounts/{id}/followers", get(list_followers))
        .route("/accounts/{id}/following", get(list_following))
        .route("/accounts/{id}/statuses", get(list_statuses))
        .route("/accounts/{id}/note", post(set_note))
        .route(
            "/accounts/{id}/follow",
            post(follow_account).layer(crate::rate_limit::rate_limit(state, "relations_actions")),
        )
        .route(
            "/accounts/{id}/unfollow",
            post(unfollow_account).layer(crate::rate_limit::rate_limit(state, "relations_actions")),
        )
        .route(
            "/accounts/{id}/remove_from_followers",
            post(remove_from_followers),
        )
        .route("/accounts/{id}", get(show_account))
}

/// The email-confirmation route, merged by [`crate::api`] at `/api/account`.
///
/// That is the prefix Pleroma serves the confirmation link on
/// (`/api/account/confirm_email/:user_id/:token`,
/// `lib/pleroma/web/router.ex`), which belongs to neither versioned API nor
/// to `/api/pleroma`.
///
/// No body limit is raised here: the route reads its whole request from the
/// path and runs no credential, so the app-wide
/// [`MAX_BODY_BYTES`](crate::middleware::MAX_BODY_BYTES) is more than
/// enough.
pub fn email_confirmation_router() -> Router<AppState> {
    Router::new().route("/confirm_email/{user_id}/{token}", get(confirm_email))
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
    let emojis = AccountEmojis::load_one(&state.db, found).await?;
    let account = emojis.fill(account, found, &state.config);
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
