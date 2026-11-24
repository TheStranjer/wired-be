//! Tests of the `federator.backfill` job and its thread walk.

use std::collections::HashSet;

use super::*;

/// The args of a `federator.backfill` job for `status_id` at `depth`.
fn backfill_args(status_id: Uuid, depth: i64) -> Value {
    json!({ "status_id": status_id.to_string(), "depth": depth })
}

/// A peer whose thread shape is `head → … → tail`: each named fixture
/// note answers the next one, and the last roots its own thread.
fn chain_peer(base: &str, names: &[&str]) -> Peer {
    let peer = peer_at(base);
    {
        let mut parents = peer.parents.lock().unwrap();
        for (name, parent) in names.iter().copied().zip(names.iter().copied().skip(1)) {
            let _inserted = parents.insert(name.to_owned(), parent.to_owned());
        }
    }
    peer
}

#[tokio::test]
async fn the_backfill_job_walks_a_thread_and_queues_the_next_link() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    // child → p1 → p2: the walk closes child→p1, storing p1 (itself
    // deferred), and queues p1's own pass one deeper — not restarted.
    let peer = chain_peer(&base, &["child", "p1", "p2"]);
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, peer_app(peer.clone()));

    let IngestOutcome::Created(child) =
        ingest::ingest_note(&state, &alice, &note_document(&peer, "child"))
            .await
            .expect("the child note ingests")
    else {
        panic!("a first-seen note is created");
    };
    assert_eq!(
        child.in_reply_to_ap_id.as_deref(),
        Some(object_url(&base, "p1").as_str()),
        "the parent is kept as an unresolved id"
    );
    assert_eq!(child.in_reply_to_id, None, "and nothing is linked yet");
    let (args, max_attempts) = pending_backfill(&pool, child.id)
        .await
        .expect("the ingest queued a backfill");
    assert_eq!(
        args["depth"],
        json!(0),
        "the first walk starts at depth zero"
    );
    assert_eq!(
        max_attempts, BACKFILL_MAX_ATTEMPTS,
        "the queued job carries the backfill retry budget"
    );

    handle_backfill(state.clone(), backfill_args(child.id, 0))
        .await
        .expect("the first walk links the child to its parent");

    let stored_parent = status::get_by_uri(&pool, &object_url(&base, "p1"))
        .await
        .expect("the parent read")
        .expect("the parent was fetched and stored");
    let linked = status::get_by_id(&pool, child.id)
        .await
        .expect("the child read")
        .expect("the child is stored");
    assert_eq!(
        linked.in_reply_to_id,
        Some(stored_parent.id),
        "the deferred id became a real link"
    );
    assert_eq!(linked.in_reply_to_ap_id, None, "the deferred id cleared");
    assert_eq!(
        linked.in_reply_to_account_id,
        Some(stored_parent.user_id),
        "the parent's author linked too"
    );
    let parent = status::get_by_id(&pool, stored_parent.id)
        .await
        .expect("the parent read")
        .expect("the parent is stored");
    assert_eq!(parent.replies_count, 1, "the parent gained its first reply");
    assert!(
        parent.in_reply_to_ap_id.is_some(),
        "the parent itself answers an unstored object"
    );
    let (next_args, _) = pending_backfill(&pool, parent.id)
        .await
        .expect("the walk queued the parent's own pass");
    assert_eq!(
        next_args["depth"],
        json!(1),
        "the chain continued one deeper, not restarted at zero"
    );
    server.abort();
}

#[tokio::test]
async fn the_backfill_walk_stops_at_the_depth_limit() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mut config = Config::for_tests();
    config.fetch.limits.thread_fetch_depth = 2;
    let state = state_with_config(pool.clone(), config);
    let listener = bind().await;
    let base = origin(&listener);
    // child → p1 → p2 with the limit at two: the walk closes child→p1
    // (the pass at depth 0) and p1→p2 (the pass at depth 1), and p2
    // is ingested at the limit, so it is never queued a pass of its own.
    let peer = chain_peer(&base, &["child", "p1", "p2", "p3"]);
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, peer_app(peer.clone()));
    let IngestOutcome::Created(child) =
        ingest::ingest_note(&state, &alice, &note_document(&peer, "child"))
            .await
            .expect("the child note ingests")
    else {
        panic!("a first-seen note is created");
    };

    handle_backfill(state.clone(), backfill_args(child.id, 0))
        .await
        .expect("the first walk completes");
    let p1 = status::get_by_uri(&pool, &object_url(&base, "p1"))
        .await
        .expect("the parent read")
        .expect("the first parent stored");
    handle_backfill(state.clone(), backfill_args(p1.id, 1))
        .await
        .expect("the second walk completes at the limit");

    let p2 = status::get_by_uri(&pool, &object_url(&base, "p2"))
        .await
        .expect("the grandparent read")
        .expect("the second parent stored within the limit");
    assert_eq!(
        p2.in_reply_to_ap_id.as_deref(),
        Some(object_url(&base, "p3").as_str())
    );
    assert!(
        pending_backfill(&pool, p2.id).await.is_none(),
        "at the limit ingest queues no pass instead of continuing"
    );
    assert_eq!(
        rows_for_uri(&pool, &object_url(&base, "p3")).await,
        0,
        "the ancestor past the limit was never fetched"
    );
    server.abort();
}

#[tokio::test]
async fn a_gone_parent_completes_the_walk_in_silence() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let alice = alice_mirror(&pool, &base).await;
    let peer = peer_at(&base);
    let mut child = note_document(&peer, "child");
    child["inReplyTo"] = json!(format!("{base}/gone"));
    let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &child)
        .await
        .expect("the child ingests")
    else {
        panic!("a first-seen note is created");
    };
    let server = spawn(listener, refusal_app(&peer));

    handle_backfill(state, backfill_args(row.id, 0))
        .await
        .expect("a 410 parent is not an error the queue should retry");

    let still_deferred = status::get_by_id(&pool, row.id)
        .await
        .expect("the child read")
        .expect("the child is stored");
    assert_eq!(
        still_deferred.in_reply_to_ap_id.as_deref(),
        Some(format!("{base}/gone").as_str()),
        "the link stays deferred: the parent may legitimately be missing"
    );
    server.abort();
}

#[tokio::test]
async fn a_flaky_parent_retries_the_walk() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let alice = alice_mirror(&pool, &base).await;
    let peer = peer_at(&base);
    let mut child = note_document(&peer, "child");
    child["inReplyTo"] = json!(format!("{base}/flaky"));
    let IngestOutcome::Created(row) = ingest::ingest_note(&state, &alice, &child)
        .await
        .expect("the child ingests")
    else {
        panic!("a first-seen note is created");
    };
    let server = spawn(listener, refusal_app(&peer));

    let err = handle_backfill(state, backfill_args(row.id, 0))
        .await
        .expect_err("a 500 parent is retryable");

    assert!(
        matches!(err, JobError::Retry(_)),
        "the queue runs the walk again: {err}"
    );
    server.abort();
}

#[tokio::test]
async fn the_walk_is_idempotent_across_repeats() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool.clone());
    let listener = bind().await;
    let base = origin(&listener);
    let peer = chain_peer(&base, &["child", "p1"]);
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, peer_app(peer.clone()));
    let IngestOutcome::Created(child) =
        ingest::ingest_note(&state, &alice, &note_document(&peer, "child"))
            .await
            .expect("the child ingests")
    else {
        panic!("a first-seen note is created");
    };
    handle_backfill(state.clone(), backfill_args(child.id, 0))
        .await
        .expect("the first walk links");
    let hits = peer.hits.load(Ordering::SeqCst);

    handle_backfill(state, backfill_args(child.id, 0))
        .await
        .expect("the repeat walk is not a failure");

    let stored_parent = status::get_by_uri(&pool, &object_url(&base, "p1"))
        .await
        .expect("the parent read")
        .expect("the parent stored");
    let parent = status::get_by_id(&pool, stored_parent.id)
        .await
        .expect("the parent read")
        .expect("the parent stored");
    assert_eq!(
        parent.replies_count, 1,
        "the counter stepped once, not twice"
    );
    assert_eq!(
        peer.hits.load(Ordering::SeqCst),
        hits,
        "the linked child resolves from the database, touching no network"
    );
    server.abort();
}

#[tokio::test]
async fn unusable_backfill_args_are_discarded() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(pool);

    let missing = handle_backfill(state.clone(), json!({"depth": 0}))
        .await
        .expect_err("args with no status id are unusable");
    let broken = handle_backfill(state, json!({"status_id": "not-a-uuid", "depth": 0}))
        .await
        .expect_err("a non-uuid id is unusable");

    assert!(
        matches!(missing, JobError::Discard(_)),
        "no id discards: {missing}"
    );
    assert!(
        matches!(broken, JobError::Discard(_)),
        "a broken id discards: {broken}"
    );
}

/// Every `federator.backfill` job in the table, oldest first: its unique
/// key and args, whatever its state.
async fn queued_backfills(pool: &PgPool) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>(
        "SELECT unique_key, args FROM jobs WHERE kind = $1 ORDER BY id",
    )
    .bind(KIND_BACKFILL)
    .fetch_all(pool)
    .await
    .expect("the job read")
}

#[tokio::test]
async fn an_endless_chain_stops_at_the_depth_limit_across_jobs() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let limit: usize = 3;
    let mut config = Config::for_tests();
    config.fetch.limits.thread_fetch_depth = limit;
    let state = state_with_config(pool.clone(), config);
    let listener = bind().await;
    let base = origin(&listener);
    // A hostile peer whose chain `c0 → c1 → … → c9` is far longer than the
    // limit: only the depth carried through the jobs can end the walk.
    let names: Vec<String> = (0..10).map(|n| format!("c{n}")).collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let peer = chain_peer(&base, &name_refs);
    let alice = alice_mirror(&pool, &base).await;
    let server = spawn(listener, peer_app(peer.clone()));
    let IngestOutcome::Created(_first) =
        ingest::ingest_note(&state, &alice, &note_document(&peer, "c0"))
            .await
            .expect("the first reply ingests")
    else {
        panic!("a first-seen note is created");
    };

    // Run the queued jobs the way a worker would, reading them back from
    // the table each round; the bound only fails a runaway test.
    let mut done: HashSet<String> = HashSet::new();
    for _round in 0..(limit * 4) {
        let Some((key, args)) = queued_backfills(&pool)
            .await
            .into_iter()
            .find(|(key, _args)| !done.contains(key))
        else {
            break;
        };
        let _fresh = done.insert(key);
        handle_backfill(state.clone(), args)
            .await
            .expect("a queued backfill runs");
    }

    let jobs = queued_backfills(&pool).await;
    assert!(
        jobs.iter().all(|(key, _args)| done.contains(key)),
        "every queued job ran: the walk ended on its own"
    );
    let stored = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
        .fetch_one(&pool)
        .await
        .expect("the status count reads");
    assert_eq!(
        stored,
        i64::try_from(limit + 1).expect("the limit fits"),
        "the first reply and `thread_fetch_depth` ancestors, no more"
    );
    assert_eq!(jobs.len(), limit, "the ancestor at the limit queues no job");
    for (_key, args) in &jobs {
        let id = args["status_id"]
            .as_str()
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .expect("the job names a status");
        let row = status::get_by_id(&pool, id)
            .await
            .expect("the status read")
            .expect("the status is stored");
        let distance: i64 = row
            .uri
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_prefix('c'))
            .and_then(|n| n.parse().ok())
            .expect("the fixture uri ends in its chain position");
        assert_eq!(
            args["depth"],
            json!(distance),
            "{}: the recorded depth is the distance from the first reply",
            row.uri
        );
    }
    assert_eq!(
        rows_for_uri(&pool, &object_url(&base, &format!("c{}", limit + 1))).await,
        0,
        "nothing past the limit was fetched"
    );
    server.abort();
}
