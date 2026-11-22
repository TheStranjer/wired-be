//! Delivered announces: boosts folding onto the original status.

use super::*;

// --- Announce ---------------------------------------------------------------------

#[tokio::test]
async fn a_delivered_announce_boosts_the_status_under_its_activity_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "boost me").await;

    deliver(
        &state,
        &interaction(
            "Announce",
            MALLORY,
            &target_of(&posted),
            1,
            json!({"to": [PUBLIC], "published": "2026-11-01T00:00:00Z"}),
        ),
        &mallory,
    )
    .await;

    assert_eq!(
        boost_rows(&pool).await,
        vec![(
            interaction_id("Announce", MALLORY, 1),
            "public".to_owned(),
            "2026-11-01".to_owned(),
            posted.uri.clone(),
            MALLORY.to_owned(),
        )],
        "a reblog row IS the activity: its uri, its date, its public addressing"
    );
    let seen = viewer(&pool, &alice).await;
    assert_eq!(seen.status(posted.id).await["reblogs_count"], json!(1));
    let told = seen.told_of(TYPE_REBLOG).await;
    assert_eq!(told.len(), 1, "the author is told once");
    assert_eq!(told[0]["account"]["id"], json!(mallory.id.to_string()));
    assert_eq!(told[0]["status"]["id"], json!(posted.id.to_string()));
}

#[tokio::test]
async fn a_boost_of_a_boost_and_a_cc_addressed_boost_fold_onto_the_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (_alice, posted) = alice_post(&pool, "boost chain").await;

    // mallory boosts, addressed so the public address sits in `cc`: the
    // row parses the same way a note would — unlisted.
    let first = interaction(
        "Announce",
        MALLORY,
        &target_of(&posted),
        1,
        json!({"to": [format!("{}followers", MALLORY)], "cc": [PUBLIC]}),
    );
    deliver(&state, &first, &mallory).await;
    // bob boosts the boost: the pair is folded to the original, and bob's
    // public addressing gives the public row.
    deliver(
        &state,
        &interaction(
            "Announce",
            BOB,
            &json!(first["id"]),
            2,
            json!({"to": [PUBLIC]}),
        ),
        &bob,
    )
    .await;

    let rows = boost_rows(&pool).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str()),
        (first["id"].as_str().expect("an id"), "unlisted"),
        "cc'd boost lands unlisted"
    );
    assert_eq!(rows[1].1, "public");
    assert_eq!(rows[0].3, posted.uri, "both name the original");
    assert_eq!(rows[1].3, posted.uri);
}

#[tokio::test]
async fn a_restated_boost_folds_nothing_and_a_withdrawn_ids_restatement_folds_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "boosted once").await;
    let announce = |n| {
        interaction(
            "Announce",
            MALLORY,
            &target_of(&posted),
            n,
            json!({"to": [PUBLIC]}),
        )
    };

    deliver(&state, &announce(1), &mallory).await;
    // The same id redelivered, and a fresh id for the same pair: both
    // restate a standing fact.
    deliver(&state, &announce(1), &mallory).await;
    deliver(&state, &announce(2), &mallory).await;
    assert_eq!(boost_rows(&pool).await.len(), 1);
    assert_eq!(
        viewer(&pool, &alice).await.status(posted.id).await["reblogs_count"],
        json!(1)
    );

    // The undo tombstones the row; its uri stays reserved, so the id that
    // announced once never announces again — only a fresh one does.
    deliver(
        &state,
        &undo(MALLORY, &json!(interaction_id("Announce", MALLORY, 1)), 3),
        &mallory,
    )
    .await;
    assert!(
        boost_rows(&pool).await.is_empty(),
        "the live boost is gone with the undo"
    );
    let seen = viewer(&pool, &alice).await;
    assert_eq!(seen.status(posted.id).await["reblogs_count"], json!(0));
    assert!(
        seen.told_of(TYPE_REBLOG).await.is_empty(),
        "the telling is gone with the boost"
    );

    deliver(&state, &announce(1), &mallory).await;
    assert_eq!(
        boost_rows(&pool).await.len(),
        0,
        "the reserved id restates nothing"
    );
    deliver(&state, &announce(4), &mallory).await;
    assert_eq!(boost_rows(&pool).await.len(), 1, "a fresh id boosts again");
}

#[tokio::test]
async fn a_boost_of_a_hidden_status_folds_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let followers_only = common::create_status(&pool, &alice, "private", Visibility::Private).await;
    let just_bob = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            ..NewStatus::local(&alice, "direct")
        },
    )
    .await
    .expect("the direct fixture inserts");

    for (n, post) in [(1, &followers_only), (2, &just_bob)] {
        deliver(
            &state,
            &interaction(
                "Announce",
                MALLORY,
                &target_of(post),
                n,
                json!({"to": [PUBLIC]}),
            ),
            &mallory,
        )
        .await;
    }
    assert!(
        boost_rows(&pool).await.is_empty(),
        "private and direct post nothing to the boost fold"
    );
}
