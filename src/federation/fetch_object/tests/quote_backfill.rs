//! Tests of the `federator.quote_backfill` job: an unlinked remote quote
//! fetches its quoted object and links, a missing one stays a plain post,
//! an out-of-order pair links on arrival, and a quote chain costs one fetch.

use std::sync::atomic::Ordering;

use super::*;

/// The args of a `federator.quote_backfill` job for `status_id`.
fn quote_args(status_id: Uuid) -> Value {
    json!({ "status_id": status_id.to_string() })
}

/// A peer whose fixture note `name` quotes `quoted_url`.
fn quoting_peer(base: &str, name: &str, quoted_url: &str) -> Peer {
    let peer = peer_at(base);
    let _previous = peer
        .quotes
        .lock()
        .unwrap()
        .insert(name.to_owned(), quoted_url.to_owned());
    peer
}

/// The peer's standard surface plus its refusal routes.
fn full_app(peer: &Peer) -> Router {
    peer_app(peer.clone()).merge(refusal_app(peer))
}

/// Ingests the peer's fixture note `name` as `alice` and answers its row.
async fn ingest_fixture(state: &AppState, alice: &User, peer: &Peer, name: &str) -> status::Status {
    let IngestOutcome::Created(row) = ingest::ingest_note(state, alice, &note_document(peer, name))
        .await
        .expect("the fixture note ingests")
    else {
        panic!("a first-seen note is created");
    };
    row
}

/// The stored row `id`, read back.
async fn reread(pool: &PgPool, id: Uuid) -> status::Status {
    status::get_by_id(pool, id)
        .await
        .expect("the status reads")
        .expect("the status is stored")
}

/// The live `federator.quote_backfill` job queued for `status_id`, if any:
/// its args and its attempt budget.
async fn pending_quote_backfill(pool: &PgPool, status_id: Uuid) -> Option<(Value, i32)> {
    sqlx::query_as::<_, (Value, i32)>(
        "SELECT args, max_attempts FROM jobs
              WHERE kind = $1 AND unique_key = $2 AND state IN ('available', 'retryable')",
    )
    .bind(KIND_QUOTE_BACKFILL)
    .bind(quote_backfill_key(status_id))
    .fetch_optional(pool)
    .await
    .expect("the job read")
}

#[tokio::test]
async fn a_quote_of_an_unseen_object_fetches_and_links_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = quoting_peer(&base, "quote", &object_url(&base, "quoted"));
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, full_app(&peer));

    let quote = ingest_fixture(&state, &alice, &peer, "quote").await;
    assert_eq!(quote.quote_of_id, None, "the quoted object is not stored");
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        0,
        "ingest fetched nothing"
    );
    let (args, max_attempts) = pending_quote_backfill(&pool, quote.id)
        .await
        .expect("the ingest queued a quote backfill");
    assert_eq!(
        args,
        quote_args(quote.id),
        "the job names the quoting status"
    );
    assert_eq!(
        max_attempts, QUOTE_BACKFILL_MAX_ATTEMPTS,
        "with its retry budget"
    );

    handle_quote_backfill(state.clone(), args)
        .await
        .expect("the job links the quote");

    let quoted = status::get_by_uri(&pool, &object_url(&base, "quoted"))
        .await
        .expect("the quoted read")
        .expect("the quoted object was fetched and stored");
    let linked = reread(&pool, quote.id).await;
    assert_eq!(linked.quote_of_id, Some(quoted.id), "the quote is linked");
    assert_eq!(
        reread(&pool, quoted.id).await.quotes_count,
        1,
        "and counted"
    );
    let hits_after_first = peer.hits.load(Ordering::SeqCst);

    handle_quote_backfill(state.clone(), quote_args(quote.id))
        .await
        .expect("a second pass completes");
    assert_eq!(
        reread(&pool, quoted.id).await.quotes_count,
        1,
        "a second pass counts nothing"
    );
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        hits_after_first,
        "and fetches nothing"
    );
    server.abort();
}

#[tokio::test]
async fn a_quote_of_a_missing_object_stays_unlinked_without_failing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = quoting_peer(&base, "quote", &format!("{base}/missing"));
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, full_app(&peer));

    let quote = ingest_fixture(&state, &alice, &peer, "quote").await;
    handle_quote_backfill(state.clone(), quote_args(quote.id))
        .await
        .expect("a 404 completes the job quietly");

    assert_eq!(
        reread(&pool, quote.id).await.quote_of_id,
        None,
        "still a plain post"
    );
    assert_eq!(
        rows_for_uri(&pool, &format!("{base}/missing")).await,
        0,
        "nothing was stored for the missing object"
    );
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        1,
        "the 404 was asked once"
    );
    server.abort();
}

#[tokio::test]
async fn a_failing_peer_retries_the_quote_backfill() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = quoting_peer(&base, "quote", &format!("{base}/flaky"));
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, full_app(&peer));

    let quote = ingest_fixture(&state, &alice, &peer, "quote").await;
    let outcome = handle_quote_backfill(state.clone(), quote_args(quote.id)).await;

    assert!(
        matches!(outcome, Err(JobError::Retry(_))),
        "a 500 is a retry, got {outcome:?}"
    );
    assert_eq!(reread(&pool, quote.id).await.quote_of_id, None);
    server.abort();
}

#[tokio::test]
async fn a_quote_ingested_before_its_original_links_when_it_arrives() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    // No server is listening: the quoted object is not fetchable at all.
    let base = "http://127.0.0.1:9";
    let peer = quoting_peer(base, "quote", &object_url(base, "original"));
    let alice = alice_mirror(&pool, base).await;

    let quote = ingest_fixture(&state, &alice, &peer, "quote").await;
    assert_eq!(quote.quote_of_id, None, "the original is not stored yet");

    let original = ingest_fixture(&state, &alice, &peer, "original").await;

    assert_eq!(
        reread(&pool, quote.id).await.quote_of_id,
        Some(original.id),
        "the original's arrival linked the waiting quote"
    );
    assert_eq!(reread(&pool, original.id).await.quotes_count, 1);
    handle_quote_backfill(state.clone(), quote_args(quote.id))
        .await
        .expect("the queued job finds the quote linked and completes");
    assert_eq!(
        reread(&pool, original.id).await.quotes_count,
        1,
        "the job counts nothing twice"
    );
}

#[tokio::test]
async fn a_quote_chain_costs_one_fetch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    // quote → mid → deep: only `mid` is fetched, and it queues nothing.
    let peer = quoting_peer(&base, "quote", &object_url(&base, "mid"));
    let _previous = peer
        .quotes
        .lock()
        .unwrap()
        .insert("mid".to_owned(), object_url(&base, "deep"));
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, full_app(&peer));

    let quote = ingest_fixture(&state, &alice, &peer, "quote").await;
    handle_quote_backfill(state.clone(), quote_args(quote.id))
        .await
        .expect("the job completes");

    let mid = status::get_by_uri(&pool, &object_url(&base, "mid"))
        .await
        .expect("the mid read")
        .expect("the quoted object is stored");
    assert_eq!(reread(&pool, quote.id).await.quote_of_id, Some(mid.id));
    assert_eq!(
        reread(&pool, mid.id).await.quote_of_id,
        None,
        "its own quote is not chased"
    );
    assert!(
        pending_quote_backfill(&pool, mid.id).await.is_none(),
        "the fetched object queued no quote backfill"
    );
    assert_eq!(
        rows_for_uri(&pool, &object_url(&base, "deep")).await,
        0,
        "the next link of the chain was never fetched"
    );
    server.abort();
}

#[tokio::test]
async fn unusable_args_discard_and_a_gone_quote_completes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());

    assert!(matches!(
        handle_quote_backfill(state.clone(), json!({})).await,
        Err(JobError::Discard(_))
    ));
    assert!(matches!(
        handle_quote_backfill(state.clone(), json!({ "status_id": "nope" })).await,
        Err(JobError::Discard(_))
    ));
    assert_eq!(
        handle_quote_backfill(state, quote_args(Uuid::now_v7())).await,
        Ok(()),
        "a quoting status that is gone needs nothing"
    );
}
