//! Inbound `Block` (+ its `Undo`), `Flag`, `Add`, `Remove` and `Move`: the
//! routes for activities about a relationship or an account's lifecycle,
//! not about a post or a profile document.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.SideEffects`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`) — its `Block` clause
//! calls `User.block`, its `Undo`-of-`Block` clause `User.unblock`, its
//! `Add`/`Remove` clauses `User.add_pinned_object_id` /
//! `User.remove_pinned_object_id` — together with the `Flag` and `Move`
//! clauses of `Transmogrifier.handle_incoming`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`) and the containment
//! checks of its `object_validators/`. The ticket rules are authoritative
//! where the two differ:
//!
//! * [`handle_block`] mirrors a remote account's block of a *local* user:
//!   [`block::block`](crate::models::block::block) writes the pair and severs the
//!   follows between them (its documented side effect), and
//!   [`block::set_ap_id`](crate::models::block::set_ap_id) records the
//!   delivered activity id on the row so the duplicate delivery stands as
//!   one fact and the `Undo` below can find it. An object naming no local
//!   account is somebody else's block — ignored.
//! * [`handle_undo_block`] is the `undo_by_type["Block"]` route: it lifts
//!   exactly the block *its actor placed* —
//!   [`block::unblock`](crate::models::block::unblock) is keyed on
//!   the pair, so an `Undo` signed by anyone else removes nothing and is
//!   ignored. The blocked account comes from the embedded inner activity
//!   when the `Undo` carries one, and from the stored block row when it
//!   names the activity id alone
//!   ([`block::get_by_ap_id`](crate::models::block::get_by_ap_id)).
//! * [`handle_flag`] files the forwarded report: the object is a list of
//!   ids whose local-account entry names the target (a `Flag` about a
//!   non-local account is ignored, per Pleroma's `get_reported` reduced to
//!   the local audience), the rest are cited status uris matched by
//!   [`status::get_by_uri`](crate::models::status::get_by_uri) when stored, and `content` is the
//!   comment. The row is written with [`report::create`](crate::models::report::create) as *forwarded* — a remote
//!   instance filing with ours is exactly that — and given the activity id
//!   through [`report::set_ap_id`](crate::models::report::set_ap_id), before the shared
//!   `after_report_created` hook ([`crate::api::reports`]) runs.
//! * [`handle_add`] and [`handle_remove`] fold the featured-collection
//!   pins: only a `target` equal to the actor's own `featured_url` means
//!   anything here (Pleroma's `Add`/`Remove` are that one collection for
//!   a remote actor), and only a stored status the actor authored can be
//!   pinned ([`pinned_status::pin`](crate::models::pinned_status::pin) /
//!   [`pinned_status::unpin`](crate::models::pinned_status::unpin), idempotent
//!   both ways). An unknown status is ignored — fetching it is the later
//!   inbox-fetch ticket's job.
//! * [`handle_move`] is the account-migration announcement: the object
//!   must be the actor's own id, the target is fetched
//!   ([`get_or_fetch_by_ap_id`](crate::federation::fetch_actor::get_or_fetch_by_ap_id)) and must have claimed the old actor in
//!   its `alsoKnownAs` (the validator Pleroma's `Move` clause runs), and
//!   the origin's `moved_to_ap_id` is mirrored through
//!   [`user::set_moved_to`] — no [`mark_moved`] cooldown stamp, because
//!   this instance performed nothing. Then every local follower Pleroma
//!   would migrate ([`follow::move_followers_of`](crate::models::follow::move_followers_of)) follows the new account
//!   and unfollows the old — skipped whole for a blocked pair — and is
//!   told about the move ([`notify::on_move`](crate::services::notify::on_move)).
//!
//! Every route runs behind the dispatcher's own gates: containment has
//! already proved the activity's `actor` to be the signer's `ap_id`
//! ([`crate::federation::dispatch`]), so the [`User`] a handler is handed
//! *is* the remote account the activity speaks for; the local-signer guard
//! each one keeps is the belt to that bracer.
//!
//! [`mark_moved`]: crate::models::user::mark_moved

use sqlx::PgPool;

use crate::{
    error::{AppError, AppResult},
    federation::dispatch::{ActivityView, FederationError, Handlers},
    models::user::{self, User},
};

mod block;
mod featured;
mod flag;
mod moves;
#[cfg(test)]
mod test_support;

pub use block::{handle_block, handle_undo_block};
pub use featured::{handle_add, handle_remove};
pub use flag::handle_flag;
pub use moves::handle_move;

/// Folds this module's routes into the dispatcher's registry.
///
/// The five types by their own names, plus the `Undo` of a `Block` in the
/// `Undo` table. `crate::federation::handlers::registry` calls it, which is
/// the whole composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register("Block", handle_block);
    handlers.register_undo("Block", handle_undo_block);
    handlers.register("Flag", handle_flag);
    handlers.register("Add", handle_add);
    handlers.register("Remove", handle_remove);
    handlers.register("Move", handle_move);
}

/// The local user the activity's object names: the object's own id (bare
/// or embedded) resolved against `users`, kept only when the account is
/// one this instance owns — the `Block` and `Flag` targets alike.
async fn resolve_local_target(pool: &PgPool, view: &ActivityView) -> AppResult<Option<User>> {
    let Some(id) = view.object.id() else {
        return Ok(None);
    };
    let found = user::get_by_ap_id(pool, id).await?;
    Ok(found.filter(|found| found.local.get()))
}

/// The first of `ids` that names a stored *local* account, with its
/// position so the status pass can skip it: Pleroma's `get_reported`
/// reduced to the local audience, since a report about another instance's
/// account is that instance's to file.
async fn find_local_account(pool: &PgPool, ids: &[String]) -> AppResult<Option<(usize, User)>> {
    for (index, id) in ids.iter().enumerate() {
        if let Some(found) = user::get_by_ap_id(pool, id).await?
            && found.local.get()
        {
            return Ok(Some((index, found)));
        }
    }
    Ok(None)
}

/// The retry arm every database failure in these handlers maps onto: the
/// writes here are idempotent, so the queue's next attempt can run the
/// same activity again.
fn transient(what: &str, err: &AppError) -> FederationError {
    FederationError::Transient(format!(
        "could not fold the remote activity's {what}: {err}"
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        extract::State,
        http::{StatusCode, header::CONTENT_TYPE},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::{Value, json};
    use sqlx::postgres::PgPoolOptions;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        federation::handlers::misc::test_support::*,
        http_client::HttpClient,
        models::{
            block, pinned_status,
            user::{NewUser, insert},
        },
        state::AppState,
    };

    /// What the loopback peer answers a request for the move target with.
    #[derive(Clone, Debug)]
    struct TargetPeer {
        /// The actor document served on a 200.
        document: Value,
        /// The status the peer answers with.
        status: StatusCode,
        /// The requests the peer has answered.
        hits: Arc<AtomicUsize>,
    }

    /// Counts the hit and answers the configured status, with the document
    /// on a 200.
    async fn serve_target(State(peer): State<TargetPeer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        if peer.status != StatusCode::OK {
            return peer.status.into_response();
        }
        (
            [(CONTENT_TYPE, "application/activity+json")],
            peer.document.to_string(),
        )
            .into_response()
    }

    /// The running loopback peer of one refetch test.
    #[derive(Debug)]
    struct Loopback {
        /// The target actor's id on the peer.
        target_id: String,
        /// The requests the peer has answered.
        hits: Arc<AtomicUsize>,
        /// The server task, aborted when the test ends.
        server: JoinHandle<()>,
    }

    /// Serves carol on a fresh loopback port: her document claims `aliases`
    /// and the peer answers `status`.
    async fn loopback_target(aliases: &[&str], status: StatusCode) -> Loopback {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port binds");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the bound address")
        );
        let target_id = format!("{base}/users/carol");
        let hits = Arc::new(AtomicUsize::new(0));
        let peer = TargetPeer {
            document: json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "type": "Person",
                "id": target_id,
                "preferredUsername": "carol",
                "name": "Carol",
                "inbox": format!("{target_id}/inbox"),
                "alsoKnownAs": aliases,
                "publicKey": {
                    "id": format!("{target_id}#main-key"),
                    "owner": target_id,
                    "publicKeyPem":
                        "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
                },
            }),
            status,
            hits: Arc::clone(&hits),
        };
        let app = Router::new()
            .route("/users/carol", get(serve_target))
            .with_state(peer);
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        });
        Loopback {
            target_id,
            hits,
            server,
        }
    }

    /// The target's stored mirror as a day-old-or-less cache holds it: no
    /// aliases yet, and a fetch stamp of now, so a plain lookup serves it
    /// without the network.
    async fn cached_target(pool: &PgPool, target_id: &str) -> User {
        let authority = target_id
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .expect("the loopback id is an http url");
        let stored = insert(
            pool,
            &NewUser {
                nickname: format!("carol@{authority}"),
                ap_id: target_id.to_owned(),
                domain: Some(authority.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the cached target inserts");
        let _stamped = sqlx::query("UPDATE users SET last_fetched_at = now() WHERE id = $1")
            .bind(stored.id)
            .execute(pool)
            .await
            .expect("the fixture stamps the fetch");
        stored
    }

    /// Test state whose client may reach the loopback peer.
    fn loopback_state(pool: &PgPool) -> AppState {
        let mut state = app_state(pool);
        state.http_client = HttpClient::new_for_tests("Wired (https://test.example)");
        state
    }

    /// A `Move` of [`ALICE`] to `target_id`.
    fn move_to(target_id: &str) -> ActivityView {
        view(
            "Move",
            "refetch-move",
            ALICE,
            json!({"object": ALICE, "target": target_id}),
        )
    }

    /// A cached target without the alias is refetched once; the peer's
    /// fresh document carries it, so the `Move` is folded.
    #[tokio::test]
    async fn a_move_whose_cached_target_lacks_the_alias_refetches_and_succeeds() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let peer = loopback_target(&[ALICE], StatusCode::OK).await;
        let _cached = cached_target(&pool, &peer.target_id).await;
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;

        let moved = handle_move(loopback_state(&pool), move_to(&peer.target_id), alice).await;

        assert_handled(&moved);
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            1,
            "refetched exactly once"
        );
        assert_eq!(
            fresh_user(&pool, alice_id).await.moved_to_ap_id.as_deref(),
            Some(peer.target_id.as_str()),
            "the old account points at the new one"
        );
        let stored = user::get_by_ap_id(&pool, &peer.target_id)
            .await
            .unwrap()
            .expect("the target stays mirrored");
        assert_eq!(stored.also_known_as, vec![ALICE.to_owned()], "alias stored");
        peer.server.abort();
    }

    /// A refetch that still does not list the origin is refused, after one
    /// refetch and no more.
    #[tokio::test]
    async fn a_move_whose_refetched_target_still_lacks_the_alias_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let peer = loopback_target(&[MALLORY], StatusCode::OK).await;
        let _cached = cached_target(&pool, &peer.target_id).await;
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;

        let refused = handle_move(loopback_state(&pool), move_to(&peer.target_id), alice).await;

        assert!(ignored(refused).contains("aliases"), "no claim, no move");
        assert_eq!(
            peer.hits.load(Ordering::SeqCst),
            1,
            "refetched exactly once"
        );
        assert!(
            fresh_user(&pool, alice_id).await.moved_to_ap_id.is_none(),
            "the account is not marked moved"
        );
        peer.server.abort();
    }

    /// A target the peer reports gone on the refetch is ignored.
    #[tokio::test]
    async fn a_move_whose_target_refetch_is_gone_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let peer = loopback_target(&[ALICE], StatusCode::GONE).await;
        let _cached = cached_target(&pool, &peer.target_id).await;
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;

        let refused = handle_move(loopback_state(&pool), move_to(&peer.target_id), alice).await;

        assert!(
            ignored(refused).contains("cannot be resolved"),
            "a gone target moves nobody"
        );
        assert_eq!(peer.hits.load(Ordering::SeqCst), 1, "the refetch was tried");
        assert!(
            fresh_user(&pool, alice_id).await.moved_to_ap_id.is_none(),
            "the account is not marked moved"
        );
        peer.server.abort();
    }

    /// A target that is not found on the refetch is ignored too.
    #[tokio::test]
    async fn a_move_whose_target_refetch_is_not_found_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let peer = loopback_target(&[ALICE], StatusCode::NOT_FOUND).await;
        let _cached = cached_target(&pool, &peer.target_id).await;
        let alice = remote_signer(&pool).await;

        let refused = handle_move(loopback_state(&pool), move_to(&peer.target_id), alice).await;

        assert!(ignored(refused).contains("cannot be resolved"));
        peer.server.abort();
    }

    /// Any other refetch failure is the queue's to retry.
    #[tokio::test]
    async fn a_move_whose_target_refetch_fails_is_retried() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let peer = loopback_target(&[ALICE], StatusCode::INTERNAL_SERVER_ERROR).await;
        let _cached = cached_target(&pool, &peer.target_id).await;
        let alice = remote_signer(&pool).await;

        let failed = handle_move(loopback_state(&pool), move_to(&peer.target_id), alice).await;

        assert!(
            matches!(failed, Err(FederationError::Transient(_))),
            "a failing peer is a retry, not a verdict: {failed:?}"
        );
        peer.server.abort();
    }

    /// Test state over a pool that cannot answer: the database having gone
    /// away, as the handlers see it.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
    }

    /// The local-signer guard on every route: an account this instance owns
    /// does not file inbound federation against itself — each handler
    /// ignores the same activity it would fold for a remote one, and
    /// nothing is written.
    #[tokio::test]
    async fn a_local_signer_folds_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let ours = local_user(&pool, "ours").await;
        let ours_id = ours.id;
        let bob = local_user(&pool, "guarded-bob").await;
        let posted = local_post(&pool, &ours, "mine").await;
        let who = ours.ap_id.clone();
        let featured = format!("{who}/collections/featured");

        let outcomes = [
            handle_block(
                state.clone(),
                object_view("Block", "guard-1", &who, json!(bob.ap_id.clone())),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_undo_block(
                state.clone(),
                object_view("Undo", "guard-2", &who, json!(bob.ap_id.clone())),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_flag(
                state.clone(),
                object_view("Flag", "guard-3", &who, json!([bob.ap_id.clone()])),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_add(
                state.clone(),
                view(
                    "Add",
                    "guard-4",
                    &who,
                    json!({"object": posted.uri.clone(), "target": featured.clone()}),
                ),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_remove(
                state.clone(),
                view(
                    "Remove",
                    "guard-5",
                    &who,
                    json!({"object": posted.uri.clone(), "target": featured.clone()}),
                ),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_move(
                state,
                view(
                    "Move",
                    "guard-6",
                    &who,
                    json!({"object": who.clone(), "target": CAROL}),
                ),
                fresh_user(&pool, ours_id).await,
            )
            .await,
        ];
        for (label, outcome) in ["Block", "Undo", "Flag", "Add", "Remove", "Move"]
            .into_iter()
            .zip(outcomes)
        {
            assert!(
                ignored(outcome).contains("this instance owns"),
                "a {label} by a local account is the client API's business"
            );
        }
        assert!(
            block::blocked_ids(&pool, ours_id).await.unwrap().is_empty()
                && stored_reports(&pool).await.is_empty()
                && fresh_user(&pool, ours_id).await.moved_to_ap_id.is_none()
                && pinned_status::count(&pool, ours_id).await.unwrap() == 0,
            "and none of the six touched anything"
        );
    }

    /// Every route against a database that cannot answer: the reads fail
    /// first, the failure is transient, and the queue's retry finds the
    /// same idempotent writes waiting for it.
    #[tokio::test]
    async fn a_dead_database_sends_every_route_back_for_a_retry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "dead-bob").await;
        let state = broken_state().await;

        let outcomes = [
            handle_block(
                state.clone(),
                object_view("Block", "dead-1", ALICE, json!(bob.ap_id.clone())),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_undo_block(
                state.clone(),
                object_view(
                    "Undo",
                    "dead-2",
                    ALICE,
                    json!("https://peer.example/activities/dead-block"),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_flag(
                state.clone(),
                object_view("Flag", "dead-3", ALICE, json!([bob.ap_id.clone()])),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_add(
                state.clone(),
                view(
                    "Add",
                    "dead-4",
                    ALICE,
                    json!({"object": "https://peer.example/objects/any", "target": FEATURED}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_move(
                state,
                view(
                    "Move",
                    "dead-5",
                    ALICE,
                    json!({"object": ALICE, "target": CAROL}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await,
        ];
        for outcome in outcomes {
            assert!(
                matches!(outcome, Err(FederationError::Transient(_))),
                "the dead database is a retry, not a verdict: {outcome:?}"
            );
        }
    }
}
