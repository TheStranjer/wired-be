//! The follow writes: `POST /api/v1/accounts/{id}/follow`,
//! `…/{id}/unfollow` and the legacy `POST /api/v1/follows`.
//!
//! `resolve_actor_or_fetch` is the federation arm the lookup shares,
//! and `after_follow_changed` is the hook every follow write runs.
//!
//! `POST /api/v1/accounts/{id}/follow` and `…/unfollow` are Pleroma's
//! `AccountController.follow` and `.unfollow` over `MastodonAPI.follow` /
//! `CommonAPI.unfollow` (`lib/pleroma/web/mastodon_api/controllers/
//! account_controller.ex`, `lib/pleroma/web/mastodon_api/mastodon_api.ex`,
//! `lib/pleroma/user.ex`): [`AuthUser`] plus the `write:follows` / `follow`
//! scope, both running in the limiter's `relations_actions` bucket. Follow
//! writes through [`follow::follow`] and unfollow through
//! [`follow::unfollow`]; both answer the
//! caller's [`Relationship`] to the account, read back off the row so the
//! answer states the stored fact rather than trusting the request. Follow
//! takes the two per-follow toggles through [`ApiParams`] — `reblogs`
//! (default `true`) and `notify` (default `false`), the JSON, form and query
//! spellings alike — and stores the ones the request named with
//! [`set_toggles`](crate::models::follow::set_toggles), which is how a repeat
//! follow of an existing pair updates its flags instead of being a plain
//! no-op; a parameter the body left out keeps what the row holds. Errors:
//! the malformed, unknown or deactivated id is the usual 404, following
//! yourself is the model's 422 `{"error":"Can not follow yourself"}`, and
//! unfollowing an account the caller does not follow is no error at all
//! (where Pleroma's controller answers "Not subscribed!", this ticket is
//! idempotent) — the unchanged [`Relationship`] answers it with 200. Creating
//! or withdrawing a follow is also the moment the followee is told — this
//! module's `after_follow_changed` hook lands the
//! [`notify`] module's follow telling — and the moment a REMOTE followee is
//! told too: the same hook hands the pair's `Follow` (or the `Undo` of it)
//! to [`crate::services::federation_out`], which queues the delivery.
//!
//! `POST /api/v1/follows` is Pleroma's `AccountController.follow_by_uri`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`): the
//! legacy Mastodon spelling of the follow write, keyed by handle instead of
//! by id. Same gates as `…/{id}/follow` — [`AuthUser`] plus the
//! `write:follows` / `follow` scope, the limiter's `relations_actions`
//! bucket — and the same two optional toggles, arriving beside the required
//! `uri` through [`ApiParams`]. The `uri` names the target three ways: an
//! `acct` handle (`user@domain`, with or without the leading `@`), resolved
//! by [`get_or_fetch_by_acct`], or an actor URL, resolved by
//! [`get_or_fetch_by_ap_id`]; both answer from the stored rows first and
//! reach for `WebFinger` and the
//! actor fetch only for a handle this instance has never seen. What the
//! fetch then does is exactly what `…/{id}/follow` does with its resolved
//! account — the same [`follow::follow`] write, the same toggles, the same
//! `after_follow_changed` hook and the same refusals (a target that resolves
//! to no live account is the shared 404, following yourself the model's
//! 422). The answer is the target's [`Account`] entity rendered for the
//! caller — not the [`Relationship`] the id-keyed route answers — because
//! this legacy endpoint returns the account it followed, with the caller's
//! relationship embedded in `pleroma.relationship`. An absent, blank or
//! `null` `uri` is Mastodon's 400 "Missing parameter: uri".
//!

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use tracing::debug;

use crate::{
    api::{
        helpers::{visible_account_or_404, visible_to},
        pleroma_account_settings::looks_like_ap_id,
    },
    auth::AuthUser,
    entities::{account::Account, relationship::Relationship},
    error::{AppError, AppResult},
    extract::{ApiParams, flex},
    federation::{
        fetch_actor::{get_or_fetch_by_acct, get_or_fetch_by_ap_id},
        webfinger_client::strip_finger_prefixes,
    },
    models::{
        follow::{self, Follow, FollowState, FollowToggles},
        user::User,
    },
    oauth::scopes::{FOLLOW, WRITE_FOLLOWS},
    services::{federation_out, notify},
    state::AppState,
};

use super::{notes_followers::one_relationship, render_account};

/// The 400 body message when the `uri` parameter of `POST /api/v1/follows`
/// is absent, blank or `null` (Mastodon's `ParameterMissing` wording).
const MISSING_URI: &str = "Missing parameter: uri";

/// The request parameters of `POST /api/v1/accounts/{id}/follow`: Mastodon's
/// two per-follow toggles, read from the query string, a form body or a JSON
/// body alike ([`ApiParams`], decision 10 — clients use all three, and a
/// request may carry no body at all). Both are optional, and a form's
/// `"true"` / `"1"` spells the same thing through [`flex::opt_bool`]; anything
/// that is neither is the extractor's 422.
#[derive(Debug, Deserialize)]
pub(super) struct FollowParams {
    /// Mastodon's `reblogs`, the row's [`Follow::show_reblogs`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    reblogs: Option<bool>,
    /// Mastodon's `notify`, the row's [`Follow::notify`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    notify: Option<bool>,
}

impl FollowParams {
    /// The toggles to store, exactly the parameters the request named: an
    /// absent one keeps the flag the row holds, which needs no default here
    /// because the documented ones (`reblogs` true, `notify` false) *are*
    /// the `follows` column defaults a fresh row is inserted with (see
    /// [`set_toggles`](crate::models::follow::set_toggles)).
    const fn into_toggles(self) -> FollowToggles {
        FollowToggles {
            show_reblogs: self.reblogs,
            notify: self.notify,
        }
    }
}

/// `POST /api/v1/accounts/{id}/follow` — follow the account and answer the
/// caller's [`Relationship`] to it: `following: true` when this instance
/// grants the follow on the spot (its own unlocked account), `requested: true`
/// when the target is locked and the request waits on
/// `/api/v1/follow_requests`. The two toggles the request names are stored on
/// the relationship the write created *or* found already there, so a repeat
/// follow of a pair is how a client rewrites its flags (Pleroma's `follow`
/// does the same: the second call changes nothing but the toggles).
///
/// The account is resolved by [`visible_account_or_404`] before the write, so a
/// malformed, unknown or deactivated id is the shared 404 whatever the body
/// asked for; following the caller's own account is the model's 422 (its
/// wording, Pleroma's `Can not follow yourself`). A pair blocked either way is
/// the model's own 403 refusal, which stands between this route and the write.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::NotFound`] for a
/// malformed, unknown or deactivated account id; [`AppError::Unprocessable`]
/// with `"Can not follow yourself"`, with the model's block refusal, or with
/// the extractor's message when a toggle is not a boolean. Any database
/// failure arrives through `?` as [`AppError::Database`].
pub(super) async fn follow_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<FollowParams>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let target = visible_account_or_404(&state, &id, Some(&auth.0)).await?;
    let toggles = params.into_toggles();
    let created = follow::follow(&state.db, &state.config.base_url, &auth.0, &target).await?;
    let stored = store_toggles(&state, created, toggles).await?;
    debug!(
        follow = %stored.id,
        user = %auth.0.id,
        target = %target.id,
        granted = stored.state == FollowState::Accepted,
        reblogs = stored.show_reblogs,
        notifying = stored.notify,
        "account followed"
    );
    after_follow_changed(&state, &auth.0, &target, Some(&stored), true).await;
    one_relationship(&state, auth.0.id, target.id).await
}

/// Writes the toggles a follow named onto the row [`follow::follow`] returned
/// and hands that row back as it now stands, so the log and the federation
/// hook state the stored fact rather than the request. A call that named
/// neither toggle writes nothing, and a relationship the pair lost to a
/// concurrent unfollow between the two statements keeps the row the follow
/// returned.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn store_toggles(
    state: &AppState,
    created: Follow,
    toggles: FollowToggles,
) -> AppResult<Follow> {
    if toggles.is_empty() {
        return Ok(created);
    }
    let updated =
        follow::set_toggles(&state.db, created.follower_id, created.followee_id, toggles).await?;
    Ok(updated.unwrap_or(created))
}

/// The request parameters of `POST /api/v1/follows`: the required `uri`
/// beside the same two per-follow [`toggles`](FollowParams) the id-keyed
/// follow route reads, from the query string, a form body or a JSON body
/// alike ([`ApiParams`], decision 10). `uri` is optional in the type only
/// because its absence is the documented 400 the type cannot express (the
/// [`LookupQuery`] pattern); the toggles are the documented optionals,
/// each a [`flex::opt_bool`] so a form's `"1"` spells the same as JSON
/// `true` and anything else is the extractor's 422.
#[derive(Debug, Deserialize)]
pub(super) struct FollowByUriParams {
    /// The account to follow: `user@domain`, `@user@domain` or an actor
    /// URL, resolved by [`resolve_actor_or_fetch`].
    #[serde(default)]
    uri: Option<String>,
    /// Mastodon's `reblogs`, the row's [`Follow::show_reblogs`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    reblogs: Option<bool>,
    /// Mastodon's `notify`, the row's [`Follow::notify`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    notify: Option<bool>,
}

impl FollowByUriParams {
    /// The toggles exactly as [`FollowParams::into_toggles`] reads them for
    /// the id-keyed route: an absent one keeps what the row holds. Not a
    /// `const fn` like that one, because this type owns the `uri` text.
    fn into_toggles(self) -> FollowToggles {
        FollowToggles {
            show_reblogs: self.reblogs,
            notify: self.notify,
        }
    }
}

/// `POST /api/v1/follows` — follow the account a `uri` names (Pleroma's
/// `.follow_by_uri`) and answer its [`Account`] entity rendered for the
/// caller, their [`Relationship`] embedded in `pleroma.relationship`.
///
/// The gates and the write are [`follow_account`]'s, applied to a target
/// resolved from the parameter instead of the path: the scope check first,
/// then the `uri` ([`MISSING_URI`] for an absent, blank or `null` one),
/// then [`resolve_actor_or_fetch`], and the very same
/// [`follow::follow`] + [`store_toggles`] sequence, the same log and the
/// same [`after_follow_changed`] hook — so the self refusal, the block
/// refusal and the locked-target request are the model's own answers.
/// Where Pleroma's controller hands off to the relationship-rendering
/// `follow` action, this legacy Mastodon endpoint answers the followed
/// account itself (decision: the ticket pins the [`Account`] entity).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::BadRequest`] for a
/// missing `uri`; [`AppError::NotFound`] when the `uri` resolves to no live
/// account; [`AppError::Unprocessable`] with `"Can not follow yourself"`
/// or the extractor's message when a toggle is not a boolean; the model's
/// [`AppError::Forbidden`] block refusal. Any database failure arrives
/// through `?` as [`AppError::Database`].
pub(super) async fn follow_by_uri(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<FollowByUriParams>,
) -> AppResult<Json<Account>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let Some(uri) = params
        .uri
        .as_deref()
        .map(str::trim)
        .filter(|uri| !uri.is_empty())
    else {
        return Err(AppError::BadRequest(MISSING_URI.to_owned()));
    };
    let target = resolve_actor_or_fetch(&state, uri, Some(&auth.0)).await?;
    let toggles = params.into_toggles();
    let created = follow::follow(&state.db, &state.config.base_url, &auth.0, &target).await?;
    let stored = store_toggles(&state, created, toggles).await?;
    debug!(
        follow = %stored.id,
        user = %auth.0.id,
        target = %target.id,
        granted = stored.state == FollowState::Accepted,
        reblogs = stored.show_reblogs,
        notifying = stored.notify,
        "account followed by uri"
    );
    after_follow_changed(&state, &auth.0, &target, Some(&stored), true).await;
    Ok(Json(render_account(&state, Some(&auth.0), &target).await?))
}

/// Resolves `uri` to the account it names: an `http(s)` URL goes through
/// [`get_or_fetch_by_ap_id`], anything else is an `acct` handle — with a
/// leading `@` or `acct:` prefix stripped — through [`get_or_fetch_by_acct`].
/// Both answer from the stored rows first, so a local account or a mirror
/// resolves without the network, and only an unseen handle reaches for
/// `WebFinger` and the actor fetch. Whatever the fetchers refuse — an
/// unresolvable handle, an unreachable or invalid actor, an unknown local
/// URL — and a row that resolves but is deactivated is the same shared
/// [`AppError::NotFound`] the id-keyed routes give through
/// [`visible_account_or_404`] — as is one `viewer` may not see
/// ([`visible_to`]: awaiting approval or email confirmation); the fetcher's
/// own reason is logged, never sent (decision 8). This is the shared federation arm of `POST
/// /api/v1/follows` and of `GET /api/v1/accounts/lookup?resolve=true`.
pub(super) async fn resolve_actor_or_fetch(
    state: &AppState,
    uri: &str,
    viewer: Option<&User>,
) -> AppResult<User> {
    let fetched = if looks_like_ap_id(uri) {
        get_or_fetch_by_ap_id(state, uri).await
    } else {
        get_or_fetch_by_acct(state, strip_finger_prefixes(uri)).await
    };
    let resolved = fetched.map_err(|err| {
        debug!(%err, uri, "follow target could not be resolved");
        AppError::NotFound
    })?;
    if visible_to(&resolved, viewer) {
        Ok(resolved)
    } else {
        Err(AppError::NotFound)
    }
}

/// `POST /api/v1/accounts/{id}/unfollow` — withdraw the caller's follow of the
/// account and answer their [`Relationship`] to it, `following: false` and
/// `requested: false` afterwards.
///
/// [`follow::unfollow`] takes back an accepted follow or a request of the
/// caller's that is still waiting, and is content with neither: unfollowing an
/// account the caller never followed (or one that already stopped following
/// them, or a `rejected` mirror) changes nothing and is the unchanged
/// [`Relationship`] with 200 — this ticket's idempotence, where Pleroma's
/// controller would answer "Not subscribed!". The usual 404 resolves the
/// account first, and unfollowing *yourself* is one of those no-op withdrawals
/// rather than an error, because no follow row of the pair can exist.
///
/// # Errors
///
/// Those of [`follow_account`] minus the parameter and self-follow refusals.
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn unfollow_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let target = visible_account_or_404(&state, &id, Some(&auth.0)).await?;
    // The row the delete removes is read while it still exists: the `Undo` the
    // federation hook will one day send names the `Follow` activity the
    // relationship holds, and `unfollow` reports only whether a row was there.
    let previous = follow::get(&state.db, auth.0.id, target.id).await?;
    let removed = follow::unfollow(&state.db, &auth.0, &target).await?;
    debug!(
        user = %auth.0.id,
        target = %target.id,
        removed,
        "follow withdrawn"
    );
    after_follow_changed(
        &state,
        &auth.0,
        &target,
        previous.as_ref().filter(|_| removed),
        false,
    )
    .await;
    one_relationship(&state, auth.0.id, target.id).await
}

/// Post-write hook of [`follow_account`] and [`unfollow_account`]: the
/// notification telling the [`notify`] module owns
/// runs through here, and so does the federation of the `Follow` / `Undo`:
/// the write's remote half lands in [`crate::services::federation_out`],
/// which builds the activity from `follow`'s [`Follow::ap_id`] with its
/// maker's [`User::ap_id`] as actor and [`Follow::followee_id`]'s account as
/// object and sole recipient, and queues the delivery (decision 11).
///
/// `follow` is the relationship the write left behind (a `…/follow` always has
/// one; a `…/unfollow` names the row it deleted and is `None` for a pair with
/// nothing to take back, which is nothing to federate either). `following`
/// states which way the write moved the fact: `true` for the `Follow` a
/// `…/follow` created or reopened, `false` for the `Undo` of the `Follow` a
/// `…/unfollow` withdrew.
///
/// The hook never fails the request: both halves are fire-and-forget, and a
/// pair whose followed account is local federates nothing at all.
pub(crate) async fn after_follow_changed(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) {
    notify::on_follow(state, follower, target, follow, following).await;
    federation_out::after_follow_changed(state, follower, target, follow, following).await;
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::*;

    /// Deserializes a `POST /api/v1/follows` parameter map, the way the
    /// extractor hands form bodies over (every value a string).
    fn follow_by_uri_params(pairs: &[(&str, &str)]) -> FollowByUriParams {
        let map = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), json!(value)))
            .collect::<Map<String, Value>>();
        serde_json::from_value(Value::Object(map)).expect("the fixture params parse")
    }

    #[test]
    fn follow_by_uri_keeps_only_the_toggles_the_request_named() {
        let toggles = follow_by_uri_params(&[("uri", "@bob@test.example"), ("notify", "true")])
            .into_toggles();
        assert_eq!(
            (toggles.show_reblogs, toggles.notify),
            (None, Some(true)),
            "the named `notify` moves, the absent `reblogs` keeps the row"
        );
        let bare = follow_by_uri_params(&[("uri", "bob")]).into_toggles();
        assert!(bare.is_empty(), "a follow naming no toggle writes nothing");
    }

    #[test]
    fn follow_by_uri_reads_the_uri_wherever_it_was_sent() {
        let json: FollowByUriParams =
            serde_json::from_str(r#"{"uri": "https://remote.example/users/bob"}"#)
                .expect("a JSON body parses");
        assert_eq!(
            json.uri.as_deref(),
            Some("https://remote.example/users/bob"),
            "the JSON spelling"
        );
        let nulled = follow_by_uri_params(&[("uri", "")]);
        assert_eq!(
            nulled.uri.as_deref(),
            Some(""),
            "the form's empty string reads empty"
        );
        let absent: FollowByUriParams =
            serde_json::from_str(r#"{"uri": null}"#).expect("a null parses");
        assert_eq!(absent.uri, None, "and a JSON null is the absent one");
    }
}
