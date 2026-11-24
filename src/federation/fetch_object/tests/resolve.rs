//! Tests of the resolver: the local mirror first, then a contained fetch.

use super::*;

#[tokio::test]
async fn a_stored_uri_answers_from_the_database_without_a_fetch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    // No mock runs at all: `remote.invalid` cannot answer, so any
    // network attempt here would fail the resolve instead of proving
    // the database answered first.
    let alice = alice_mirror(&pool, "https://remote.invalid").await;
    let uri = object_url("https://remote.invalid", "quiet");
    let mut document = json!({
        "id": uri,
        "type": "Note",
        "attributedTo": alice.ap_id,
        "to": [PUBLIC],
        "content": "<p>quiet</p>",
    });
    document["published"] = json!("2024-03-01T10:00:00.000Z");
    let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &document)
        .await
        .expect("the fixture note ingests")
    else {
        panic!("a first-seen note is created");
    };

    let resolved = resolve_status(&state, &uri)
        .await
        .expect("the stored row resolves without the network");

    assert_eq!(
        resolved.map(|stored| stored.id),
        Some(row.id),
        "the mirror of the stored object answers as stored"
    );
}

#[tokio::test]
async fn a_notice_url_resolves_to_the_local_object_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let carol = user::insert(
        &pool,
        &NewUser {
            nickname: "carol".to_owned(),
            ap_id: format!("{TEST_BASE_URL}/users/carol"),
            local: Flag::new(true),
            ..NewUser::default()
        },
    )
    .await
    .expect("the local author inserts");
    let row = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus::local(&carol, "a local root"),
    )
    .await
    .expect("the local status inserts");
    let notice = row
        .url
        .clone()
        .expect("a local status carries its notice url");

    let resolved = resolve_status(&state, &notice)
        .await
        .expect("the alias resolves");

    assert_eq!(
        resolved.map(|stored| stored.id),
        Some(row.id),
        "the `/notice/…` form matches the row stored under `/objects/…`"
    );
}

#[tokio::test]
async fn a_local_id_this_database_lacks_resolves_to_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let missing = format!("{TEST_BASE_URL}/objects/{}", Uuid::now_v7());

    // No mock: an unknown id on this instance's own origin is answered
    // without the network, so a fetch attempt would surface as an error.
    let resolved = resolve_status(&state, &missing)
        .await
        .expect("the local miss is not a failure");

    assert_eq!(
        resolved, None,
        "this instance serves its own ids from its database"
    );
}

#[tokio::test]
async fn a_tombstoned_row_resolves_to_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let alice = alice_mirror(&pool, "https://remote.invalid").await;
    let uri = object_url("https://remote.invalid", "deleted");
    let document = json!({
        "id": uri,
        "type": "Note",
        "attributedTo": alice.ap_id,
        "to": [PUBLIC],
        "content": "<p>short lived</p>",
    });
    let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &document)
        .await
        .expect("the fixture ingests")
    else {
        panic!("a first-seen note is created");
    };
    let _tombstone = status::soft_delete(&pool, row.id)
        .await
        .expect("the delete tombstones");

    let resolved = resolve_status(&state, &uri)
        .await
        .expect("the tombstone answers");

    assert_eq!(
        resolved, None,
        "a deleted post resolves to nothing and is never refetched"
    );
}

#[tokio::test]
async fn a_fetched_note_is_ingested_exactly_once() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, peer_app(peer.clone()));
    let uri = object_url(&base, "one");

    let first = resolve_status(&state, &uri)
        .await
        .expect("the fixture note resolves");
    let hits_after_first = peer.hits.load(Ordering::SeqCst);
    let second = resolve_status(&state, &uri)
        .await
        .expect("the repeat resolve is not a failure");

    let Some(row) = first else {
        panic!("a fetchable note with a fetchable author becomes a status");
    };
    assert_eq!(row.uri, uri, "the object id is the stored uri");
    assert!(!row.local, "the row is a mirror");
    assert_eq!(
        second.map(|stored| stored.id),
        Some(row.id),
        "the second resolve restates the same row"
    );
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        hits_after_first,
        "the second resolve touched nothing: the row answers from the database"
    );
    assert_eq!(
        rows_for_uri(&pool, &uri).await,
        1,
        "one status for the two resolves"
    );
    server.abort();
}

#[tokio::test]
async fn a_document_outside_the_note_family_resolves_to_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let url = format!("{base}/place");
    let document = json!({
        "id": url,
        "type": "Place",
        "attributedTo": format!("{base}/users/{ALICE}"),
        "content": "<p>a location, not a post</p>",
    });
    let server = spawn(listener, fixed_app("/place", document));

    let resolved = resolve_status(&state, &url)
        .await
        .expect("a non-note is an answer, not a failure");

    assert_eq!(resolved, None, "only the Note family becomes a status");
    server.abort();
}

#[tokio::test]
async fn a_gone_or_missing_object_resolves_to_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, refusal_app(&peer));

    let gone = resolve_status(&state, &format!("{base}/gone"))
        .await
        .expect("a 410 completes the resolve");
    let missing = resolve_status(&state, &format!("{base}/missing"))
        .await
        .expect("a 404 completes the resolve");
    let flaky = resolve_status(&state, &format!("{base}/flaky")).await;

    assert_eq!(gone, None, "gone is nothing, retryable or not");
    assert_eq!(missing, None, "missing is nothing");
    assert!(
        matches!(flaky, Err(FederationError::Transient(_))),
        "a 500 says not now: {flaky:?}"
    );
    server.abort();
}

#[tokio::test]
async fn a_foreign_document_is_not_folded_for_the_delivering_actor() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, peer_app(peer.clone()));
    // `mallory` delivers a `Create` naming alice's note: the fetched
    // document names its own author, not the deliverer.
    let mallory = mirror_user(&pool, "mallory", &base).await;

    let resolved = resolve_status_as_actor(&state, &object_url(&base, "one"), &mallory)
        .await
        .expect("the refusal is an answer, not a failure");

    assert_eq!(
        resolved, None,
        "a peer may only create its own posts by reference"
    );
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        1,
        "the refusal lands after the object GET and before any author fetch"
    );
    assert_eq!(
        rows_for_uri(&pool, &object_url(&base, "one")).await,
        0,
        "nothing is stored under the foreign author's object"
    );
    server.abort();
}

#[tokio::test]
async fn an_author_on_another_origin_is_never_fetched() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let other_listener = bind().await;
    let other_base = origin(&other_listener);
    let other = peer_at(&other_base);
    let other_server = spawn(other_listener, peer_app(other.clone()));
    let url = object_url(&base, "borrowed");
    let document = json!({
        "id": url,
        "type": "Note",
        "attributedTo": format!("{other_base}/users/{ALICE}"),
        "to": [PUBLIC],
        "content": "<p>claims a stranger wrote it</p>",
        "published": "2024-03-01T10:00:00.000Z",
    });
    let server = spawn(listener, fixed_app("/objects/borrowed", document));

    let resolved = resolve_status(&state, &url).await;

    assert!(
        matches!(&resolved, Err(FederationError::Permanent(detail))
            if detail == "attributedTo host differs from object host"),
        "a foreign author is a permanent refusal: {resolved:?}"
    );
    assert_eq!(
        other.hits.load(Ordering::SeqCst),
        0,
        "the foreign author's origin is never contacted"
    );
    assert_eq!(rows_for_uri(&pool, &url).await, 0, "nothing is stored");
    server.abort();
    other_server.abort();
}

#[tokio::test]
async fn an_author_that_is_not_an_http_url_is_refused_before_any_fetch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let url = object_url(&base, "urn");
    let document = json!({
        "id": url,
        "type": "Note",
        "attributedTo": "urn:uuid:3f1c0a52-0000-4000-8000-000000000000",
        "to": [PUBLIC],
        "content": "<p>no usable author</p>",
    });
    let server = spawn(listener, fixed_app("/objects/urn", document));

    let resolved = resolve_status(&state, &url).await;

    assert!(
        matches!(resolved, Err(FederationError::Permanent(_))),
        "an author without an http host is refused: {resolved:?}"
    );
    server.abort();
}

#[tokio::test]
async fn an_author_on_the_object_origin_still_resolves() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, peer_app(peer.clone()));
    let uri = object_url(&base, "home");

    let resolved = resolve_status(&state, &uri)
        .await
        .expect("a same-origin author resolves");

    assert_eq!(
        resolved.map(|row| row.uri),
        Some(uri),
        "the note is ingested under its own id"
    );
    server.abort();
}
