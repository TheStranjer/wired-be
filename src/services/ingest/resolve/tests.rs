//! The reference arms' tests: a reply to a stored parent links and groups,
//! an orphan inherits its context's conversation, mentions resolve to the
//! stored accounts and drop the unreachable one, and a quote resolves
//! against stored rows only — an unlinked one is queued for a fetch and
//! links when its original arrives.

use serde_json::json;
use sqlx::PgPool;

use super::*;
use crate::{
    federation::fetch_object::{KIND_QUOTE_BACKFILL, quote_backfill_key},
    models::user::{Flag, NewUser},
    services::ingest::{
        IngestDepth, IngestOutcome, ingest_note, ingest_note_with,
        test_support::{app_state, author, note},
    },
};

#[tokio::test]
async fn a_reply_to_a_stored_parent_links_and_groups() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let parent = ingest_note(&state, &bob, &note("ingest-root"))
        .await
        .expect("the root ingests");
    let IngestOutcome::Created(root) = parent else {
        panic!("the root writes");
    };
    let child = json!({
        "id": "https://remote.example/objects/ingest-child",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>replied</p>",
        "inReplyTo": "https://remote.example/objects/ingest-root",
        "context": "https://remote.example/contexts/ingest-thread",
    });

    let outcome = ingest_note(&state, &bob, &child)
        .await
        .expect("the reply ingests");
    let IngestOutcome::Created(row) = outcome else {
        panic!("the reply writes, got {outcome:?}");
    };

    assert_eq!(
        row.in_reply_to_id,
        Some(root.id),
        "the stored parent is linked by id"
    );
    assert_eq!(row.in_reply_to_account_id, Some(bob.id), "and by author");
    assert_eq!(
        row.in_reply_to_ap_id, None,
        "a linked parent leaves no dangling reference"
    );
    assert_eq!(
        row.conversation_id, root.conversation_id,
        "the reply joins the parent's conversation"
    );
    assert_eq!(
        row.ap_context.as_deref(),
        Some("https://remote.example/contexts/ingest-thread"),
        "and keeps the thread collection it names"
    );
    let replies = sqlx::query_scalar::<_, i32>("SELECT replies_count FROM statuses WHERE id = $1")
        .bind(root.id)
        .fetch_one(&pool)
        .await
        .expect("the parent reads back");
    assert_eq!(replies, 1, "and the parent counts the answer");
}

#[tokio::test]
async fn a_reply_to_an_unknown_parent_groups_by_context() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let orphan = |n: &str| {
        json!({
            "id": format!("https://remote.example/objects/{n}"),
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": format!("<p>orphan {n}</p>"),
            "inReplyTo": "https://remote.example/objects/never-delivered",
            "context": "https://remote.example/contexts/orphans",
        })
    };

    let first = ingest_note(&state, &bob, &orphan("ingest-orphan-1"))
        .await
        .expect("the first orphan ingests");
    let second = ingest_note(&state, &bob, &orphan("ingest-orphan-2"))
        .await
        .expect("the second orphan ingests");
    let (IngestOutcome::Created(one), IngestOutcome::Created(two)) = (first, second) else {
        panic!("both orphans write");
    };

    assert_eq!(
        one.in_reply_to_ap_id.as_deref(),
        Some("https://remote.example/objects/never-delivered"),
        "an unstored parent stays an ap_id reference"
    );
    assert_eq!(one.in_reply_to_id, None, "and gets no id link");
    assert_eq!(
        two.conversation_id, one.conversation_id,
        "the second status of the same context joins the first's conversation"
    );
}

#[tokio::test]
async fn mentions_resolve_to_stored_accounts_and_unresolved_drop() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let carol = user::insert(
        &pool,
        &NewUser {
            nickname: "carol@remote.example".to_owned(),
            ap_id: "https://remote.example/users/carol".to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the mentionee inserts");

    let object = json!({
        "id": "https://remote.example/objects/ingest-mentions",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>hi carol and nobody</p>",
        "tag": [
            { "type": "Mention", "href": "https://remote.example/users/carol", "name": "@carol" },
            { "type": "Mention", "href": "https://nowhere.invalid/users/gone", "name": "@gone" },
        ],
    });
    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("the mentions that resolve are enough");
    let IngestOutcome::Created(row) = outcome else {
        panic!("the post writes, got {outcome:?}");
    };

    let linked = mention_user_ids(&pool, row.id).await;
    assert_eq!(
        linked,
        vec![carol.id],
        "the stored mention links, the unreachable one drops"
    );
}

/// The users `status_id` mentions, in `status_mentions` order.
async fn mention_user_ids(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY user_id",
    )
    .bind(status_id)
    .fetch_all(pool)
    .await
    .expect("the mentions read")
}

#[tokio::test]
async fn a_locally_quoted_object_links_the_quote() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let IngestOutcome::Created(quoted) = ingest_note(&state, &bob, &note("ingest-quoted"))
        .await
        .expect("the quote target ingests")
    else {
        panic!("the target writes");
    };
    let quoting = json!({
        "id": "https://remote.example/objects/ingest-quoter",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>quoting</p>",
        "quoteUrl": "https://remote.example/objects/ingest-quoted",
    });

    let outcome = ingest_note(&state, &bob, &quoting)
        .await
        .expect("the quoting note ingests");
    let IngestOutcome::Created(row) = outcome else {
        panic!("the quoting post writes, got {outcome:?}");
    };
    assert_eq!(
        row.quote_of_id,
        Some(quoted.id),
        "a stored quote target links"
    );

    let stranger = json!({
        "id": "https://remote.example/objects/ingest-quoter-2",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>quoting nothing</p>",
        "quoteUrl": "https://remote.example/objects/never-quoted",
    });
    let IngestOutcome::Created(unlinked) = ingest_note(&state, &bob, &stranger)
        .await
        .expect("an unstored quote is no failure")
    else {
        panic!("the post writes");
    };
    assert_eq!(
        unlinked.quote_of_id, None,
        "and stays unlinked — resolving never fetches"
    );
    assert_eq!(
        queued_quote_backfills(&pool, unlinked.id).await,
        1,
        "the unlinked quote is queued for a fetch"
    );
    assert_eq!(
        queued_quote_backfills(&pool, row.id).await,
        0,
        "a linked quote needs none"
    );
}

/// How many live `federator.quote_backfill` jobs are queued for `status_id`.
async fn queued_quote_backfills(pool: &PgPool, status_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs
          WHERE kind = $1 AND unique_key = $2 AND state IN ('available', 'retryable')",
    )
    .bind(KIND_QUOTE_BACKFILL)
    .bind(quote_backfill_key(status_id))
    .fetch_one(pool)
    .await
    .expect("the job count reads")
}

/// A note quoting `quoted` under the `quoteUrl` key.
fn quoting_note(name: &str, quoted: &str) -> serde_json::Value {
    json!({
        "id": format!("https://remote.example/objects/{name}"),
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>quoting</p>",
        "quoteUrl": quoted,
    })
}

#[tokio::test]
async fn a_quote_ingested_before_its_original_links_when_the_original_arrives() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let quoted_uri = "https://remote.example/objects/late-original";
    let IngestOutcome::Created(quote) =
        ingest_note(&state, &bob, &quoting_note("early-quote", quoted_uri))
            .await
            .expect("the quote ingests")
    else {
        panic!("the quote writes");
    };
    assert_eq!(quote.quote_of_id, None, "its original is not stored yet");

    let IngestOutcome::Created(original) = ingest_note(&state, &bob, &note("late-original"))
        .await
        .expect("the original ingests")
    else {
        panic!("the original writes");
    };

    let linked = status::get_by_id(&pool, quote.id)
        .await
        .expect("the quote reads")
        .expect("the quote is stored");
    assert_eq!(
        linked.quote_of_id,
        Some(original.id),
        "the arrival linked the waiting quote"
    );
    assert_eq!(
        original.quotes_count, 1,
        "the returned row counts the quote it was linked to"
    );
    let stored = status::get_by_id(&pool, original.id)
        .await
        .expect("the original reads")
        .expect("the original is stored");
    assert_eq!(stored.quotes_count, 1, "and so does the stored one");
}

#[tokio::test]
async fn an_object_ingested_for_a_quote_queues_no_quote_fetch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let depth = IngestDepth {
        backfill: 0,
        follow_quotes: false,
    };
    let IngestOutcome::Created(row) = ingest_note_with(
        &state,
        &bob,
        &quoting_note("nested-quote", "https://remote.example/objects/deeper"),
        depth,
    )
    .await
    .expect("the note ingests") else {
        panic!("the note writes");
    };
    assert_eq!(row.quote_of_id, None, "the deeper quote is not linked");
    assert_eq!(
        queued_quote_backfills(&pool, row.id).await,
        0,
        "and not chased: a quote chain costs one fetch"
    );
}
