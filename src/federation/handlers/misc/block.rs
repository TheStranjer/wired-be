//! The `Block` route and its `Undo`: [`handle_block`] mirrors a remote
//! account's block of a local user — the pair's row then stands as the
//! delivered activity's mirror — and [`handle_undo_block`] lifts exactly
//! the block its actor placed, the blocked account coming from the
//! embedded inner activity or from the stored block row.

use futures::future::BoxFuture;
use sqlx::PgPool;
use tracing::debug;

use crate::{
    error::AppResult,
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        json,
    },
    models::{
        block,
        user::{self, User},
    },
    state::AppState,
};

use super::{resolve_local_target, transient};

/// The `Block` route: mirror a remote account's block of a local user,
/// severing the pair's follows and recording the activity id.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// block from being written or its id recorded; anything wrong with the
/// *shape* of the activity is reported as [`Outcome::Ignored`].
pub fn handle_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_block(state, view, signer).await })
}

/// The `undo_by_type["Block"]` route: lift the block the signer placed.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// block from being read or removed; an unresolvable or unmatched `Undo`
/// is [`Outcome::Ignored`].
pub fn handle_undo_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_block(state, view, signer).await })
}

/// The body of [`handle_block`]: the local-signer guard, the local-target
/// resolution, the model's block-and-sever write, and the activity id that
/// makes the pair's row *this* activity's mirror.
async fn apply_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Block signed by an account this instance owns",
        ));
    }
    let target = resolve_local_target(&state.db, &view)
        .await
        .map_err(|err| transient("block target", &err))?;
    let Some(target) = target else {
        return Ok(Outcome::Ignored(
            "a Block whose object is not a stored local account",
        ));
    };
    let row = block::block(&state.db, &state.config.base_url, &signer, &target)
        .await
        .map_err(|err| transient("block", &err))?;
    if row.ap_id.as_deref() != Some(view.id.as_str()) {
        block::set_ap_id(&state.db, row.id, &view.id)
            .await
            .map_err(|err| transient("block activity id", &err))?;
    }
    debug!(
        blocker = %signer.nickname,
        blocked = %target.nickname,
        "an inbound Block mirrored a remote account's block of a local one"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_undo_block`]: resolve which account the withdrawn
/// block was about, then lift exactly the signer's own block of it.
async fn apply_undo_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Undo of a Block signed by an account this instance owns",
        ));
    }
    let target = undo_block_target(&state.db, &view)
        .await
        .map_err(|err| transient("undone block", &err))?;
    let Some(target) = target else {
        return Ok(Outcome::Ignored(
            "an Undo of a Block this instance cannot resolve to a blocked account",
        ));
    };
    let lifted = block::unblock(&state.db, &signer, &target)
        .await
        .map_err(|err| transient("block lift", &err))?;
    if lifted.is_none() {
        return Ok(Outcome::Ignored(
            "an Undo of a Block its actor never placed",
        ));
    }
    debug!(
        blocker = %signer.nickname,
        blocked = %target.nickname,
        "an inbound Undo lifted a remote account's block"
    );
    Ok(Outcome::Handled)
}

/// The blocked account an `Undo` of a `Block` is about: the embedded inner
/// activity states it in its own `object`, and an `Undo` naming the
/// activity id alone learns it from the stored block row that id keys —
/// which is also why only the blocker's own `Undo` can find its block.
async fn undo_block_target(pool: &PgPool, view: &ActivityView) -> AppResult<Option<User>> {
    if let Some(inner) = view.object.embedded() {
        let Some(id) = inner.get("object").and_then(json::value_or_id) else {
            return Ok(None);
        };
        return user::get_by_ap_id(pool, &id).await;
    }
    let Some(id) = view.object.id() else {
        return Ok(None);
    };
    match block::get_by_ap_id(pool, id).await? {
        Some(row) => user::get_by_id(pool, row.blocked_id).await,
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{federation::handlers::misc::test_support::*, models::follow};

    /// The block mirrored behind an activity id, expecting it to exist.
    async fn mirrored_block(pool: &PgPool, ap_id: &str) -> block::Block {
        block::get_by_ap_id(pool, ap_id)
            .await
            .expect("the block reads")
            .unwrap_or_else(|| panic!("{ap_id} mirrors a stored block"))
    }

    /// The block route: the pair's row lands with the delivered activity id
    /// as its own, the follows in both directions are severed with their
    /// counters, and the same activity delivered twice stands as one row
    /// with the same id.
    #[tokio::test]
    async fn a_block_of_a_local_account_mirrors_the_activity_and_severs_follows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "block-bob").await;
        accepted_follow(&pool, &bob, &alice).await;
        accepted_follow(&pool, &alice, &bob).await;

        let folded = handle_block(
            state.clone(),
            object_view("Block", "block-1", ALICE, json!(bob.ap_id.clone())),
            alice,
        )
        .await;
        assert_handled(&folded);

        let row = mirrored_block(&pool, "https://peer.example/activities/block-1").await;
        assert_eq!(row.blocker_id, alice_id, "the remote actor blocks");
        assert_eq!(row.blocked_id, bob.id, "the local account is blocked");
        assert!(
            block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
            "the fact reads back"
        );
        assert!(
            follow::get(&pool, bob.id, alice_id)
                .await
                .unwrap()
                .is_none()
                && follow::get(&pool, alice_id, bob.id)
                    .await
                    .unwrap()
                    .is_none(),
            "both directions' follows went with the block"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "and the counters stepped down"
        );

        let again = handle_block(
            state,
            object_view("Block", "block-1", ALICE, json!(bob.ap_id.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&again);
        assert_eq!(
            block::blocker_ids(&pool, bob.id).await.unwrap(),
            vec![alice_id],
            "the repeat found the same pair's row and wrote no second one"
        );
        assert_eq!(
            block::blocked_ids(&pool, alice_id).await.unwrap(),
            vec![bob.id],
            "and that one row is alice's block of bob"
        );
        assert_eq!(
            mirrored_block(&pool, "https://peer.example/activities/block-1")
                .await
                .id,
            row.id,
            "the row itself did not move"
        );
    }

    /// A `Block` whose object names a remote mirror, or no account at all,
    /// describes somebody else's business: ignored, and nothing blocked.
    #[tokio::test]
    async fn a_block_about_anything_but_a_local_account_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;

        let remote_target = handle_block(
            state.clone(),
            object_view("Block", "block-2", ALICE, json!(mallory.ap_id.clone())),
            alice,
        )
        .await;
        assert!(
            ignored(remote_target).contains("local account"),
            "a block of a remote account is not ours to mirror"
        );

        let unknown = handle_block(
            state.clone(),
            object_view(
                "Block",
                "block-3",
                ALICE,
                json!("https://test.example/users/nobody"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(unknown).contains("local account"),
            "and an object naming no stored account"
        );

        let idless = handle_block(
            state,
            object_view("Block", "block-6", ALICE, json!({"type": "Person"})),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(idless).contains("local account"),
            "and an embedded object that names no id at all"
        );
        assert!(
            block::blocked_ids(&pool, mallory.id)
                .await
                .unwrap()
                .is_empty()
                && sqlx::query_scalar::<_, i64>("SELECT count(*) FROM blocks")
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                    == 0,
            "neither delivery wrote a block"
        );
    }

    /// The `Undo` of a `Block`, both shapes: the bare activity id resolves
    /// the pair through the stored block row, the embedded inner activity
    /// through its own object. Both lift the block; withdrawing a gone
    /// block's id resolves to nothing and is ignored.
    #[tokio::test]
    async fn an_undo_by_the_same_actor_lifts_the_block_it_names() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "undo-bob").await;
        let carol = local_user(&pool, "undo-carol").await;
        let _bob_blocked = handle_block(
            state.clone(),
            object_view("Block", "block-bare", ALICE, json!(bob.ap_id.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        let _carol_blocked = handle_block(
            state.clone(),
            object_view("Block", "block-embed", ALICE, json!(carol.ap_id.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            block::is_blocking(&pool, alice_id, bob.id).await.unwrap()
                && block::is_blocking(&pool, alice_id, carol.id).await.unwrap(),
            "both fixture blocks stand"
        );

        let bare = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-bare-1",
                ALICE,
                json!("https://peer.example/activities/block-bare"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&bare);
        assert!(
            !block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
            "the bare-id Undo found the pair through the stored row"
        );

        let embedded = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-embed-1",
                ALICE,
                json!({
                    "id": "https://peer.example/activities/block-embed",
                    "type": "Block",
                    "actor": ALICE,
                    "object": carol.ap_id,
                }),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&embedded);
        assert!(
            !block::is_blocking(&pool, alice_id, carol.id).await.unwrap(),
            "and the embedded inner activity named the blocked account itself"
        );

        let twice = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-bare-2",
                ALICE,
                json!("https://peer.example/activities/block-bare"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(twice).contains("cannot resolve"),
            "a withdrawn block's id keys nothing any more"
        );
    }

    /// The unresolvable `Undo` shapes: an id that never stored a block and
    /// an embedded inner activity that names no blocked account both
    /// resolve to nothing and are ignored.
    #[tokio::test]
    async fn an_undo_that_resolves_to_no_blocked_account_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;

        let unknown = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-none-1",
                ALICE,
                json!("https://peer.example/activities/never-blocked"),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(unknown).contains("cannot resolve"),
            "an id no block carries resolves to nothing"
        );

        let innerless = handle_undo_block(
            state,
            object_view("Undo", "undo-none-2", ALICE, json!({"type": "Block"})),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(innerless).contains("cannot resolve"),
            "and an embedded Block that names no blocked account resolves to nothing"
        );
    }

    /// An `Undo` of a `Block` signed by an account that never placed it —
    /// with the inner activity embedded or the id named bare — lifts
    /// nothing: the lift is keyed to the pair, so another actor's block of
    /// the same local account stands untouched.
    #[tokio::test]
    async fn an_undo_by_a_different_actor_lifts_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "wrong-bob").await;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;
        let mallory_id = mallory.id;
        let _blocked = handle_block(
            state.clone(),
            object_view("Block", "block-4", ALICE, json!(bob.ap_id.clone())),
            alice,
        )
        .await;

        let embedded = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-2",
                MALLORY,
                json!({"type": "Block", "actor": ALICE, "object": bob.ap_id.clone()}),
            ),
            mallory,
        )
        .await;
        assert!(
            ignored(embedded).contains("never placed"),
            "mallory holds no block of bob to lift"
        );

        let bare = handle_undo_block(
            state,
            object_view(
                "Undo",
                "undo-3",
                MALLORY,
                json!("https://peer.example/activities/block-4"),
            ),
            fresh_user(&pool, mallory_id).await,
        )
        .await;
        assert!(
            ignored(bare).contains("never placed"),
            "and the row found through alice's activity id is not mallory's"
        );
        assert!(
            block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
            "alice's block of bob stands untouched through both"
        );
    }
}
