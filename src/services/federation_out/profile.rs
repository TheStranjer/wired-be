//! The actor-document arms: the profile `Update` an edited account sends to
//! its remote followers, the account `Delete` its deletion sweep sends to
//! both sides of every relationship it held, and the inbox snapshot the
//! deletion and the suspension `Update` share.
//!
//! The pair that announces an account's move is [`super::moves`]' arm, which
//! finishes by running the profile `Update` documented here.

use std::collections::HashSet;

use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::{
    error::AppResult,
    federation::{
        activities::activity_id,
        actor::{actor_json, profile_emojis},
        delivery::best_inboxes,
        note, publisher,
        vocab::AS_PUBLIC,
    },
    models::{
        activitypub_actor::{self, ActivityPubActor},
        custom_emoji::CustomEmoji,
        follow, relay,
        user::{self, User},
    },
    state::AppState,
    util::id::new_id,
};

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
/// Its one non-test caller is `ActivityPub`'s adapter, which runs it on
/// [`AccountEvent::ProfileChanged`]; core code fires that event instead of calling this.
///
/// [`job::enqueue`]: crate::models::job::enqueue
/// [`AccountEvent::ProfileChanged`]: crate::protocols::events::AccountEvent::ProfileChanged
pub async fn after_profile_updated(state: &AppState, account: &User) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !account.local.get() {
        return; // only our own actors publish
    }
    let row = match activitypub_actor::get(&state.db, account.id).await {
        Ok(row) => row,
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not load the actor row for the Update");
            return;
        }
    };
    let Some(followers_collection) = row.as_ref().and_then(|stored| stored.followers_url.clone())
    else {
        return; // an actor with no followers collection has nobody to tell
    };
    let emojis = match profile_emojis(&state.db, account).await {
        Ok(emojis) => emojis,
        Err(err) => {
            warn!(%err, actor = %account.ap_id, "could not load profile emoji for the Update");
            return;
        }
    };
    let document = update_document(state, account, &followers_collection, row.as_ref(), &emojis);
    let unique_key = format!("profile-update:{}", account.id);
    match publisher::publish_in_background(state, account.id, &document, &[], Some(&unique_key))
        .await
    {
        Ok(()) => debug!(actor = %account.ap_id, "queued profile Update"),
        Err(err) => warn!(%err, actor = %account.ap_id, "could not queue profile Update"),
    }
}

/// The actor document for the profile in `account`, as an `Update` activity
/// addressed to the public and to the account's followers collection;
/// `actor` is the account's `activitypub_actors` row, which the document's
/// key and inbox come from, and `emojis` are the custom emojis the profile
/// uses, tagged on the actor.
pub(super) fn update_document(
    state: &AppState,
    account: &User,
    followers_collection: &str,
    actor: Option<&ActivityPubActor>,
    emojis: &[CustomEmoji],
) -> Value {
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": activity_id(&state.config, new_id()),
        "type": "Update",
        "actor": account.ap_id,
        "object": actor_json(account, actor, &state.config, emojis),
        "to": [AS_PUBLIC],
        "cc": [followers_collection],
    })
}

/// Announce the account whose deletion sweep is running.
///
/// Its one non-test caller is `ActivityPub`'s adapter, which runs it on
/// [`AccountEvent::Deleted`](crate::protocols::events::AccountEvent::Deleted); the sweep
/// fires that event instead of calling this.
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
///
/// Instances that hold the account's posts without any follow — through
/// boosts, replies, mentions or a relay — must learn it too (Mastodon's
/// `DeleteAccountService`). The accepted relays' inboxes join the follow
/// peers in the first publish; every other known remote inbox
/// ([`user::all_remote_inboxes`]) not already named goes in a second, so
/// the peers are not queued behind the long tail. The publisher drops
/// unreachable hosts when each job runs.
///
/// # Errors
///
/// Any failure to read the recipients or to queue a publish is returned, so
/// the deletion job can retry *before* it clears the follow rows the
/// recipient list is built from. Federation disabled, a remote account and
/// an account nobody knew all answer `Ok(())`.
pub async fn after_account_deleted(state: &AppState, account: &User) -> AppResult<()> {
    if !state.config.federation_enabled.get() {
        return Ok(());
    }
    if !account.local.get() {
        return Ok(()); // only our own actors announce themselves
    }
    let inboxes = known_inboxes(state, account).await?;
    if inboxes.peers.is_empty() && inboxes.long_tail.is_empty() {
        return Ok(()); // nobody on another instance ever knew this account
    }
    let actor_row = activitypub_actor::get(&state.db, account.id).await?;
    let followers = activitypub_actor::followers_address(actor_row.as_ref(), &account.ap_id);
    let document = delete_document(state, account, &followers);
    publish_to_known(state, account, &document, &inboxes, "Delete").await
}

/// The remote inboxes that know an account, in the two batches an
/// account-wide announcement goes out in.
#[derive(Debug)]
pub(super) struct KnownInboxes {
    /// The follow peers' inboxes (followers, followees) and the accepted relays'.
    pub(super) peers: Vec<String>,
    /// Every other known remote inbox, which hears behind the peers.
    pub(super) long_tail: Vec<String>,
}

/// Snapshots the inboxes of every instance that may hold `account`'s posts.
///
/// They are its remote followers and followees, the accepted relays, and
/// then every other remote inbox this instance has stored: the set
/// [`after_account_deleted`] and the suspension `Update` share.
///
/// # Errors
///
/// Any database failure reading the follow graph, the relays or the inboxes.
pub(super) async fn known_inboxes(state: &AppState, account: &User) -> AppResult<KnownInboxes> {
    let mut peers: Vec<String> = Vec::new();
    let fans = follow::remote_followers(&state.db, account.id).await?;
    peers.extend(best_inboxes(&state.db, &fans).await?);
    let idols = follow::remote_followees(&state.db, account.id).await?;
    peers.extend(best_inboxes(&state.db, &idols).await?);
    peers.extend(relay::accepted_inboxes(&state.db).await?);
    // Two remote accounts on one instance name one shared inbox; the
    // publisher's fan-out deduplicates by URL, so the snapshot POSTs there
    // once.
    let named: HashSet<&str> = peers.iter().map(String::as_str).collect();
    // Every other inbox this instance has ever stored: the long tail that
    // learns of the announcement behind the follow peers, never ahead of them.
    let long_tail: Vec<String> = user::all_remote_inboxes(&state.db)
        .await?
        .into_iter()
        .filter(|inbox| !named.contains(inbox.as_str()))
        .collect();
    Ok(KnownInboxes { peers, long_tail })
}

/// Queues `document` for the peers, then for the long tail, as `account`;
/// `what` names the activity in the debug log.
///
/// # Errors
///
/// Any failure to queue a publish job.
pub(super) async fn publish_to_known(
    state: &AppState,
    account: &User,
    document: &Value,
    inboxes: &KnownInboxes,
    what: &str,
) -> AppResult<()> {
    for (batch, priority) in [(&inboxes.peers, "peers"), (&inboxes.long_tail, "long tail")] {
        if batch.is_empty() {
            continue;
        }
        publisher::publish_in_background(state, account.id, document, batch, None).await?;
        debug!(
            actor = %account.ap_id,
            inboxes = batch.len(),
            priority,
            what,
            "queued account federation"
        );
    }
    Ok(())
}

/// The account `Delete` activity: the actor id as both `actor` and
/// `object`, addressed to the public with the followers collection in `cc`,
/// under the deterministic id the ticket pins. `followers` is the address of
/// the account's followers collection.
fn delete_document(state: &AppState, account: &User, followers: &str) -> Value {
    let id = format!(
        "{}/activities/delete-user-{}",
        state.config.base_url, account.id
    );
    json!({
        "@context": note::json_ld_context(&state.config),
        "id": id,
        "type": "Delete",
        "actor": account.ap_id,
        "object": account.ap_id,
        "to": [AS_PUBLIC],
        "cc": [followers],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::{
            activitypub_actor::ActivityPubActorWrite,
            custom_emoji::{self, NewCustomEmoji},
            user::{Flag, NewUser, followers, insert},
        },
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, delivery_jobs, job_count, local,
            publish_jobs, remote, run_publishes,
        },
        test_fixtures::write_actor,
    };

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
        assert_eq!(activity["to"][0], AS_PUBLIC);
        assert_eq!(activity["cc"][0], followers(&alice.ap_id));
    }

    /// The queued `Update` carries the `Emoji` tags of the local emojis the
    /// profile uses, and none for a shortcode without a row.
    #[tokio::test]
    async fn the_update_document_carries_the_profile_emoji_tags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let url = format!("{TEST_BASE_URL}/emoji/wiredcat.png");
        let _stored = custom_emoji::upsert(&pool, &NewCustomEmoji::local("wiredcat", &url))
            .await
            .expect("the emoji stores");
        alice.name = "Alice :wiredcat: :nosuch:".to_owned();
        let state = app_state(&pool);

        after_profile_updated(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        let (args, _key) = &jobs[0];
        assert_eq!(
            args["activity"]["object"]["tag"],
            json!([{
                "type": "Emoji",
                "id": url,
                "name": ":wiredcat:",
                "icon": {"type": "Image", "url": url},
            }]),
            "one tag for the known emoji, none for `:nosuch:`"
        );
    }

    /// The `Update` reads the actor row: the document's key and inbox, and
    /// the `cc` collection, are the row's although `users` says otherwise.
    #[tokio::test]
    async fn the_update_document_follows_the_actor_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let changed = sqlx::query(
            "UPDATE activitypub_actors
                SET public_key = 'ROW-KEY', inbox = $2, followers_url = $3
              WHERE user_id = $1",
        )
        .bind(alice.id)
        .bind("https://test.example/row-inbox")
        .bind("https://test.example/row-followers")
        .execute(&pool)
        .await
        .expect("the actor row updates")
        .rows_affected();
        assert_eq!(changed, 1, "the account has one actor row");
        let state = app_state(&pool);

        after_profile_updated(&state, &alice).await;

        let jobs = publish_jobs(&pool).await;
        let (args, _key) = &jobs[0];
        let activity = &args["activity"];
        assert_eq!(activity["object"]["publicKey"]["publicKeyPem"], "ROW-KEY");
        assert_eq!(
            activity["object"]["inbox"],
            "https://test.example/row-inbox"
        );
        assert_eq!(activity["cc"][0], "https://test.example/row-followers");
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

        after_account_deleted(&state, &alice)
            .await
            .expect("the Delete federation runs");

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
        assert_eq!(activity["to"][0], AS_PUBLIC);
        assert_eq!(
            inboxes_of(args),
            vec!["https://remote.example/inbox".to_owned(); 2],
            "the snapshot names each account's best inbox"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "both remote accounts share one instance inbox, and it is delivered to once"
        );
    }

    /// Two followers on one shared inbox cost one delivery job.
    #[tokio::test]
    async fn two_followers_on_a_shared_inbox_get_one_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let first = remote(&pool, "first").await;
        let second = remote(&pool, "second").await;
        accepted_follow(&pool, &first, &alice).await;
        accepted_follow(&pool, &second, &alice).await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice)
            .await
            .expect("the Delete federation runs");
        run_publishes(&state, &pool).await;

        let deliveries = delivery_jobs(&pool).await;
        assert_eq!(deliveries.len(), 1, "one shared inbox, one delivery");
        assert_eq!(deliveries[0]["inbox"], "https://remote.example/inbox");
    }

    /// An account that no remote instance ever touched publishes no
    /// `Delete`: there is nobody to tell.
    #[tokio::test]
    async fn a_lonely_deleted_account_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice)
            .await
            .expect("the Delete federation runs");

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

        after_account_deleted(&state, &bob)
            .await
            .expect("the Delete federation runs");
        after_profile_updated(&state, &bob).await;

        assert_eq!(job_count(&pool).await, 0, "a mirror announces nothing");
    }

    /// A remote mirror on `host` with a shared inbox and no relationship.
    async fn mirror_on(pool: &sqlx::PgPool, name: &str, host: &str) -> User {
        let mirror = insert(
            pool,
            &NewUser {
                nickname: format!("{name}@{host}"),
                ap_id: format!("https://{host}/users/{name}"),
                local: Flag::new(false),
                domain: Some(host.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the mirror fixture inserts");
        let _row = write_actor(
            pool,
            &ActivityPubActorWrite {
                inbox: Some(format!("https://{host}/users/{name}/inbox")),
                shared_inbox: Some(format!("https://{host}/inbox")),
                ..ActivityPubActorWrite::bare(mirror.id)
            },
        )
        .await;
        mirror
    }

    /// The `extra_inboxes` of one queued publish job, sorted.
    fn inboxes_of(args: &Value) -> Vec<String> {
        let mut inboxes: Vec<String> = args["extra_inboxes"]
            .as_array()
            .expect("the snapshot rides along")
            .iter()
            .filter_map(|inbox| inbox.as_str().map(str::to_owned))
            .collect();
        inboxes.sort_unstable();
        inboxes
    }

    /// The `Delete` reaches the follower and the accepted relay in the
    /// first publish and an unrelated mirror in a second; a mirror sharing
    /// the follower's inbox and a still-pending relay add nothing, and
    /// every inbox is named exactly once.
    #[tokio::test]
    async fn the_delete_reaches_relays_and_unrelated_inboxes_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = mirror_on(&pool, "fan", "a.example").await;
        let _twin = mirror_on(&pool, "twin", "a.example").await;
        let _stranger = mirror_on(&pool, "stranger", "b.example").await;
        accepted_follow(&pool, &fan, &alice).await;
        let _relay = relay::upsert(
            &pool,
            "https://relay.example/actor",
            "https://relay.example/inbox",
        )
        .await
        .expect("the relay stores");
        let _moved = relay::mark_accepted(&pool, "https://relay.example/actor")
            .await
            .expect("the relay grants");
        let _pending = relay::upsert(
            &pool,
            "https://pending.example/actor",
            "https://pending.example/inbox",
        )
        .await
        .expect("the pending relay stores");
        let state = app_state(&pool);

        after_account_deleted(&state, &alice)
            .await
            .expect("the Delete federation runs");

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 2, "peers first, the long tail second");
        assert_eq!(
            inboxes_of(&jobs[0].0),
            vec![
                "https://a.example/inbox".to_owned(),
                "https://relay.example/inbox".to_owned(),
            ]
        );
        assert_eq!(
            inboxes_of(&jobs[1].0),
            vec!["https://b.example/inbox".to_owned()]
        );
        assert_eq!(jobs[0].0["activity"], jobs[1].0["activity"]);
    }

    /// An account with no follow peers still tells every known remote
    /// inbox, in a single long-tail publish.
    #[tokio::test]
    async fn a_follower_less_account_still_announces_the_delete() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let _stranger = mirror_on(&pool, "stranger", "b.example").await;
        let state = app_state(&pool);

        after_account_deleted(&state, &alice)
            .await
            .expect("the Delete federation runs");

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "no peers, so only the long tail publishes");
        assert_eq!(
            inboxes_of(&jobs[0].0),
            vec!["https://b.example/inbox".to_owned()]
        );
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
                ..NewUser::default()
            },
        )
        .await
        .expect("the personal-inbox fixture inserts");
        let _row = write_actor(
            &pool,
            &ActivityPubActorWrite {
                inbox: Some("https://other.example/users/solo/inbox".to_owned()),
                ..ActivityPubActorWrite::bare(personal.id)
            },
        )
        .await;
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

        after_account_deleted(&state, &alice)
            .await
            .expect("the Delete federation runs");

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
}
