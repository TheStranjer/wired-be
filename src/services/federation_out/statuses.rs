//! The two hooks that push a status out: the `Create` that carries a fresh
//! post and the `Update` that re-drafts an edited one, both built from the
//! note this instance serves at its object uri.
//!
//! The note machinery they read — the wrapper enum, the document builder and
//! the publishing gate — is [`super::notes`]', and the withdrawal a deleted
//! post is owed is [`super::withdrawals`]' arm.

use tracing::warn;

use super::audience::{Kind, queue_with_audience};
use super::notes::{NoteActivity, note_document, note_publisher};
use super::queue_activity;
use super::quotes::request_quote;
use crate::{
    models::status::{Status, clear_departed_voters},
    state::AppState,
};

/// Announce the status the caller just posted: the `Create` activity
/// carrying the note as this instance federates it.
///
/// The document is [`note::note_json`] wrapped in
/// [`note::create_activity_json`] — the very pair the `/objects/{uuid}` and
/// `/activities/{id}` routes serve, so what a follower's instance receives
/// and what it later fetches are one document — published as the status'
/// author, whose followers collection and mentionees the note's own `to` and
/// `cc` name and whom the publisher therefore expands. Nothing is added to
/// the fan-out beside that: a reply to a REMOTE parent needs no special
/// arm, because the parent's author rides in the mention list the note's
/// addressing is built from, and a `direct` status addresses its mentionees
/// alone, so the followers never hear it.
///
/// A quote of a REMOTE post also asks its author for consent: the
/// `QuoteRequest` of `request_quote`, sent to that author alone after the
/// `Create`.
///
/// Silence is the answer of a mirrored row (a remote post belongs to its
/// origin's `Create`), of a reblog row (decision 6 gives it the `Announce`
/// of [`after_reblog_changed`]), of a `local`-visibility post, and of a post
/// whose author is not one of our own active actors.
///
/// [`after_reblog_changed`]: super::after_reblog_changed
/// [`note::create_activity_json`]: crate::federation::note::create_activity_json
/// [`note::note_json`]: crate::federation::note::note_json
pub async fn after_status_created(state: &AppState, status: &Status) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(author) = note_publisher(state, status).await else {
        return;
    };
    let Some(document) = note_document(state, status.id, NoteActivity::Create).await else {
        return;
    };
    queue_activity(state, &author, &document, "note").await;
    request_quote(state, status, &author).await;
}

/// Announce the edit the caller just wrote: the `Update` activity carrying
/// the status' NEW note.
///
/// The document is the re-rendered [`note::note_json`] — content, warning,
/// attachments and the `updated` stamp the row now carries — wrapped in
/// [`note::update_activity_json`] under a freshly minted
/// `{BASE_URL}/activities/{uuid}` id, and it travels under the addressing
/// the note itself states: the same audience the `Create` reached, so an
/// edit corrects the copy at exactly the instances that hold one. The hook
/// is handed the *pre-edit* row, which is why the document is re-read by id
/// rather than built from the argument.
///
/// A `local`-visibility, mirrored or reblog row and a gone or
/// non-publishing author answer with silence, as in
/// [`after_status_created`].
///
/// [`note::note_json`]: crate::federation::note::note_json
/// [`note::update_activity_json`]: crate::federation::note::update_activity_json
pub async fn after_status_edited(state: &AppState, status: &Status) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(author) = note_publisher(state, status).await else {
        return;
    };
    let Some(document) = note_document(state, status.id, NoteActivity::Update).await else {
        return;
    };
    queue_with_audience(state, &author, status.id, &document, Kind::Update).await;
    // The edit's `Update` has been queued to the voters of the poll it may
    // have reset; they are no longer owed anything.
    if let Err(err) = clear_departed_voters(&state.db, status.id).await {
        warn!(%err, status = %status.id, "could not forget the voters of an edited poll");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;

    use crate::{
        config::{Config, TEST_BASE_URL},
        federation::note,
        federation::vocab::AS_PUBLIC,
        models::{
            status::{self, NewStatus, Visibility},
            user::{Flag, NewUser, User, insert},
        },
        services::federation_out::{
            after_status_deleted,
            test_support::{
                accepted_follow, app_state, delivered_inboxes, job_count, local, local_post,
                publish_jobs, remote, remote_status, run_publishes, the_single_activity,
            },
        },
        util::time::mastodon_time,
    };

    // ───────────────  the note lifecycle  ───────────────

    /// A remote mirror `nickname@peer.example`, shared inbox included: the
    /// second host the fan-out tests need beside [`remote`]'s one, so a
    /// delivery set proves the followers' expansion crosses instances.
    async fn peer(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@peer.example"),
                ap_id: format!("https://peer.example/users/{nickname}"),
                local: Flag::new(false),
                domain: Some("peer.example".to_owned()),
                inbox: Some(format!("https://peer.example/users/{nickname}/inbox")),
                shared_inbox: Some("https://peer.example/inbox".to_owned()),
                public_key: Some("PUB".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the peer fixture inserts")
    }

    /// The edit the route just wrote, applied straight to `posted`: the new
    /// text in both its spellings and the `edited_at` stamp
    /// [`crate::models::status_edit`] marks with. The test's stand-in for
    /// the edit transaction, so the hook runs on a row in the state a real
    /// edit leaves behind.
    async fn apply_edit(pool: &PgPool, posted: &Status, html: &str, text: &str) {
        let updated = sqlx::query(
            "UPDATE statuses SET content = $2, raw_text = $3, edited_at = now(), \
                updated_at = now() WHERE id = $1",
        )
        .bind(posted.id)
        .bind(html)
        .bind(text)
        .execute(pool)
        .await
        .expect("the edit fixture writes");
        assert_eq!(updated.rows_affected(), 1, "the post is there to edit");
    }

    /// Creating a public post for an account with followers on two remote
    /// hosts queues the exact `Create` the object route serves — the
    /// document and the status-derived id decision 6 pins — and the fan-out
    /// POSTs it to each host's shared inbox once.
    #[tokio::test]
    async fn a_public_post_publishes_the_create_to_the_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        let dave = peer(&pool, "dave").await;
        accepted_follow(&pool, &carol, &alice).await;
        accepted_follow(&pool, &dave, &alice).await;
        let posted = local_post(&pool, &alice, "hello world", Visibility::Public).await;
        let state = app_state(&pool);

        after_status_created(&state, &posted).await;

        let config = Config::for_tests();
        let ctx = note::load_note_context(&pool, &config, posted.id)
            .await
            .expect("the note context reads");
        let note = note::note_json(&ctx).expect("a public note renders");
        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity,
            note::create_activity_json(&note, &posted, &config),
            "the queued document is exactly the `Create` of the served note"
        );
        assert_eq!(activity["type"], "Create");
        assert_eq!(
            activity["id"],
            format!("{TEST_BASE_URL}/activities/{}", posted.id),
            "the activity id is the status' own — the address the C2S create promises"
        );
        assert_eq!(activity["actor"], alice.ap_id, "the author publishes");
        assert_eq!(activity["to"], json!([AS_PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the followers collection rides cc, and the publisher expands it"
        );
        assert_eq!(activity["object"]["id"], posted.uri);
        assert_eq!(activity["published"], mastodon_time(&posted.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://peer.example/inbox", "https://remote.example/inbox"],
            "each host is posted to once, at its shared inbox"
        );
    }

    /// A reply to a REMOTE parent needs no special arm: the parent's author
    /// rides the note's `cc` (the mention list the document is built from
    /// carries them), and the fan-out reaches their personal inbox beside
    /// the followers' shared one.
    #[tokio::test]
    async fn a_reply_addresses_the_remote_parent_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let parent = remote_status(&pool, &bob, "parent-to-answer", Visibility::Public).await;
        let reply = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(bob.id),
                ..NewStatus::local(&alice, "answering you")
            },
        )
        .await
        .expect("the reply fixture inserts");
        let state = app_state(&pool);

        after_status_created(&state, &reply).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Create");
        assert_eq!(activity["to"], json!([AS_PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "the parent's author stands in the reply's cc line"
        );
        assert_eq!(
            activity["object"]["inReplyTo"], parent.uri,
            "and the object names the parent it answers"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the follower and the parent author share one host, hence one delivery"
        );
    }

    /// A `direct` post is addressed to its mentionees alone: the note's `to`
    /// names the remote account, `cc` is empty, and the author's followers —
    /// remote included — hear nothing at all.
    #[tokio::test]
    async fn a_direct_post_goes_only_to_the_mentioned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let dm = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "a private word")
            },
        )
        .await
        .expect("the direct fixture inserts");
        let state = app_state(&pool);

        after_status_created(&state, &dm).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Create");
        assert_eq!(
            activity["to"],
            json!([bob.ap_id]),
            "the recipient is the whole audience"
        );
        assert_eq!(
            activity["cc"],
            json!([]),
            "no followers collection rides along"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the mentioned account's personal inbox, and the follower is not told"
        );
    }

    /// An edit queues the `Update` carrying the re-drafted note: a freshly
    /// minted activity id (never the `Create`'s), the same reserved object
    /// uri inside, the new text with its `updated` stamp, and the status'
    /// own addressing so the copy every host holds is the one corrected.
    #[tokio::test]
    async fn an_edit_publishes_the_update_of_the_new_note() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let posted = local_post(&pool, &alice, "first draft", Visibility::Public).await;
        let before = posted.clone();
        apply_edit(&pool, &posted, "<p>second draft</p>", "second draft").await;
        let state = app_state(&pool);

        after_status_edited(&state, &before).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Update");
        assert_eq!(activity["actor"], alice.ap_id);
        let id = activity["id"].as_str().expect("the update carries an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "a fresh own-instance activity id: {id}"
        );
        assert_ne!(
            id,
            format!("{TEST_BASE_URL}/activities/{}", posted.id),
            "an edit is its own activity, not a restatement of the create"
        );
        assert_eq!(
            activity["object"]["id"], posted.uri,
            "the object is the same reserved uri the create carried"
        );
        assert_eq!(
            activity["object"]["content"], "<p>second draft</p>",
            "and it is the NEW note, read back after the write"
        );
        assert!(
            activity["object"]["updated"].is_string(),
            "the re-drafted note carries its edit stamp"
        );
        assert_eq!(
            activity["to"],
            json!([AS_PUBLIC]),
            "the addressing is the status' own"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "both lines"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the host holding a copy is the host that hears about the edit"
        );
    }

    /// A post whose whole audience is local is still announced — the
    /// publisher just finds no inbox to announce it to: a local follower's
    /// timeline is the delivery, and no HTTP job is ever queued.
    #[tokio::test]
    async fn local_followers_receive_no_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        accepted_follow(&pool, &neighbour, &alice).await;
        let posted = local_post(&pool, &alice, "for our own eyes", Visibility::Public).await;
        let state = app_state(&pool);

        after_status_created(&state, &posted).await;
        assert_eq!(
            publish_jobs(&pool).await.len(),
            1,
            "the note itself is announced"
        );

        run_publishes(&state, &pool).await;
        assert!(
            delivered_inboxes(&pool).await.is_empty(),
            "and the fan-out names no inbox"
        );
    }

    /// The note hooks speak for local plain posts only: a mirrored row —
    /// which the ingest path runs through this very hook — and a reblog row,
    /// whose activity is the `Announce` of [`after_reblog_changed`], are both
    /// silence.
    #[tokio::test]
    async fn a_mirror_and_a_boost_publish_no_note_activities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mirrored = remote_status(&pool, &bob, "mirror-note", Visibility::Public).await;
        let original = local_post(&pool, &alice, "boost me", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_status_created(&state, &mirrored).await;
        after_status_edited(&state, &mirrored).await;
        after_status_deleted(&state, &mirrored).await;
        after_status_created(&state, &reblog).await;
        after_status_edited(&state, &reblog).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "neither a mirror's note nor a boost row is announced as a note"
        );
    }
}
