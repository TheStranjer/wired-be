//! Interactions with targets that are not visible to their actor, unknown
//! remote objects and emoji-carrying likes.

use super::*;

// --- the target's visibility to its actor ----------------------------------------

#[tokio::test]
async fn a_private_post_folds_a_like_only_for_an_accepted_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let secret = common::create_status(&pool, &alice, "followers only", Visibility::Private).await;

    deliver(
        &state,
        &interaction("Like", MALLORY, &target_of(&secret), 1, Value::Null),
        &mallory,
    )
    .await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "a stranger cannot like what it cannot see"
    );

    // The shared model write accepts bob, and then bob's like folds.
    let _accepted = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .expect("bob's follow of open alice is granted");
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&secret), 2, Value::Null),
        &bob,
    )
    .await;
    assert_eq!(
        favourite_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            secret.uri.clone(),
            interaction_id("Like", BOB, 2)
        )],
        "the accepted follower's like folds onto the private status"
    );
}

#[tokio::test]
async fn a_direct_post_folds_for_a_mentioned_actor_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let note = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: vec![bob.id],
            ..NewStatus::local(&alice, "psst, bob")
        },
    )
    .await
    .expect("the direct fixture inserts");

    deliver(
        &state,
        &interaction(
            "EmojiReact",
            MALLORY,
            &target_of(&note),
            1,
            json!({"content": "👀"}),
        ),
        &mallory,
    )
    .await;
    assert!(
        reaction_rows(&pool).await.is_empty(),
        "a stranger's peek stores nothing"
    );
    deliver(
        &state,
        &interaction(
            "EmojiReact",
            BOB,
            &target_of(&note),
            2,
            json!({"content": "👀"}),
        ),
        &bob,
    )
    .await;
    assert_eq!(
        reaction_rows(&pool).await.len(),
        1,
        "the mentioned actor reacts"
    );
}

// --- the fetch path and the lookalikes ---------------------------------------------

#[tokio::test]
async fn an_unknown_remote_object_is_fetched_and_then_folded() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (base, _peer) = spawn_peer().await;
    let remote_note = json!(format!("{base}/objects/n1"));

    // The Like's object is a name this instance has never heard of: it is
    // fetched, ingested, and folded in one delivery.
    deliver(
        &state,
        &interaction("Like", MALLORY, &remote_note, 1, Value::Null),
        &mallory,
    )
    .await;
    let ingested = sqlx::query_scalar::<_, String>("SELECT uri FROM statuses WHERE uri = $1")
        .bind(remote_note.as_str().expect("an id"))
        .fetch_optional(&pool)
        .await
        .expect("the statuses read");
    assert_eq!(
        ingested.as_deref(),
        remote_note.as_str(),
        "the fetched note is stored"
    );
    let row = favourite_rows(&pool).await.pop().expect("one favourite");
    assert_eq!(
        (row.0.as_str(), row.1.as_str()),
        (MALLORY, remote_note.as_str().expect("an id"))
    );

    // The second fold resolves from the store now: the announce names the
    // same ingested row, and its undo takes it back.
    deliver(
        &state,
        &interaction(
            "Announce",
            MALLORY,
            &remote_note,
            2,
            json!({"to": [PUBLIC]}),
        ),
        &mallory,
    )
    .await;
    let boost = boost_rows(&pool).await.pop().expect("the boost");
    assert_eq!(boost.3, remote_note.as_str().expect("an id"));

    // A gone object is nothing to retry and nothing to fold.
    deliver(
        &state,
        &interaction(
            "Like",
            MALLORY,
            &json!(format!("{base}/missing")),
            3,
            Value::Null,
        ),
        &mallory,
    )
    .await;
    assert_eq!(
        favourite_rows(&pool).await.len(),
        1,
        "the 404 folded nothing"
    );
}

#[tokio::test]
async fn a_like_carrying_an_emoji_is_a_reaction_and_resemblances_fold_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "misskey favourite").await;

    // The favourite-with-emoji shape: a `Like` whose content names one
    // emoji is a reaction, not a favourite.
    let misskey = interaction(
        "Like",
        BOB,
        &target_of(&posted),
        1,
        json!({"content": "❤️", "tag": []}),
    );
    deliver(&state, &misskey, &bob).await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "a favourite-with-emoji is no favourite"
    );
    assert_eq!(
        reaction_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            posted.uri.clone(),
            "❤️".to_owned(),
            None,
            interaction_id("Like", BOB, 1),
        )],
        "the reaction holds the Like's own activity id"
    );

    // A `Like` whose text names no emoji is refused outright — neither a
    // favourite nor a reaction — and `Dislike` never reaches a handler.
    for (kind, actor, n, extra) in [
        ("Like", BOB, 2, json!({"content": "<p>great post!</p>"})),
        ("Dislike", BOB, 3, Value::Null),
        ("Like", BOB, 4, json!({"content": "   "})),
    ] {
        let act = json!({
            "id": interaction_id(kind, actor, n),
            "type": kind,
            "actor": actor,
            "object": target_of(&posted),
        });
        deliver(&state, &merge(act, extra), &bob).await;
    }
    assert_eq!(
        reaction_rows(&pool).await.len(),
        1,
        "text that names nothing usable folds no reaction"
    );
    let rows = favourite_rows(&pool).await;
    assert_eq!(
        rows.len(),
        1,
        "the blank content liked plainly, nothing else did"
    );
    assert_eq!(rows[0].2, interaction_id("Like", BOB, 4));
    assert_eq!(
        viewer(&pool, &alice).await.status(posted.id).await["favourites_count"],
        json!(1)
    );
}
