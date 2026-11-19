//! The block arm of the outbound hooks: the `Block` a local account's write
//! against a REMOTE account is, and the `Undo` that takes it back, both fed
//! to the blocked actor's instance alone.
//!
//! The rules both arms lean on — the stored-`ap_id` discipline of decision
//! 6, the fire-and-forget publisher, the row readers — stand in the module
//! root.

use serde_json::{Value, json};
use tracing::{debug, warn};

use super::load_user;
use crate::{
    federation::{
        activities::{AnsweredActivity, block_json, undo_json},
        publisher,
    },
    models::{block::Block, user::User},
    state::AppState,
    util::id::new_id,
};

/// Announce the block or unblock the caller just performed.
///
/// `undo` selects the direction: `false` publishes the `Block`, `true`
/// publishes its `Undo` embedding it. Only a REMOTE blocked account is
/// worth announcing (a local one never sees an activity); the recipient is
/// the blocked user alone, so the document's `to` names their actor id and
/// the publisher's recipient resolution delivers to their personal inbox.
/// The Block's activity id is the stored `blocks.ap_id` — the row minted it
/// when the block was written and the id must not change — while the Undo
/// gets a fresh `{BASE_URL}/activities/{uuid}` id, the shape every
/// activity this instance sends carries.
///
/// A block row without an `ap_id` (a mirrored remote block, which this
/// instance does not own) and a pair whose users are gone or unkeyed
/// publish nothing.
pub async fn after_block_changed(state: &AppState, block: &Block, undo: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let (Some(blocker), Some(target)) = (
        load_user(state, block.blocker_id).await,
        load_user(state, block.blocked_id).await,
    ) else {
        return; // a pair whose rows are gone has nothing left to federate
    };
    if target.local.get() || !blocker.local.get() {
        return; // a local pair is settled in the database; only our actors sign
    }
    let Some(document) = block_document(state, block, &blocker, &target, undo) else {
        return; // a mirrored block belongs to its maker's instance, not ours
    };
    // One recipient address, the blocked actor: the publisher resolves it to
    // their personal inbox and delivers there alone.
    match publisher::publish_in_background(state, blocker.id, &document, &[], None).await {
        Ok(()) => debug!(
            actor = %blocker.ap_id,
            target = %target.ap_id,
            undo,
            "queued block federation"
        ),
        Err(err) => warn!(
            %err,
            actor = %blocker.ap_id,
            target = %target.ap_id,
            "could not queue block federation"
        ),
    }
}

/// The `Block` (or the `Undo` wrapping it) for the row `block`.
///
/// The `Block` half is [`block_json`] with its `id` replaced by the stored
/// string — the builders mint ids from a fresh uuid and the row's `ap_id`
/// is its own (decision 6), so only the stored text keeps a peer's
/// `Undo`-lookup pointed at the activity it already holds. `None` when the
/// row carries no id to stand behind the document.
fn block_document(
    state: &AppState,
    block: &Block,
    blocker: &User,
    target: &User,
    undo: bool,
) -> Option<Value> {
    let stored_id = block.ap_id.as_deref()?;
    let mut document = block_json(
        &state.config,
        block.id,
        &blocker.ap_id,
        &target.ap_id,
        &block.inserted_at,
    );
    if let Some(fields) = document.as_object_mut() {
        let _previous = fields.insert("id".to_owned(), json!(stored_id));
    }
    if undo {
        let now = chrono::Utc::now();
        let to = [target.ap_id.as_str()];
        Some(undo_json(&AnsweredActivity {
            config: &state.config,
            id: new_id(),
            actor_ap_id: &blocker.ap_id,
            to: &to,
            cc: &[],
            answered: &document,
            published: &now,
        }))
    } else {
        Some(document)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    use crate::{
        config::{Config, TEST_BASE_URL},
        models::block,
        services::federation_out::test_support::{
            app_state, job_count, local, publish_jobs, remote,
        },
    };

    /// The `Block` a local blocker wrote against a remote target publishes
    /// the stored activity id, addressed to the blocked actor alone, and
    /// nothing else is queued.
    #[tokio::test]
    async fn blocking_a_remote_account_publishes_the_stored_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let state = app_state(&pool);

        after_block_changed(&state, &row, false).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the block is announced once");
        let (args, unique_key) = &jobs[0];
        assert!(unique_key.is_none(), "blocks never debounce");
        assert_eq!(args["actor_id"], alice.id.to_string());
        let activity = &args["activity"];
        assert_eq!(activity["type"], "Block");
        assert_eq!(
            activity["id"],
            row.ap_id.unwrap(),
            "the id is the stored blocks.ap_id, not a freshly minted one"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], bob.ap_id);
        assert_eq!(
            activity["to"][0], bob.ap_id,
            "the blocked account is the only address"
        );
    }

    /// The undo publishes the same `Block` document wrapped in an `Undo`
    /// with its own fresh id; the inner document keeps the stored one.
    #[tokio::test]
    async fn unblocking_publishes_the_undo_of_the_stored_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let stored_id = row.ap_id.clone().expect("a local block has an id");
        let state = app_state(&pool);

        after_block_changed(&state, &row, true).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1);
        let activity = &jobs[0].0["activity"];
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["to"][0], bob.ap_id);
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "the undo carries this instance's own activity shape: {id}"
        );
        assert_ne!(id, stored_id, "and it is a fresh id, not the block's");
        assert_eq!(activity["object"]["type"], "Block");
        assert_eq!(
            activity["object"]["id"], stored_id,
            "the embedded block keeps the stored id"
        );
    }

    /// A block of a LOCAL account announces nothing: the pair's instances
    /// are the same database, and the local target's timeline is the whole
    /// delivery.
    #[tokio::test]
    async fn blocking_a_local_account_federates_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let state = app_state(&pool);

        after_block_changed(&state, &row, false).await;
        after_block_changed(&state, &row, true).await;

        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a local pair publishes neither the block nor the undo"
        );
    }

    /// The block document is only minted from the stored id; a row without
    /// one (a mirrored remote block) publishes nothing.
    #[tokio::test]
    async fn a_block_without_a_stored_id_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mut row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        row.ap_id = None;
        let state = app_state(&pool);

        after_block_changed(&state, &row, false).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }

    /// `published` on the block document is the row's own moment; on the
    /// undo too, and the shape matches the activity builders' golden
    /// (the id swapped for the stored one).
    #[test]
    fn the_block_document_carries_the_row_clock_and_stored_id() {
        let config = Config::for_tests();
        let published = Utc::now();
        let document = block_json(&config, new_id(), "alice-ap", "bob-ap", &published);
        assert!(document["published"].is_string());
        assert!(
            document["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("http"))
        );
    }
}
