//! The admin federation endpoints: the relay ledger, the instance
//! deletion and the emoji reload under `/api/v1/pleroma/admin` (WBE-203).
//!
//! The relay ledger and the instance deletion are `ActivityPub`-backed
//! ([`router`]); `POST /reload_emoji` is core ([`emoji_router`]), since it
//! only resyncs the local emoji packs.
//!
//! The reference is Pleroma's `AdminAPI.RelayController`
//! (`lib/pleroma/web/admin_api/controllers/relay_controller.ex`), the
//! `delete` action of `AdminAPI.InstanceController`
//! (`lib/pleroma/web/admin_api/controllers/instance_controller.ex`) and
//! `AdminAPIController.reload_emoji`
//! (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`).
//! Every route takes the [`AdminUser`] extractor, and the scope half of
//! the gate is the [`AdminUser::require`] call each handler makes first:
//! `admin:read:relays` on the relay listing and `admin:write:follows` on
//! the relay follow and unfollow (Pleroma's `relay_controller.ex` asks
//! `admin:read` and `admin:write:follows`), `admin:write:federation` on
//! the instance deletion (Pleroma asks its `admin:write:accounts` +
//! `admin:write:statuses` pair), and `admin:write:emoji` on the reload
//! (Pleroma's bare `admin:write`, narrowed to the area the emoji-pack
//! writes already live in). Every write records its
//! [`moderation_log`] entry — `relay_follow`, `relay_unfollow`,
//! `delete_instance`, `reload_emoji` — beside a `debug!` line.
//!
//! - `GET /relay` (`index`) — Pleroma's `{relays: [{actor,
//!   followed_back}]}` ledger: one entry per [`relays`](crate::models::relay)
//!   row, oldest subscription first, `actor` the relay's `ActivityPub` id
//!   and `followed_back` the fact that the relay's mirror holds an
//!   *accepted* follow of the instance actor — where Pleroma asks the
//!   service actor's `Following` set, this instance asks the `follows`
//!   table through
//!   [`relay::list_with_followed_back`].
//! - `POST /relay` (`follow`) — the body's `relay_url` (an actor URL or
//!   a bare host, either spelling [`services::relay::follow_relay`]
//!   resolves) runs the instance actor's `Follow`; the answer is the one
//!   `{actor, followed_back}` pair, and any refusal the service raises —
//!   an unusable address, an actor no relay wears, an unreachable or
//!   gone document — is the 400 `{"error":"<message>"}` carrying that
//!   refusal's message (the ticket's call on Pleroma's 500).
//! - `DELETE /relay` (`unfollow`) — `relay_url` plus Pleroma's `force`,
//!   withdrawing the follow through [`services::relay::unfollow_relay`]:
//!   the `Undo` rides to the stored inbox, the `follows` row goes, and
//!   the `relays` row is deleted even when the relay is mirrored by
//!   nothing — which is what `force` buys there. The answer is the
//!   requested address as a JSON string; a relay this instance does not
//!   follow is the 400 `{"error":"Not found"}` (the ticket's wording over
//!   Pleroma's 500 and over this project's usual 404).
//! - `DELETE /instances/{instance}` (`delete`) — the domain's mirrored
//!   accounts: each is marked deleted and rides its own
//!   [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)
//!   sweep, the whole set queued in one batch, and the
//!   [`instances`](crate::models::instance) row is cleared; the answer is
//!   the domain as a JSON string, Pleroma's `json(conn, instance)`. This
//!   instance's own domain is the 400 `{"error":"Can't delete the local
//!   instance"}`. Deleting an unknown domain is a clean no-op — Pleroma's
//!   worker deletes its row only `if present`, and answers the same body
//!   either way.
//! - `POST /reload_emoji` (`reload_emoji`) — resyncs `custom_emojis`
//!   from the on-disk packs ([`emoji_packs::sync_custom_emojis`], the
//!   very pass startup runs) and answers the JSON string `"ok"`.
//!
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`moderation_log`]: crate::models::moderation_log
//! [`services::relay::follow_relay`]: crate::services::relay::follow_relay
//! [`services::relay::unfollow_relay`]: crate::services::relay::unfollow_relay
//! [`emoji_packs::sync_custom_emojis`]: crate::services::emoji_packs::sync_custom_emojis

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use serde::Deserialize;
use tracing::debug;

use crate::{
    auth::admin::AdminUser,
    entities::admin::{AdminRelay, AdminRelayList},
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    jobs::{QUEUE_BACKGROUND, deletion::KIND_DELETE_REMOTE, deletion::job_args},
    models::{instance, job, moderation_log, relay, user},
    services::{emoji_packs, instance_actor::ensure_instance_actor, relay as relay_service},
    state::AppState,
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area of the relay listing (`admin:read:relays`).
const AREA_RELAYS: &str = "relays";
/// The admin scope area of the relay follow and unfollow writes
/// (`admin:write:follows`, Pleroma's `OAuthScopesPlug` for `follow` and
/// `unfollow`); the listing stays on [`AREA_RELAYS`].
const AREA_FOLLOWS: &str = "follows";
/// The admin scope area of the instance deletion.
const AREA_FEDERATION: &str = "federation";
/// The admin scope area of the emoji reload — the same `emoji` area the
/// emoji-pack admin writes of [`crate::api::emoji_packs`] demand.
const AREA_EMOJI: &str = "emoji";
/// 400 body of `DELETE /relay` for a relay this instance does not
/// follow (the wording the ticket pins).
const NOT_FOUND: &str = "Not found";
/// 400 body of `DELETE /instances/{instance}` for this instance's own
/// domain — Pleroma's `Instance.delete` refusal, verbatim.
const CANNOT_DELETE_LOCAL: &str = "Can't delete the local instance";

/// The relay and remote-instance routes, `ActivityPub`-backed, merged into
/// [`super::activitypub_router`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/relay", get(list).post(follow).delete(unfollow))
        .route("/instances/{instance}", delete(delete_instance))
}

/// The emoji reload route, core, merged into [`super::core_router`].
///
/// It lives in this module because Pleroma's controller serves it with the
/// federation routes, but it only resyncs the local emoji packs and needs no
/// `ActivityPub` data.
pub fn emoji_router() -> Router<AppState> {
    Router::new().route("/reload_emoji", post(reload_emoji))
}

/// The body of `POST /relay` and `DELETE /relay`: Pleroma's single
/// `relay_url` parameter. It deserializes with a default so an absent
/// key reaches [`relay_service`] as the empty address it refuses — the
/// 400 then carries the service's own "no relay address was given"
/// message, the same refusal shape as every other failed write here.
#[derive(Debug, Deserialize)]
struct RelayBody {
    /// The relay actor's `ActivityPub` URL, or a bare host naming the
    /// conventional `https://{host}/relay` actor.
    #[serde(default)]
    relay_url: String,
}

/// [`RelayBody`] plus Pleroma's `force` on the withdrawal, read through
/// [`flex::opt_bool`] so a form request's `"true"` counts.
#[derive(Debug, Deserialize)]
struct RelayUnfollowBody {
    /// The relay address to withdraw.
    #[serde(default)]
    relay_url: String,
    /// Pleroma's `force`: also clear the local state when the relay
    /// cannot be reached. [`relay_service::unfollow_relay`] already does
    /// exactly that — an unmirrored relay's withdrawal deletes its
    /// `relays` row without sending anything — so the flag is taken,
    /// logged, and needs no branch here.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    force: Option<bool>,
}

/// `GET /api/v1/pleroma/admin/relay` — the ledger in Pleroma's
/// `{relays: [{actor, followed_back}]}` shape, one entry per stored
/// subscription.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read:relays` gate; [`AppError::Database`] from the
/// instance-actor read and the listing.
async fn list(State(state): State<AppState>, admin: AdminUser) -> AppResult<Json<AdminRelayList>> {
    admin.require(CLASS_READ, AREA_RELAYS)?;
    let instance = ensure_instance_actor(&state.db, &state.config).await?;
    let listings = relay::list_with_followed_back(&state.db, instance.id).await?;
    debug!(relays = listings.len(), "admin relay ledger listed");
    Ok(Json(AdminRelayList::from_listings(&listings)))
}

/// `POST /api/v1/pleroma/admin/relay` — Pleroma's `follow`: run
/// [`relay_service::follow_relay`] on the body's address, log the
/// `relay_follow` entry and answer the followed relay's
/// `{actor, followed_back}` pair.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write:follows` gate; [`AppError::BadRequest`] with the
/// refusal's message for any failure of the follow itself;
/// [`AppError::Database`] from the log write and the followed-back read.
async fn follow(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<RelayBody>,
) -> AppResult<Json<AdminRelay>> {
    admin.require(CLASS_WRITE, AREA_FOLLOWS)?;
    let subscription = relay_service::follow_relay(&state, &body.relay_url)
        .await
        .map_err(client_refusal)?;
    let _logged = moderation_log::relay_follow(&state.db, &admin.0, &body.relay_url).await?;
    let followed_back = followed_back(&state, &subscription.actor_ap_id).await?;
    debug!(
        relay = %subscription.actor_ap_id,
        followed_back,
        "admin relay followed"
    );
    Ok(Json(AdminRelay {
        actor: subscription.actor_ap_id,
        followed_back,
    }))
}

/// `DELETE /api/v1/pleroma/admin/relay` — Pleroma's `unfollow`: run
/// [`relay_service::unfollow_relay`] on the body's address (`force`
/// included in the log line, its job of clearing an unreachable relay's
/// row already done by the service) and answer the requested address as
/// a JSON string.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write:follows` gate; [`AppError::BadRequest`] with
/// [`NOT_FOUND`] for a relay that is not followed and with the refusal's
/// message for any other failed withdrawal; [`AppError::Database`] from
/// the log write.
async fn unfollow(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<RelayUnfollowBody>,
) -> AppResult<Json<String>> {
    admin.require(CLASS_WRITE, AREA_FOLLOWS)?;
    relay_service::unfollow_relay(&state, &body.relay_url)
        .await
        .map_err(unfollow_refusal)?;
    let _logged = moderation_log::relay_unfollow(&state.db, &admin.0, &body.relay_url).await?;
    debug!(
        relay = %body.relay_url,
        force = body.force.unwrap_or(false),
        "admin relay unfollowed"
    );
    Ok(Json(body.relay_url))
}

/// `DELETE /api/v1/pleroma/admin/instances/{instance}` — Pleroma's
/// instance `delete`: mark every remote mirror of the domain deleted,
/// queue one [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)
/// sweep per account in one batched transaction, clear the `instances`
/// row, and answer the domain as a JSON string. The sweeps run on the
/// background queue: this call only stages the deletion, exactly like
/// Pleroma's controller enqueuing its `DeleteWorker`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write:federation` gate; [`AppError::BadRequest`] with
/// [`CANNOT_DELETE_LOCAL`] for this instance's own domain;
/// [`AppError::Database`] from the marking, the queueing, the row delete
/// or the log write.
async fn delete_instance(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<String>> {
    admin.require(CLASS_WRITE, AREA_FEDERATION)?;
    let host = normalized_host(&raw);
    if host_is_local(state.config.domain(), &host) {
        return Err(AppError::BadRequest(CANNOT_DELETE_LOCAL.to_owned()));
    }
    let doomed = user::mark_deleted_by_domain(&state.db, &host).await?;
    let jobs: Vec<job::NewJob<'_>> = doomed
        .iter()
        .map(|id| job::NewJob {
            queue: QUEUE_BACKGROUND,
            kind: KIND_DELETE_REMOTE,
            args: job_args(*id),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        })
        .collect();
    let queued = job::enqueue_batch(&state.db, &jobs).await?;
    let _gone = instance::delete(&state.db, &host).await?;
    let _logged = moderation_log::delete_instance(&state.db, &admin.0, &host).await?;
    debug!(instance = %host, queued, "admin instance deleted");
    Ok(Json(host))
}

/// `POST /api/v1/pleroma/admin/reload_emoji` — Pleroma's `reload_emoji`:
/// re-scan the packs under `EMOJI_DIR` and resync `custom_emojis`
/// through the startup pass, answering the JSON string `"ok"`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write:emoji` gate; [`AppError::Internal`] for a packs
/// root that cannot be listed; [`AppError::Database`] from the resync and
/// the log write.
async fn reload_emoji(
    State(state): State<AppState>,
    admin: AdminUser,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let synced =
        emoji_packs::sync_custom_emojis(&state.db, &state.config.base_url, &state.config.emoji_dir)
            .await?;
    let _logged = moderation_log::reload_emoji(&state.db, &admin.0).await?;
    debug!(emojis = synced, "admin emoji packs reloaded");
    Ok(Json("ok"))
}

/// Whether the relay `actor_ap_id` follows this instance's actor back —
/// the flag both write answers carry beside the ledger's rows.
///
/// # Errors
///
/// [`AppError::Database`] from the instance-actor provisioning and the
/// follow read.
async fn followed_back(state: &AppState, actor_ap_id: &str) -> AppResult<bool> {
    let instance = ensure_instance_actor(&state.db, &state.config).await?;
    relay::is_followed_back(&state.db, instance.id, actor_ap_id).await
}

/// The failure shape of the relay writes: a refusal the service states
/// becomes the ticket's 400 with that message (Pleroma's 500 aside); a
/// server-side failure whose detail the client is never owed —
/// [`AppError::Database`], [`AppError::Internal`],
/// [`AppError::BadGateway`] — stands as its own 500/502.
fn client_refusal(err: AppError) -> AppError {
    if matches!(
        err,
        AppError::Database(_) | AppError::Internal(_) | AppError::BadGateway(_)
    ) {
        return err;
    }
    AppError::BadRequest(err.to_string())
}

/// [`client_refusal`] for the withdrawal, with the one substitution the
/// ticket pins: the service's 404 for an unknown relay is not "Record
/// not found" here but the 400 [`NOT_FOUND`].
fn unfollow_refusal(err: AppError) -> AppError {
    if matches!(err, AppError::NotFound) {
        return AppError::BadRequest(NOT_FOUND.to_owned());
    }
    client_refusal(err)
}

/// The path segment as the ledger keys it: trimmed and lowercased —
/// hostnames are case-insensitive, and the `instances` rows and the
/// mirrors' [`domain`](user::User::domain) columns hold the lowercase
/// spelling the fetch paths derived them from.
fn normalized_host(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Whether `host` names this instance's own domain: [`Config::domain`]
/// against the normalized segment, case-insensitively like the fold.
///
/// [`Config::domain`]: crate::config::Config::domain
const fn host_is_local(domain: &str, host: &str) -> bool {
    domain.eq_ignore_ascii_case(host)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::Config;

    /// The follow body reads the ticket's shape, an absent `relay_url`
    /// defaults to the empty text the service refuses, and the
    /// withdrawal's `force` accepts the JSON boolean and the form
    /// request's string alike.
    #[test]
    fn the_relay_bodies_read_the_pinned_shape() {
        let follow: RelayBody =
            serde_json::from_value(json!({"relay_url": "https://relay.example/relay"}))
                .expect("the ticket shape deserializes");
        assert_eq!(follow.relay_url, "https://relay.example/relay");

        let bare: RelayBody =
            serde_json::from_value(json!({})).expect("an empty body deserializes");
        assert_eq!(
            bare.relay_url, "",
            "an absent address is the empty refusal downstream"
        );

        let unfollow: RelayUnfollowBody = serde_json::from_value(json!({
            "relay_url": "relay.example",
            "force": true,
        }))
        .expect("the withdrawal shape deserializes");
        assert_eq!(unfollow.relay_url, "relay.example");
        assert_eq!(unfollow.force, Some(true), "a real boolean");

        let form: RelayUnfollowBody = serde_json::from_value(json!({"force": "false"}))
            .expect("the form spelling reads through flex");
        assert_eq!(form.force, Some(false), "the stringly-typed flag");
        let quiet: RelayUnfollowBody =
            serde_json::from_value(json!({"relay_url": "x"})).expect("an absent flag is none");
        assert_eq!(quiet.force, None, "absent means not forced");
    }

    /// A relay write's refusal becomes the ticket's 400 with the
    /// refusal's own message; a server-side failure stands as itself,
    /// and the withdrawal swaps the unknown relay's 404 for the pinned
    /// `Not found`.
    #[test]
    fn refusals_fold_into_the_pinned_400() {
        let bad = client_refusal(AppError::Unprocessable("no relay address".to_owned()));
        assert!(
            matches!(&bad, AppError::BadRequest(m) if m == "no relay address"),
            "the 422 refusal travels into the 400: {bad}"
        );
        let missing = client_refusal(AppError::NotFound);
        assert!(
            matches!(&missing, AppError::BadRequest(m) if m == "Record not found"),
            "a plain 404 keeps the service's message: {missing}"
        );
        let gone = client_refusal(AppError::Gone);
        assert!(
            matches!(&gone, AppError::BadRequest(m) if m == "Gone"),
            "and a 410's: {gone}"
        );

        let db = client_refusal(AppError::Database(sqlx::Error::PoolClosed));
        assert!(
            matches!(db, AppError::Database(_)),
            "a database failure stays a hidden-detail 500"
        );
        let upstream = client_refusal(AppError::BadGateway("secret".to_owned()));
        assert!(
            matches!(upstream, AppError::BadGateway(_)),
            "and an upstream one its 502"
        );

        let unknown = unfollow_refusal(AppError::NotFound);
        assert!(
            matches!(&unknown, AppError::BadRequest(m) if m == NOT_FOUND),
            "the unknown relay is the pinned 400 Not found: {unknown}"
        );
        let still = unfollow_refusal(AppError::Unprocessable("no relay address".to_owned()));
        assert!(
            matches!(&still, AppError::BadRequest(m) if m == "no relay address"),
            "every other refusal folds as before: {still}"
        );
    }

    /// The host fold trims and lowercases; the local gate fires on the
    /// instance's own domain in any spelling and on nothing else.
    #[test]
    fn the_domain_folds_and_the_local_gate_fires() {
        assert_eq!(normalized_host("  DeleteMe.Example "), "deleteme.example");
        assert_eq!(normalized_host("peer.example"), "peer.example");

        let config = Config::for_tests();
        assert_eq!(config.domain(), "test.example", "the fixture origin");
        assert!(
            host_is_local(config.domain(), "test.example"),
            "the exact domain is local"
        );
        assert!(
            host_is_local(config.domain(), "Test.Example"),
            "and the fold makes case irrelevant"
        );
        assert!(
            !host_is_local(config.domain(), "peer.example"),
            "a peer is not local"
        );
        assert!(
            !host_is_local(config.domain(), "test.example.evil"),
            "and a suffix is not the domain"
        );
    }
}
