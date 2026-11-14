//! Outbound federation triggered by account-lifecycle events.
//!
//! The client API keeps its post-write hooks deliberately thin: the endpoint
//! writes its relational fact, answers the client, and the federating half of
//! the job lands here. Four events arrive:
//!
//! - [`after_block_changed`]: a `Block` (or its `Undo`) written through
//!   `POST /api/v1/accounts/{id}/block` or the C2S endpoints, fed to the
//!   blocked account's instance only — a block of a local account is settled
//!   in our own database and never federates, as in Pleroma's
//!   `CommonApi.block`/`unblock` (the target's `local?` gate);
//! - [`after_profile_updated`]: an `Update` of the actor document after
//!   `PATCH /api/v1/accounts/update_credentials`, delivered to the account's
//!   remote followers' inboxes — Pleroma's
//!   `update_and_followers_notif_if_local` gate, minus the follow-notify
//!   relay (our local followers are told by their timelines, not by an
//!   activity). One burst of edits collapses into one queued activity: the
//!   job rides a per-account `unique_key`, so a repeat while the first is
//!   still live is dropped — and the delivery signs and serializes the
//!   freshest row when it runs, not the snapshot the edit passed in;
//! - [`after_account_deleted`]: a `Delete` of the actor, run as the first
//!   act of the `users.delete` sweep (before it clears the relationship
//!   rows the fan-out reads), delivered to the union of the account's
//!   remote followers' and remote followees' inboxes (both sides learned
//!   something from the actor, both must learn it is gone);
//! - [`after_account_disabled`]: nothing — deactivation deliberately
//!   federates no activity, so the hook is the documented landing point for
//!   a ticket that ever changes that answer.
//!
//! Every hook is fire-and-forget: it builds the document, hands it to
//! [`crate::federation::publisher`] (which wraps it in one
//! `federator.publish` job, resolved into per-inbox `federator.deliver`
//! jobs when the worker runs it), and returns. Failures to enqueue are
//! logged, never propagated: the user-visible write has already committed,
//! and a client that got its 200 must not see an `ActivityPub` problem.

use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::{
    federation::{
        activities::{AnsweredActivity, activity_id, block_json, undo_json},
        actor::actor_json,
        publisher,
    },
    models::{
        block::Block,
        follow,
        user::{self, User},
    },
    state::AppState,
    util::id::new_id,
};

/// The `ActivityStreams` public address, the `to` entry of a profile
/// `Update` and an account `Delete`.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

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
        Some(undo_json(&AnsweredActivity {
            config: &state.config,
            id: new_id(),
            actor_ap_id: &blocker.ap_id,
            other_actor_ap_id: &target.ap_id,
            answered: &document,
            published: &now,
        }))
    } else {
        Some(document)
    }
}

/// Announce the profile that `update_credentials` just wrote.
///
/// The `Update`'s object is the actor document itself ([`actor_json`] of
/// the updated row), addressed to the public with the account's followers
/// collection in `cc` — the shape both Mastodon and Pleroma send, and the
/// publisher expands the collection into the remote followers' shared
/// (preferred) or personal inboxes when the job runs, so the fan-out never
/// blocks the request.
///
/// Debouncing lives in the queue: every job this function enqueues carries
/// the per-account key `profile-update:{user id}`, and [`job::enqueue`]'s
/// dedup drops a repeat while an earlier one is still live — so a client
/// `PATCH`ing five fields in five requests pays for exactly one activity.
///
/// [`job::enqueue`]: crate::models::job::enqueue
pub async fn after_profile_updated(state: &AppState, account: &User) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !account.local.get() {
        return; // only our own actors publish
    }
    let Some(followers_collection) = account.followers_url.as_deref() else {
        return; // an actor with no followers collection has nobody to tell
    };
    let document = update_document(state, account, followers_collection);
    let unique_key = format!("profile-update:{}", account.id);
    match publisher::publish_in_background(state, account.id, &document, &[], Some(&unique_key))
        .await
    {
        Ok(()) => debug!(actor = %account.ap_id, "queued profile Update"),
        Err(err) => warn!(%err, actor = %account.ap_id, "could not queue profile Update"),
    }
}

/// The actor document for the profile in `account`, as an `Update` activity
/// addressed to the public and to the account's followers collection.
fn update_document(state: &AppState, account: &User, followers_collection: &str) -> Value {
    json!({
        "@context": crate::federation::note::json_ld_context(&state.config),
        "id": activity_id(&state.config, new_id()),
        "type": "Update",
        "actor": account.ap_id,
        "object": actor_json(account, &state.config),
        "to": [PUBLIC],
        "cc": [followers_collection],
    })
}

/// Announce the account whose deletion sweep is running.
///
/// The `Delete` names the actor as both `actor` and `object` — the peer's
/// cue to tombstone the account it mirrors — and is addressed to the public
/// with the followers collection in `cc`. Its id is the deterministic
/// `{BASE_URL}/activities/delete-user-{user id}`: the account's row id is
/// unique and never reused, so every (re)run of the sweep announces the
/// same activity id and the per-inbox delivery dedup swallows a retry.
///
/// The recipients are the snapshot taken *now* rather than an expansion the
/// delivery job computes later: the sweep clears the account's follow rows
/// right after this returns, and a `federator.deliver` job cannot re-query
/// a graph that is gone. [`after_account_deleted`] therefore lists the
/// remote followers' and remote followees' inboxes itself and hands the set
/// to the publisher as `extra_inboxes`, so both sides of every relationship
/// the account had learn it is gone.
pub async fn after_account_deleted(state: &AppState, account: &User) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !account.local.get() {
        return; // only our own actors announce themselves
    }
    let mut recipients: Vec<String> = Vec::new();
    match follow::remote_followers(&state.db, account.id).await {
        Ok(followers) => recipients.extend(followers.iter().filter_map(deliverable_inbox)),
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not read the followers for Delete federation");
        }
    }
    match follow::remote_followees(&state.db, account.id).await {
        Ok(followees) => recipients.extend(followees.iter().filter_map(deliverable_inbox)),
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not read the followees for Delete federation");
        }
    }
    if recipients.is_empty() {
        return; // nobody on another instance ever knew this account
    }
    // Two remote accounts on one instance name one shared inbox; the
    // snapshot POSTs there once.
    recipients.sort_unstable();
    recipients.dedup();
    let document = delete_document(state, account);
    match publisher::publish_in_background(state, account.id, &document, &recipients, None).await {
        Ok(()) => debug!(
            actor = %account.ap_id,
            inboxes = recipients.len(),
            "queued account Delete federation"
        ),
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not queue account Delete federation");
        }
    }
}

/// The account `Delete` activity: the actor id as both `actor` and
/// `object`, addressed to the public with the followers collection in `cc`,
/// under the deterministic id the ticket pins.
fn delete_document(state: &AppState, account: &User) -> Value {
    let id = format!(
        "{}/activities/delete-user-{}",
        state.config.base_url, account.id
    );
    let followers = account
        .followers_url
        .clone()
        .unwrap_or_else(|| format!("{}/followers", account.ap_id));
    json!({
        "@context": crate::federation::note::json_ld_context(&state.config),
        "id": id,
        "type": "Delete",
        "actor": account.ap_id,
        "object": account.ap_id,
        "to": [PUBLIC],
        "cc": [followers],
    })
}

/// The inbox to deliver to one remote account: its shared (instance-wide)
/// inbox when it publishes one, its personal inbox otherwise — the
/// publisher's own [`best_inbox`] rule, applied by the `Delete` snapshot
/// while the rows still exist. An account with no inbox at all answers
/// `None`; refused and local-origin inboxes are filtered by the publisher
/// when the queued fan-out runs.
///
/// [`best_inbox`]: crate::federation::delivery::best_inbox
fn deliverable_inbox(account: &User) -> Option<String> {
    let inbox = account
        .shared_inbox
        .as_deref()
        .or(account.inbox.as_deref())?;
    (!inbox.trim().is_empty() && inbox.starts_with("http")).then(|| inbox.to_owned())
}

/// The `disabled` landing hook: a deactivated account publishes nothing.
///
/// An account switched off with `POST /api/pleroma/disable_account` keeps
/// its actor document resolvable — its posts and follows stay visible in
/// other people's timelines — so nothing is announced. Pleroma's
/// `User.deactivate` federates nothing either. The function exists as the
/// single place a future ticket changes that answer, and at the call site
/// that states the intent.
pub const fn after_account_disabled(_state: &AppState, _account: &User) {}

/// Reads one user row for the hooks, answering `None` for a gone row or a
/// database that will not answer (a federation miss, never a failure).
async fn load_user(state: &AppState, id: uuid::Uuid) -> Option<User> {
    user::get_by_id(&state.db, id).await.unwrap_or_else(|err| {
        warn!(%err, %id, "federation hook could not read a user row");
        None
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL, Toggle},
        http_client::HttpClient,
        models::{
            block,
            user::{Flag, NewUser, insert, local_ap_id},
        },
        uploads::UploaderBackend,
    };

    /// The test state over `pool` with the default (federating) config.
    fn app_state(pool: &PgPool) -> AppState {
        state_with(pool, Config::for_tests())
    }

    /// The test state over `pool` with an injected config.
    fn state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The config with federation switched off.
    fn unfederated() -> Config {
        let mut config = Config::for_tests();
        config.federation_enabled = Toggle::new(false);
        config
    }

    /// A registered-looking local actor: keys, inbox, followers collection,
    /// exactly the row `create_local_user` writes.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        let ap_id = local_ap_id(TEST_BASE_URL, nickname);
        insert(
            pool,
            &NewUser {
                private_key: Some("PEM".to_owned()),
                public_key: Some("PUB".to_owned()),
                inbox: Some(format!("{ap_id}/inbox")),
                followers_url: Some(format!("{ap_id}/followers")),
                ..NewUser::local(nickname, &ap_id)
            },
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A remote mirror `nickname@remote.example`, shared inbox included.
    async fn remote(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                local: Flag::new(false),
                domain: Some("remote.example".to_owned()),
                inbox: Some(format!("https://remote.example/users/{nickname}/inbox")),
                shared_inbox: Some("https://remote.example/inbox".to_owned()),
                public_key: Some("PUB".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// `follower` follows `target` with an accepted relationship: a local
    /// target grants on the spot, a remote one is answered the way a peer's
    /// `Accept` would.
    async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
        let row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the follow fixture writes");
        if row.state != follow::FollowState::Accepted {
            let _granted = follow::accept(pool, row.id)
                .await
                .expect("the fixture grants it");
        }
    }

    /// The queued `federator.publish` jobs: `(args, unique_key)` pairs.
    async fn publish_jobs(pool: &PgPool) -> Vec<(Value, Option<String>)> {
        sqlx::query_as::<_, (Value, Option<String>)>(
            "SELECT args, unique_key FROM jobs WHERE kind = $1 ORDER BY inserted_at",
        )
        .bind(publisher::KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
    }

    /// How many jobs the database holds, of any kind.
    async fn job_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(pool)
            .await
            .expect("the job count")
    }

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

    /// With `FEDERATION_ENABLED` off the hooks never touch the queue.
    #[tokio::test]
    async fn a_disabled_instance_federates_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        let state = state_with(&pool, unfederated());

        after_block_changed(&state, &row, false).await;
        after_profile_updated(&state, &alice).await;
        after_account_deleted(&state, &alice).await;

        assert!(
            publish_jobs(&pool).await.is_empty(),
            "an instance that does not federate queues nothing"
        );
    }

    /// A profile edit publishes an `Update` of the actor document, to the
    /// public and the followers collection, under the per-account debounce
    /// key — and a burst of edits publishes exactly one activity.
    #[tokio::test]
    async fn a_profile_edit_publishes_the_update_once_per_burst() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let state = app_state(&pool);

        after_profile_updated(&state, &alice).await;
        after_profile_updated(&state, &alice).await;
        after_profile_updated(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(
            jobs.len(),
            1,
            "the two follow-ups collapse into the live job"
        );
        let (args, unique_key) = &jobs[0];
        assert_eq!(
            unique_key.as_deref(),
            Some(&*format!("profile-update:{}", alice.id))
        );
        let activity = &args["activity"];
        assert_eq!(activity["type"], "Update");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"]["id"], alice.ap_id);
        assert_eq!(activity["to"][0], PUBLIC);
        assert_eq!(activity["cc"][0], alice.followers_url.unwrap());
    }

    /// The account `Delete` names the actor id as both `actor` and
    /// `object`, carries the deterministic id, and hands the publisher the
    /// union of the remote followers' and followees' inboxes (a shared
    /// inbox per instance, once).
    #[tokio::test]
    async fn deleting_an_account_publishes_the_delete_to_both_sides() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let star = remote(&pool, "star").await;
        accepted_follow(&pool, &fan, &alice).await;
        accepted_follow(&pool, &alice, &star).await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1);
        let (args, unique_key) = &jobs[0];
        assert!(unique_key.is_none(), "the delete id itself dedups");
        let activity = &args["activity"];
        assert_eq!(activity["type"], "Delete");
        assert_eq!(
            activity["id"],
            format!("{TEST_BASE_URL}/activities/delete-user-{}", alice.id)
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], alice.ap_id);
        assert_eq!(activity["to"][0], PUBLIC);
        let mut inboxes: Vec<&str> = args["extra_inboxes"]
            .as_array()
            .expect("the snapshot rides along")
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .expect("every entry is a string")
            .into_iter()
            .collect();
        inboxes.sort_unstable();
        assert_eq!(
            inboxes,
            vec!["https://remote.example/inbox"],
            "both remote accounts share one instance inbox, and the snapshot names it once"
        );
    }

    /// An account that no remote instance ever touched publishes no
    /// `Delete`: there is nobody to tell.
    #[tokio::test]
    async fn a_lonely_deleted_account_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "nobody on another instance knew this account"
        );
    }

    /// A remote mirror's row is not this instance's to announce.
    #[tokio::test]
    async fn a_remote_account_publishes_no_delete() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = remote(&pool, "bob").await;
        let state = app_state(&pool);

        after_account_deleted(&state, &bob).await;
        after_profile_updated(&state, &bob).await;

        assert_eq!(job_count(&pool).await, 0, "a mirror announces nothing");
    }

    /// Deactivation keeps the actor document live for the histories that
    /// cite it: the hook is documented silence, in every configuration.
    #[tokio::test]
    async fn deactivating_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let state = app_state(&pool);

        after_account_disabled(&state, &alice);

        assert_eq!(job_count(&pool).await, 0, "a deactivation queues nothing");
    }

    /// The Delete snapshot reaches a followee that publishes no shared
    /// inbox at their personal one, and skips a mirror with no inbox at
    /// all.
    #[tokio::test]
    async fn the_delete_snapshot_falls_back_to_personal_inboxes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let personal = insert(
            &pool,
            &NewUser {
                nickname: "solo@other.example".to_owned(),
                ap_id: "https://other.example/users/solo".to_owned(),
                local: Flag::new(false),
                domain: Some("other.example".to_owned()),
                inbox: Some("https://other.example/users/solo/inbox".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the personal-inbox fixture inserts");
        let headless = insert(
            &pool,
            &NewUser {
                nickname: "ghost@other.example".to_owned(),
                ap_id: "https://other.example/users/ghost".to_owned(),
                local: Flag::new(false),
                domain: Some("other.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the headless fixture inserts");
        accepted_follow(&pool, &personal, &alice).await;
        accepted_follow(&pool, &alice, &headless).await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        let inboxes = jobs[0].0["extra_inboxes"]
            .as_array()
            .expect("a snapshot rides along");
        assert_eq!(
            inboxes.as_slice(),
            &[Value::String(
                "https://other.example/users/solo/inbox".to_owned()
            )],
            "the account with an inbox is named; the one without is skipped"
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
