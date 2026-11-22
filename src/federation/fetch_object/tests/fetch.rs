//! Tests of the wire fetch: containment, redirects, size cap and budget.

use super::*;

#[tokio::test]
async fn a_self_identifying_object_is_returned_verbatim() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, peer_app(peer.clone()));

    let document = fetch_object_json(&state, &object_url(&base, "one"))
        .await
        .expect("the fixture note dereferences");

    assert_eq!(
        document["id"],
        json!(object_url(&base, "one")),
        "the document names the URL it was fetched from"
    );
    assert_eq!(document["type"], json!("Note"), "the body is the fixture");
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        1,
        "a self-naming object costs exactly one GET"
    );
    assert_eq!(
        peer.last_accept.lock().unwrap().as_deref(),
        Some(ACTOR_ACCEPT),
        "the fetch asks with both ActivityPub JSON media types"
    );
    server.abort();
}

#[tokio::test]
async fn gone_missing_and_flaky_answer_their_own_errors() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, refusal_app(&peer));

    let gone = fetch_object_json(&state, &format!("{base}/gone"))
        .await
        .expect_err("410 is not a document");
    assert!(matches!(gone, FetchError::Gone), "410 maps to Gone: {gone}");
    let missing = fetch_object_json(&state, &format!("{base}/missing"))
        .await
        .expect_err("404 is not a document");
    assert!(
        matches!(missing, FetchError::NotFound),
        "404 maps to NotFound: {missing}"
    );
    let flaky = fetch_object_json(&state, &format!("{base}/flaky"))
        .await
        .expect_err("500 is not a document");
    assert!(
        matches!(flaky, FetchError::Http(HttpError::Status(500))),
        "an unhandled status maps to Http: {flaky}"
    );
    server.abort();
}

#[tokio::test]
async fn unusable_bodies_are_invalid() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let app = refusal_app(&peer).merge(fixed_app("/noid", json!({"type": "Note"})));
    let server = spawn(listener, app);

    let html = fetch_object_json(&state, &format!("{base}/html"))
        .await
        .expect_err("an HTML body is not a document");
    assert!(
        matches!(&html, FetchError::Invalid(detail) if detail.contains("content type")),
        "a wrong media type is Invalid: {html}"
    );
    let broken = fetch_object_json(&state, &format!("{base}/broken"))
        .await
        .expect_err("unparsable JSON is not a document");
    assert!(
        matches!(&broken, FetchError::Invalid(detail) if detail.contains("parseable")),
        "a broken body is Invalid: {broken}"
    );
    let noid = fetch_object_json(&state, &format!("{base}/noid"))
        .await
        .expect_err("a document without an id cannot be contained");
    assert!(
        matches!(&noid, FetchError::Invalid(detail) if detail.contains("id")),
        "a document with no string id is Invalid: {noid}"
    );
    server.abort();
}

#[tokio::test]
async fn a_body_past_the_two_megabyte_cap_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let url = object_url(&base, "huge");
    let document = json!({
        "id": url,
        "type": "Note",
        "content": "x".repeat(MAX_OBJECT_BODY_BYTES + 4096),
    });
    let server = spawn(listener, fixed_app("/objects/huge", document));

    let err = fetch_object_json(&state, &url)
        .await
        .expect_err("the cap must cut the body off");

    assert!(
        matches!(err, FetchError::Http(HttpError::TooLarge)),
        "over the cap is an Http(TooLarge) failure: {err}"
    );
    server.abort();
}

#[tokio::test]
async fn a_moved_object_is_reanchored_at_its_own_id() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, moved_app(&peer));

    let document = fetch_object_json(&state, &object_url(&base, "moved"))
        .await
        .expect("a same-origin move is followed once");

    assert_eq!(
        document["id"],
        json!(object_url(&base, "real")),
        "the accepted document names the URL its own id was fetched from"
    );
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        2,
        "the claim costs exactly one refetch"
    );
    server.abort();
}

#[tokio::test]
async fn a_document_that_keeps_moving_is_invalid() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let app = fixed_app("/a", json!({"id": format!("{base}/b"), "type": "Note"})).merge(fixed_app(
        "/b",
        json!({"id": format!("{base}/a"), "type": "Note"}),
    ));
    let server = spawn(listener, app);

    let err = fetch_object_json(&state, &format!("{base}/a"))
        .await
        .expect_err("a document that moves again is not accepted");

    assert!(
        matches!(&err, FetchError::Invalid(detail) if detail.contains("identifies itself")),
        "the second mismatch names the moving document: {err}"
    );
    server.abort();
}

#[tokio::test]
async fn a_cross_origin_id_is_refused_without_a_refetch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let foreign = json!({
        "id": "http://foreign.invalid/objects/stolen",
        "type": "Note",
    });
    let app = refusal_app(&peer).merge(counted_fixed_app(&peer, "/foreign", foreign));
    let server = spawn(listener, app);

    let err = fetch_object_json(&state, &format!("{base}/foreign"))
        .await
        .expect_err("origin containment refuses a document naming another host");

    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        1,
        "exactly the first GET ran; the foreign id was never fetched"
    );
    assert!(
        matches!(&err, FetchError::Invalid(detail) if detail.contains("different origin")),
        "a foreign id is Invalid on the spot: {err}"
    );
    server.abort();
}

#[tokio::test]
async fn redirect_chains_are_bounded_by_three_hops() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = test_state(db.pool().clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, peer_app(peer.clone()));

    let document = fetch_object_json(&state, &format!("{base}/hop/2"))
        .await
        .expect("a two-hop chain is followed");
    assert_eq!(
        document["id"],
        json!(object_url(&base, "landed")),
        "the chain lands on the self-naming note"
    );

    let err = fetch_object_json(&state, &format!("{base}/hop/9"))
        .await
        .expect_err("a chain longer than the cap never resolves");
    assert!(
        matches!(err, FetchError::Http(_)),
        "past the redirect cap the client's failure rides on Http: {err}"
    );
    server.abort();
}

#[tokio::test]
async fn the_fetch_budget_stops_a_fanning_run() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let mut config = Config::for_tests();
    config.fetch.limits.max_fetches_per_activity = 1;
    let state = state_with_config(db.pool().clone(), config);
    let listener = bind().await;
    let base = origin(&listener);
    let peer = peer_at(&base);
    let server = spawn(listener, moved_app(&peer));

    let err = fetch_object_json(&state, &object_url(&base, "moved"))
        .await
        .expect_err("the refetch the moved document wants costs a second dereference");

    assert!(
        matches!(&err, FetchError::Invalid(detail) if detail.contains("budget")),
        "a run past its budget is refused: {err}"
    );
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        1,
        "the budget stopped the run before the second GET left"
    );
    server.abort();
}
