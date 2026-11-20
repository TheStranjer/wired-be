//! The `Move` route: [`handle_move`] mirrors an account's migration
//! announcement — the self-object and alias-claim validators around the
//! target fetch — and [`migrate_followers`] takes this instance's local
//! followers along to the new account.

use futures::future::BoxFuture;
use tracing::debug;

use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        fetch_actor::{FetchError, get_or_fetch_by_ap_id},
    },
    models::{
        block, follow,
        user::{self, User},
    },
    services::notify,
    state::AppState,
};

use super::transient;

/// The `Move` route: an account moved to a new actor; mirror the target
/// and migrate this instance's local followers.
///
/// # Errors
///
/// [`FederationError::Transient`] when the target fetch or any migration
/// write failed the way a retry can fix; a `Move` the validators refuse
/// — a mismatched object, an unfetchable target, a missing alias claim —
/// is [`Outcome::Ignored`].
pub fn handle_move(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_move(state, view, signer).await })
}

/// The body of [`handle_move`]: the two validators (self-object and alias
/// claim) around the target fetch, the mirror write, and the follower
/// migration.
async fn apply_move(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Move signed by an account this instance owns",
        ));
    }
    if view.object.id() != Some(view.actor.as_str()) {
        return Ok(Outcome::Ignored(
            "a Move whose object is not the actor's own id",
        ));
    }
    let Some(target_id) = view.target.as_deref() else {
        return Ok(Outcome::Ignored("a Move with no target actor"));
    };
    let target = match get_or_fetch_by_ap_id(&state, target_id).await {
        Ok(target) => target,
        Err(FetchError::NotFound | FetchError::Gone) => {
            debug!(target_id, "a Move to an actor that is not there; ignoring");
            return Ok(Outcome::Ignored(
                "a Move whose target actor cannot be resolved",
            ));
        }
        Err(err) => {
            return Err(FederationError::Transient(format!(
                "the move target could not be fetched: {err}"
            )));
        }
    };
    if !target.also_known_as.contains(&view.actor) {
        debug!(
            target = %target.nickname,
            "a Move to a target that has not claimed the old account; ignoring"
        );
        return Ok(Outcome::Ignored(
            "a Move whose target does not list the old account among its aliases",
        ));
    }
    user::set_moved_to(&state.db, signer.id, Some(&target.ap_id))
        .await
        .map_err(|err| transient("move target", &err))?;
    migrate_followers(&state, &signer, &target).await?;
    debug!(
        from = %signer.nickname,
        to = %target.nickname,
        "an inbound Move mirrored the account's new home and migrated its local followers"
    );
    Ok(Outcome::Handled)
}

/// The follower migration [`apply_move`] runs: every candidate from
/// [`follow::move_followers_of`] (local, accepted, still allowing its
/// following list to move) takes [`migrate_one_follower`] and then the
/// `move` telling ([`notify::on_move`]) — remote followers are untouched,
/// their instance re-follows on its own once the `Move` reaches it.
async fn migrate_followers(
    state: &AppState,
    origin: &User,
    target: &User,
) -> Result<(), FederationError> {
    let followers = follow::move_followers_of(&state.db, origin.id)
        .await
        .map_err(|err| transient("follower list", &err))?;
    for follower in &followers {
        migrate_one_follower(state, follower, origin, target).await?;
        notify::on_move(state, follower, origin).await;
    }
    Ok(())
}

/// The pair of writes one local follower takes from a move: follow the new
/// account, then unfollow the old — and neither for a pair that has
/// blocked the target, which keeps its relationship whole (the same
/// nothing-halfway rule the account-move endpoint's transfer states, see
/// [`crate::api::pleroma_account_settings`]).
async fn migrate_one_follower(
    state: &AppState,
    follower: &User,
    origin: &User,
    target: &User,
) -> Result<(), FederationError> {
    if block::blocked_between(&state.db, follower.id, target.id)
        .await
        .map_err(|err| transient("block check", &err))?
    {
        debug!(
            follower = %follower.nickname,
            "a blocked pair keeps both its follows through a move"
        );
        return Ok(());
    }
    let _opened = follow::follow(&state.db, &state.config.base_url, follower, target)
        .await
        .map_err(|err| transient("migrated follow", &err))?;
    let _lifted = follow::unfollow(&state.db, follower, origin)
        .await
        .map_err(|err| transient("migrated unfollow", &err))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        federation::handlers::misc::test_support::*,
        models::{
            follow::FollowState,
            user::{NewUser, insert},
        },
    };

    /// [`CAROL`] as a freshly fetched mirror claiming `aliases`: the state
    /// [`get_or_fetch_by_ap_id`] answers from the database, without the
    /// network.
    async fn move_target(pool: &PgPool, aliases: &[&str]) -> User {
        let stored = insert(
            pool,
            &NewUser {
                nickname: "carol@new.example".to_owned(),
                ap_id: CAROL.to_owned(),
                domain: Some("new.example".to_owned()),
                also_known_as: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
                ..NewUser::default()
            },
        )
        .await
        .expect("the target fixture inserts");
        let _stamped = sqlx::query("UPDATE users SET last_fetched_at = now() WHERE id = $1")
            .bind(stored.id)
            .execute(pool)
            .await
            .expect("the fixture stamps the fetch");
        stored
    }

    /// The `Move` fixture cast, ids and the stored target.
    #[derive(Debug)]
    struct MoveFixture {
        /// The departing remote account.
        alice: Uuid,
        /// The claimed new account: a fresh mirror listing alice.
        carol: User,
        /// The plain local follower, whose follow moves.
        bob: Uuid,
        /// The local follower blocked with `carol`.
        erin: Uuid,
        /// The remote follower nobody migrates from here.
        dave: Uuid,
    }

    /// The `Move` scenario: [`ALICE`] moving to [`CAROL`], accepted as a
    /// follow by the local `bob`, by the local `erin` — which blocks carol
    /// — and by the remote `dave`.
    async fn move_scenario(pool: &PgPool) -> MoveFixture {
        let alice = remote_signer(pool).await;
        let carol = move_target(pool, &[ALICE]).await;
        let bob = local_user(pool, "move-bob").await;
        let erin = local_user(pool, "move-erin").await;
        let dave = remote_at(pool, "dave", "https://other.example/users/dave").await;
        for follower in [&bob, &erin, &dave] {
            accepted_follow(pool, follower, &alice).await;
        }
        let _erin_blocked_carol = block::block(pool, BASE, &erin, &carol)
            .await
            .expect("the blocked-pair fixture stores");
        MoveFixture {
            alice: alice.id,
            carol,
            bob: bob.id,
            erin: erin.id,
            dave: dave.id,
        }
    }

    /// The `Move` activity named `n` both halves of the happy path deliver.
    fn move_activity(n: &str) -> ActivityView {
        view("Move", n, ALICE, json!({"object": ALICE, "target": CAROL}))
    }

    /// The recipient's stored notifications, oldest first, as
    /// `(type, from_account_id, status_id)` triples.
    async fn told(pool: &PgPool, recipient: Uuid) -> Vec<(String, Uuid, Option<Uuid>)> {
        sqlx::query_as::<_, (String, Uuid, Option<Uuid>)>(
            "SELECT type, from_account_id, status_id FROM notifications \
              WHERE user_id = $1 ORDER BY id",
        )
        .bind(recipient)
        .fetch_all(pool)
        .await
        .expect("the notifications read")
    }

    /// The `Move` happy path, first half: the target's alias claim is
    /// honoured, the old account's `moved_to_ap_id` mirrors the new one
    /// with no local-move cooldown stamp, and the unblocked local follower
    /// re-follows the target, drops the old account, and is told `move`
    /// with the old account as actor.
    #[tokio::test]
    async fn a_move_mirrors_the_target_and_migrates_the_local_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let scene = move_scenario(&pool).await;

        let moved = handle_move(
            state,
            move_activity("move-1"),
            fresh_user(&pool, scene.alice).await,
        )
        .await;
        assert_handled(&moved);

        let departed = fresh_user(&pool, scene.alice).await;
        assert_eq!(
            departed.moved_to_ap_id.as_deref(),
            Some(CAROL),
            "the old account points at the new one"
        );
        assert!(
            departed.last_move_at.is_none(),
            "and this instance stamps no move cooldown for a peer's move"
        );

        let bob_to_carol = follow::get(&pool, scene.bob, scene.carol.id)
            .await
            .unwrap()
            .expect("bob follows the new account now");
        assert_eq!(
            bob_to_carol.state,
            FollowState::Pending,
            "the re-follow of a remote account waits for its answer, like any other"
        );
        assert!(
            follow::get(&pool, scene.bob, scene.alice)
                .await
                .unwrap()
                .is_none(),
            "and no longer the old one"
        );
        assert_eq!(
            told(&pool, scene.bob).await,
            vec![("move".to_owned(), scene.alice, None)],
            "the migrated follower is told the move, from the old account"
        );
    }

    /// The `Move` happy path, second half: the follower blocked with the
    /// target keeps both its relationships (and is told anyway), the remote
    /// follower and the target itself hear nothing, and a redelivered
    /// `Move` — after its follow has already moved — writes and tells
    /// nothing new.
    #[tokio::test]
    async fn a_move_leaves_blocked_and_remote_followers_and_retells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let scene = move_scenario(&pool).await;

        let moved = handle_move(
            state.clone(),
            move_activity("move-1"),
            fresh_user(&pool, scene.alice).await,
        )
        .await;
        assert_handled(&moved);

        assert!(
            follow::get(&pool, scene.erin, scene.alice)
                .await
                .unwrap()
                .is_some(),
            "the follower blocked with the target keeps its old follow"
        );
        assert!(
            follow::get(&pool, scene.erin, scene.carol.id)
                .await
                .unwrap()
                .is_none(),
            "and gains no follow of the account it blocked"
        );
        assert_eq!(
            told(&pool, scene.erin).await,
            vec![("move".to_owned(), scene.alice, None)],
            "and is told anyway — the move happened whether or not its follow moved"
        );
        assert!(
            follow::get(&pool, scene.dave, scene.alice)
                .await
                .unwrap()
                .is_some(),
            "the remote follower's relationship is its own instance's to migrate"
        );
        assert!(
            told(&pool, scene.dave).await.is_empty()
                && told(&pool, scene.carol.id).await.is_empty(),
            "and nobody else is told from here"
        );

        let again = handle_move(
            state,
            move_activity("move-2"),
            fresh_user(&pool, scene.alice).await,
        )
        .await;
        assert_handled(&again);
        assert_eq!(
            told(&pool, scene.bob).await.len(),
            1,
            "the redelivered Move tells bob once"
        );
    }

    /// The alias validator: a target that has not claimed the old actor in
    /// its `alsoKnownAs` moves nothing — no `moved_to_ap_id`, no migrated
    /// follower, no telling.
    #[tokio::test]
    async fn a_move_the_target_has_not_claimed_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let _carol = move_target(&pool, &["https://other.example/users/someone"]).await;
        let bob = local_user(&pool, "alias-bob").await;
        accepted_follow(&pool, &bob, &alice).await;

        let refused = handle_move(
            state,
            view(
                "Move",
                "move-3",
                ALICE,
                json!({"object": ALICE, "target": CAROL}),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(refused).contains("aliases"),
            "the unclaimed origin answers for nothing"
        );
        assert!(
            fresh_user(&pool, alice_id).await.moved_to_ap_id.is_none(),
            "the account is not marked moved"
        );
        assert!(
            follow::get(&pool, bob.id, alice_id)
                .await
                .unwrap()
                .is_some()
                && told(&pool, bob.id).await.is_empty(),
            "and its follower keeps the relationship and hears nothing"
        );
    }

    /// The other refusals: an object that is not the actor's own id, no
    /// target at all, and a target this instance resolves to no actor — an
    /// unknown id on this instance's own origin, answered without the
    /// network — are all ignored.
    #[tokio::test]
    async fn a_malformed_or_unresolvable_move_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let _carol = move_target(&pool, &[ALICE]).await;

        let wrong_object = handle_move(
            state.clone(),
            view(
                "Move",
                "move-4",
                ALICE,
                json!({"object": MALLORY, "target": CAROL}),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(wrong_object).contains("not the actor's own id"),
            "a Move of somebody else's account is nobody's"
        );

        let no_target = handle_move(
            state.clone(),
            object_view("Move", "move-5", ALICE, json!(ALICE)),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(no_target).contains("no target"),
            "and a Move with no destination"
        );

        let gone_target = handle_move(
            state,
            view(
                "Move",
                "move-6",
                ALICE,
                json!({"object": ALICE, "target": "https://test.example/users/ghost"}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(gone_target).contains("cannot be resolved"),
            "a target with no actor behind it moves nobody"
        );
    }
}
