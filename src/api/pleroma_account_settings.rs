//! Account aliases and account moves: `/api/pleroma/aliases` and
//! `/api/pleroma/move_account` (WBE-137), Pleroma's `UtilController`
//! account-utilities actions.
//!
//! The reference is `list_aliases`, `add_alias`, `delete_alias` and
//! `move_account` in `lib/pleroma/web/pleroma_api/controllers/
//! util_controller.ex`, with the state change behind Pleroma's
//! `ActivityPub.move/3`
//! (`lib/pleroma/web/activity_pub/activity_pub.ex`) and
//! `FollowingRelationship.move_following/2`. The ticket shapes are
//! authoritative where Pleroma's differ.
//!
//! Every route requires a user token ([`AuthUser`]): `GET` asks the
//! `read:accounts` scope, everything that changes state the
//! `write:accounts` scope. State lives in three `users` columns —
//! `also_known_as` (the alias list of `ActivityPub` actor ids),
//! `moved_to_ap_id` (the entity's top-level `moved`) and `last_move_at`
//! (the move cooldown) — written through
//! [`set_also_known_as`](crate::models::user::set_also_known_as) and
//! [`mark_moved`](crate::models::user::mark_moved).
//!
//! The endpoints:
//!
//! - `GET /api/pleroma/aliases` — `{"aliases": [handle, …]}`: the stored
//!   actor ids rendered as `acct` handles (a mirrored actor by its
//!   full nickname, like Pleroma's `list_aliases`); an id this instance
//!   has no row for falls back to the raw URL.
//! - `PUT /api/pleroma/aliases` — body `alias` (an `acct` handle or an
//!   `ActivityPub` id), resolved through the federation get-or-fetch
//!   ([`get_or_fetch_by_acct`] / [`get_or_fetch_by_ap_id`]); a resolution
//!   failure is 404 `{"error":"Record not found"}`. The target's actor id
//!   is appended to the list unless it is already there; `{"status":
//!   "success"}`.
//! - `DELETE /api/pleroma/aliases` — body `alias`: drops the matching
//!   entries (the string as given, and the actor id of the row that
//!   handle names), then `{"status":"success"}`. Removing an alias that
//!   is not on the list changes nothing and still succeeds.
//! - `POST /api/pleroma/move_account` — `password` plus
//!   `target_account` (an `acct` handle), gated in Pleroma's order: the
//!   password first (a wrong one answers 200 `{"error":
//!   "Invalid password."}` — the shape Pleroma's `json(conn, %{error:
//!   msg})` renders), the target's resolution 404 `{"error":"Target
//!   account not found."}`, and the account move itself — Pleroma's
//!   `ActivityPub.move` refuses when the target has not listed the
//!   origin in `alsoKnownAs` — as 200 with Pleroma's message. The
//!   cooldown (Pleroma's once-per-30-days move limit, kept in
//!   `last_move_at`) is checked at the same 200-with-error shape. Once
//!   every gate passes the account is marked moved, its followers
//!   transfer, and the `after_account_moved` hook runs.
//!
//! The follower transfer mirrors Pleroma's `move_following`: every
//! *local* follower with an accepted relationship that still allows its
//! following list to move follows the target through the normal
//! [`follow::follow`] logic (so the follow is accepted immediately when
//! this instance owns an unlocked target, and a blocked pair is refused
//! and skipped), and unfollows the origin; the denormalised counters move
//! with those two calls. Remote followers' relationships are their own
//! instances' to rewrite, and stay.
//!
//! Federation of the `Move` activity and the move notifications are not
//! part of this ticket; they attach to the `after_account_moved` hook
//! later.
//!
//! [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
//! [`get_or_fetch_by_ap_id`]: crate::federation::fetch_actor::get_or_fetch_by_ap_id

use std::fmt;

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    auth::AuthUser,
    entities::account::full_nickname,
    error::{AppError, AppResult},
    extract::ApiBody,
    federation::fetch_actor::{get_or_fetch_by_acct, get_or_fetch_by_ap_id},
    models::{
        follow,
        user::{self, User, get_by_ap_id},
    },
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    state::AppState,
    util::password::verify_password,
};

/// 200 error body when the submitted password does not match the stored
/// hash (Pleroma's `confirm_current_password` wording, period included —
/// `move_account` renders it through `json(conn, %{error: msg})`, which
/// is why these failures are 200s, not 4xx).
const INVALID_PASSWORD: &str = "Invalid password.";
/// 404 error body when the move target cannot be resolved (Pleroma's
/// `move_account` `:not_found` branch, period included).
const TARGET_NOT_FOUND: &str = "Target account not found.";
/// 200 error body when the target has not listed the moving account as
/// an alias (Pleroma's `ActivityPub.move` refusal, backticks and all).
const MOVE_ALIAS_REQUIRED: &str = "Target account must have the origin in `alsoKnownAs`";
/// 200 error body when the account moved within the cooldown window
/// (Pleroma's `move_account` rate limit wording).
const MOVE_COOLDOWN: &str = "You can't move your account more than once in 30 days";
/// How long an account must wait between moves (Pleroma's
/// once-per-30-days `move_account` cooldown).
const MOVE_COOLDOWN_WINDOW: Duration = Duration::days(30);

/// The `/api/pleroma` account-settings router, merged into
/// [`crate::api::pleroma`].
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/aliases",
            get(list_aliases).put(add_alias).delete(delete_alias),
        )
        .route("/move_account", post(move_account))
}

/// The `{"aliases": […]}` body of [`list_aliases`].
#[derive(Debug, Serialize)]
struct AliasesBody {
    /// The account's aliases as `acct` handles (or raw actor URLs when
    /// this instance has no row for the id).
    aliases: Vec<String>,
}

/// The parameters of `PUT`/`DELETE /api/pleroma/aliases`: the alias to
/// add or drop, as an `acct` handle or an `ActivityPub` actor id.
#[derive(Debug, Deserialize)]
struct AliasParams {
    /// The alias the client names.
    alias: String,
}

/// The parameters of `POST /api/pleroma/move_account`.
#[derive(Deserialize)]
struct MoveParams {
    /// The account's current password; secret material.
    password: String,
    /// The `acct` handle to move to.
    target_account: String,
}

/// `Debug` redacts the password.
impl fmt::Debug for MoveParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoveParams")
            .field("password", &"<redacted>")
            .field("target_account", &self.target_account)
            .finish()
    }
}

/// `GET /api/pleroma/aliases` — the account's alias list as handles.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:accounts` check. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn list_aliases(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<AliasesBody>> {
    auth.require(READ_ACCOUNTS)?;
    let aliases = alias_handles(&state, &auth.0).await?;
    Ok(Json(AliasesBody { aliases }))
}

/// The stored alias actor ids rendered one by one: the full `acct`
/// nickname of the mirrored actor when this instance has a row for the
/// id (Pleroma's `list_aliases` maps through `User.full_nickname/1`),
/// the raw URL when it has none.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn alias_handles(state: &AppState, account: &User) -> AppResult<Vec<String>> {
    let mut handles = Vec::with_capacity(account.also_known_as.len());
    for ap_id in &account.also_known_as {
        let handle = get_by_ap_id(&state.db, ap_id).await?.map_or_else(
            || ap_id.clone(),
            |found| full_nickname(&found, &state.config),
        );
        handles.push(handle);
    }
    Ok(handles)
}

/// `PUT /api/pleroma/aliases` — append an alias. The target is resolved
/// through the federation get-or-fetch (see [`resolve_alias`]); its
/// actor id joins the list unless it is already there, so adding twice
/// is one entry. Answers `{"status":"success"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] (the generic
/// "Record not found" body) when the alias resolves to no account. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn add_alias(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<AliasParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let target = resolve_alias(&state, &params.alias).await?;
    let mut aliases = auth.0.also_known_as.clone();
    if !aliases.contains(&target.ap_id) {
        aliases.push(target.ap_id.clone());
        user::set_also_known_as(&state.db, auth.0.id, &aliases).await?;
    }
    Ok(success_body())
}

/// `DELETE /api/pleroma/aliases` — drop an alias. Both spellings a
/// client may echo back are removed: the string exactly as given (an
/// entry [`list_aliases`] rendered as its raw URL falls back to that)
/// and, when the input names a row this instance knows, that row's
/// actor id. A list without a matching entry is written unchanged; the
/// answer is `{"status":"success"}` either way.
///
/// Unlike `PUT` this never touches the network: removal needs no fresh
/// view of the target, only its stored id.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn delete_alias(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<AliasParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let stored = &auth.0.also_known_as;
    let kept = dropped_aliases(&state, &params.alias, stored).await?;
    if kept.len() != stored.len() {
        user::set_also_known_as(&state.db, auth.0.id, &kept).await?;
    }
    Ok(success_body())
}

/// The alias list with `input` removed, as [`delete_alias`] writes it:
/// entries equal to the input verbatim, or to the actor id of the row
/// the input names (the mirrored-alias case, where `GET` showed a handle
/// rather than the raw id). The `Vec` is rebuilt (order-preserving) so a
/// client that added the same alias under both spellings is fully
/// cleared.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn dropped_aliases(
    state: &AppState,
    input: &str,
    stored: &[String],
) -> AppResult<Vec<String>> {
    let known_id = user::get_by_acct(&state.db, &state.config, input)
        .await?
        .map(|found| found.ap_id);
    Ok(without_alias(stored, input, known_id.as_deref()))
}

/// The list-editing half of [`dropped_aliases`], pure: keep every entry
/// that is neither the input verbatim nor the resolved actor id
/// (`known_id`, `None` when the input names no row).
fn without_alias(stored: &[String], input: &str, known_id: Option<&str>) -> Vec<String> {
    stored
        .iter()
        .filter(|alias| **alias != input && Some(alias.as_str()) != known_id)
        .cloned()
        .collect()
}

/// `POST /api/pleroma/move_account` — move the calling account to
/// `target_account`. The gates run in Pleroma's order (password, target
/// resolution, alias precondition, cooldown), all before any state
/// changes; the first one that refuses answers, and only a fully gated
/// move marks the account moved and transfers its followers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFoundMessage`] (404
/// "Target account not found.") when the target resolves to no account.
/// The password, alias and cooldown refusals are 200 error bodies, as
/// Pleroma renders them. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn move_account(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<MoveParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    let target = resolve_move_target(&state, &params.target_account).await?;
    if !target_lists_origin(&target, &auth.0) {
        return Ok(error_body(MOVE_ALIAS_REQUIRED));
    }
    if moved_within_cooldown(auth.0.last_move_at) {
        return Ok(error_body(MOVE_COOLDOWN));
    }
    user::mark_moved(&state.db, auth.0.id, &target.ap_id).await?;
    transfer_followers(&state, &auth.0, &target).await?;
    after_account_moved(&state, &auth.0, &target);
    Ok(success_body())
}

/// Whether `password` matches the account's stored hash. A row without
/// one (a bot-created account, a mirror) fails exactly like a wrong
/// password, as Pleroma's `confirm_current_password` does.
fn password_accepted(account: &User, password: &str) -> bool {
    account
        .password_hash
        .as_deref()
        .is_some_and(|hash| verify_password(password, hash))
}

/// Resolves an alias string to the account behind it: an `http(s)` URL
/// goes through [`get_or_fetch_by_ap_id`], anything else through
/// [`get_or_fetch_by_acct`] (which answers from the stored nickname when
/// there is one, and only then reaches for `WebFinger`). A failure to
/// resolve is reported as the plain 404 the ticket fixes; the fetcher's
/// own reason is logged, never sent.
///
/// [`get_or_fetch_by_ap_id`]: crate::federation::fetch_actor::get_or_fetch_by_ap_id
/// [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
async fn resolve_alias(state: &AppState, alias: &str) -> AppResult<User> {
    let fetched = if looks_like_ap_id(alias) {
        get_or_fetch_by_ap_id(state, alias).await
    } else {
        get_or_fetch_by_acct(state, alias).await
    };
    fetched.map_err(|err| {
        debug!(%err, alias, "alias target could not be resolved");
        AppError::NotFound
    })
}

/// Resolves a move target like [`resolve_alias`], replacing its generic
/// 404 with the message the `move_account` endpoint owes ("Target
/// account not found.").
async fn resolve_move_target(state: &AppState, acct: &str) -> AppResult<User> {
    resolve_alias(state, acct).await.map_err(|err| {
        debug!(%err, acct, "move target could not be resolved");
        AppError::NotFoundMessage(TARGET_NOT_FOUND.to_owned())
    })
}

/// Whether `text` is given as an `ActivityPub` actor id rather than an
/// `acct` handle: a case-insensitive `http://` or `https://` prefix.
fn looks_like_ap_id(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// The `ActivityPub.move` precondition: the target must have listed the
/// moving account's own actor id among its aliases.
fn target_lists_origin(target: &User, origin: &User) -> bool {
    target.also_known_as.contains(&origin.ap_id)
}

/// Whether `last_move_at` falls inside the [`MOVE_COOLDOWN_WINDOW`]. A
/// stamp in the future (clock skew) counts as recent rather than
/// letting the account move twice around it.
fn moved_within_cooldown(last_move_at: Option<DateTime<Utc>>) -> bool {
    last_move_at.is_some_and(|moved| Utc::now().signed_duration_since(moved) < MOVE_COOLDOWN_WINDOW)
}

/// Moves `origin`'s followers onto `target`: every candidate from
/// [`follow::move_followers_of`] (local, accepted, `allow_following_move`)
/// follows the target through [`follow::follow`] — accepted on the spot
/// when this instance owns an unlocked target, refused-and-skipped when
/// the pair is blocked — and then unfollows the origin through
/// [`follow::unfollow`]. Both calls move the denormalised counters; the
/// follower's own `following_count` ends unchanged (up with the target,
/// down with the origin). A follower whose re-follow failed keeps its
/// original relationship untouched: nothing about the move is applied
/// half-way.
///
/// # Errors
///
/// Any database failure from the candidate query or the unfollow arrives
/// as [`AppError::Database`]; per-follower [`AppError::Unprocessable`]
/// refusals (a blocked pair, Pleroma's `deny_follow_blocked`) only log
/// and skip.
async fn transfer_followers(state: &AppState, origin: &User, target: &User) -> AppResult<()> {
    for follower in follow::move_followers_of(&state.db, origin.id).await? {
        if let Err(err) = follow::follow(&state.db, &state.config.base_url, &follower, target).await
        {
            debug!(%err, follower = %follower.nickname, "skipped during follower transfer");
            continue;
        }
        follow::unfollow(&state.db, &follower, origin)
            .await
            .map(drop)?;
    }
    Ok(())
}

/// The hook the move runs once the account is marked moved and its
/// followers have transferred. It is deliberately empty: federating the
/// `Move` activity (Pleroma's `ActivityPub.move` inserts it into the
/// origin's outbox and delivers to the follower collection) and the move
/// notifications attach here with the federation ticket that owns the
/// outbox pipeline. Later tickets replace the body, not the call sites.
const fn after_account_moved(_state: &AppState, _old: &User, _new: &User) {}

/// The `{"status":"success"}` body all four endpoints answer success
/// with.
fn success_body() -> Json<Value> {
    Json(json!({"status": "success"}))
}

/// The 200 error body Pleroma's `json(conn, %{error: msg})` renders for
/// the refusals that are not status errors.
fn error_body(message: &str) -> Json<Value> {
    Json(json!({"error": message}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// An account id the fixtures hand to the pure helpers.
    const ALIAS: &str = "https://old.example/users/alice";

    /// A [`User`]-shaped alias list for the pure list-editing assertions.
    fn aliases(extra: &[&str]) -> Vec<String> {
        let mut list = vec![ALIAS.to_owned()];
        list.extend(extra.iter().map(|alias| (*alias).to_owned()));
        list
    }

    #[test]
    fn ap_ids_are_told_apart_from_handles() {
        for url in [
            "https://remote.example/users/bob",
            "http://remote.example/users/bob",
            "HTTPS://REMOTE.EXAMPLE/users/bob",
        ] {
            assert!(looks_like_ap_id(url), "`{url}` is an actor id");
        }
        for handle in ["bob@remote.example", "alice", "ftp://x.example/a"] {
            assert!(!looks_like_ap_id(handle), "`{handle}` is not an actor id");
        }
    }

    #[test]
    fn the_move_precondition_reads_the_target_s_alias_list() {
        let mut origin = template_user("alice");
        origin.ap_id = ALIAS.to_owned();
        let mut target = template_user("bob@remote.example");

        assert!(!target_lists_origin(&target, &origin), "no aliases yet");
        target.also_known_as = vec!["https://elsewhere.example/users/x".to_owned()];
        assert!(
            !target_lists_origin(&target, &origin),
            "an alias of someone else is not the origin"
        );
        target.also_known_as = aliases(&[]);
        assert!(
            target_lists_origin(&target, &origin),
            "the origin's own actor id satisfies the precondition"
        );
    }

    #[test]
    fn cooldown_is_thirty_days_and_clock_skew_counts_recent() {
        assert!(
            !moved_within_cooldown(None),
            "an account that never moved may move"
        );
        assert!(
            moved_within_cooldown(Some(Utc::now() - Duration::days(29))),
            "29 days ago is inside the window"
        );
        assert!(
            !moved_within_cooldown(Some(Utc::now() - Duration::days(31))),
            "31 days ago is outside it"
        );
        assert!(
            moved_within_cooldown(Some(Utc::now() + Duration::days(1))),
            "a future stamp is treated as recent, not as free rein"
        );
    }

    /// A bare [`User`] with the two fields the pure helpers read and the
    /// rest at the constructor defaults (the shape of a fresh row).
    fn template_user(nickname: &str) -> User {
        User {
            id: Uuid::now_v7(),
            nickname: nickname.to_owned(),
            ap_id: format!("https://test.example/users/{nickname}"),
            local: user::Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: String::new(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: user::Flag::new(false),
            bot: user::Flag::new(false),
            discoverable: user::Flag::new(false),
            fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: user::Flag::new(false),
            is_moderator: user::Flag::new(false),
            is_active: user::Flag::new(true),
            hide_followers: user::Flag::new(false),
            hide_follows: user::Flag::new(false),
            hide_followers_count: user::Flag::new(false),
            hide_follows_count: user::Flag::new(false),
            hide_favorites: user::Flag::new(true),
            allow_following_move: user::Flag::new(true),
            skip_thread_containment: user::Flag::new(false),
            accepts_chat_messages: user::Flag::new(true),
            show_role: user::Flag::new(true),
            no_rich_text: user::Flag::new(false),
            default_scope: "public".to_owned(),
            sensitive_default: user::Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            background_image_url: None,
            birthday: None,
            show_birthday: user::Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: user::Flag::new(true),
            approval_pending: user::Flag::new(false),
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
            mfa_settings: user::mfa::MfaSettings::default_value(),
            preferred_frontend: None,
        }
    }

    /// The pure removal filter behind [`dropped_aliases`]: both spellings
    /// of the named alias go, everything else stays in order.
    #[test]
    fn removal_drops_the_verbatim_input_and_the_resolved_id() {
        let bob = "https://test.example/users/bob";
        let stored = aliases(&[bob, "https://x.example/users/y"]);

        let kept = without_alias(&stored, bob, Some(bob));
        assert_eq!(
            kept,
            vec![ALIAS.to_owned(), "https://x.example/users/y".to_owned()],
            "the verbatim spelling drops the entry with the same id"
        );

        let by_handle = without_alias(&stored, "bob@remote.example", Some(bob));
        assert_eq!(
            by_handle,
            vec![ALIAS.to_owned(), "https://x.example/users/y".to_owned()],
            "a handle spelling drops the row's actor id"
        );

        let untouched = without_alias(&stored, "ghost@nowhere", None);
        assert_eq!(
            untouched, stored,
            "an input nothing matches keeps every entry"
        );
    }

    #[test]
    fn success_and_error_bodies_have_the_documented_shapes() {
        assert_eq!(success_body().0, json!({"status": "success"}));
        assert_eq!(
            error_body(INVALID_PASSWORD).0,
            json!({"error": "Invalid password."})
        );
    }

    #[test]
    fn move_params_debug_redacts_the_password() {
        let params = MoveParams {
            password: "hunter2".to_owned(),
            target_account: "bob@remote.example".to_owned(),
        };
        let shown = format!("{params:?}");
        assert!(
            !shown.contains("hunter2") && shown.contains("bob@remote.example"),
            "the Debug hides the credential and keeps the handle: {shown}"
        );
    }
}
