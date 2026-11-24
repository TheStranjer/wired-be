//! Muting and unmuting: flags, durations, re-mutes and malformed requests.

use super::*;

/// Muting answers the caller's relationship with `muting` and
/// `muting_notifications` both true (Mastodon's default for the
/// `notifications` parameter), stores the pair as one indefinite row with
/// `hide_notifications` set, and says nothing about the other end of the pair.
#[tokio::test]
async fn muting_lights_both_flags_from_the_caller_s_end() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let (bob, bob_token) = fixture(&pool, "bob", "read").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the mute is written");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "and JSON"
    );
    assert_eq!(
        response.json_value(),
        relationship(
            &bob.id,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "both mutes flags ride the answer"
    );

    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 1, "one row for the pair");
    assert_eq!(stored[0].1, bob.id.to_string(), "and it names bob");
    assert_eq!(
        stored[0].2, None,
        "with no duration asked for, the mute is indefinite"
    );
    assert!(
        mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "the model agrees"
    );
    assert_eq!(
        relationship_of(&app, &bob_token, &alice.id).await,
        relationship(&alice.id, &json!({})),
        "being muted lights nothing on bob's side of the pair"
    );
}

/// `notifications=false` mutes the timeline only: `muting` stays true,
/// `muting_notifications` is false, and the row stores the flag as the caller
/// set it. The parameter arrives in a JSON body, a form body and the query
/// string alike (decision 10).
#[tokio::test]
async fn the_notifications_parameter_says_which_flags_light() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:mutes").await;
    let app = common::test_app(pool.clone());

    for (n, nickname) in ["in_json", "in_form", "in_query"].iter().enumerate() {
        let muted = common::create_local_user(&pool, nickname).await;
        let path = action_path(&muted.id, "mute");
        let request = match n {
            0 => common::TestRequest::post(&path).json(&json!({ "notifications": false })),
            1 => common::TestRequest::post(&path).form(&[("notifications", "false")]),
            _ => common::TestRequest::post(&format!("{path}?notifications=0")),
        };
        let response = request.bearer(&token).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`{nickname}` spells a mute that works"
        );
        assert_eq!(
            response.json_value(),
            relationship(
                &muted.id,
                &json!({ "muting": true, "muting_notifications": false })
            ),
            "a timeline-only mute lights `muting` alone"
        );
    }

    assert_eq!(
        mute::muted_ids(&pool, alice.id).await.unwrap().len(),
        3,
        "three pairs, each spelled differently on the wire"
    );
    assert!(
        notification_ids(&pool, alice.id).await.is_empty(),
        "and none of them hides notifications"
    );
    assert_eq!(mute_count(&pool).await, 3, "and three rows");
}

/// `duration` becomes an `expires_at` that many seconds ahead, and `duration=0`
/// is Mastodon's spelling of an indefinite mute.
#[tokio::test]
async fn the_duration_parameter_stores_an_expiry() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let timed = common::create_local_user(&pool, "timed").await;
    let forever = common::create_local_user(&pool, "forever").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&action_path(&timed.id, "mute"))
        .bearer(&token)
        .json(&json!({ "duration": 3_600 }))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the timed mute is written");
    assert_eq!(
        response.json_value(),
        relationship(
            &timed.id,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "and the answer carries both flags"
    );

    let expiry: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM mutes WHERE muted_id = $1")
            .bind(timed.id)
            .fetch_one(&pool)
            .await
            .expect("the expiry reads");
    let expiry = expiry.expect("a timed mute stores an expiry");
    let ahead = expiry.signed_duration_since(Utc::now()).num_seconds();
    assert!(
        (3_590..=3_600).contains(&ahead),
        "the row's expiry is the hour the caller asked for, got {ahead}s"
    );

    let zero = common::TestRequest::post(&action_path(&forever.id, "mute"))
        .bearer(&token)
        .form(&[("duration", "0")])
        .send(&app)
        .await;
    assert_eq!(zero.status, StatusCode::OK, "`duration=0` is written");
    assert_eq!(
        stored_mutes(&pool, &alice)
            .await
            .into_iter()
            .filter(|(_, muted, _)| *muted == forever.id.to_string())
            .map(|(_, _, expires_at)| expires_at)
            .collect::<Vec<Option<String>>>(),
        vec![None],
        "and `duration=0` means no expiry at all"
    );
}

/// A mute whose `expires_at` lies in the past is treated as absent: the flags
/// fall back to their defaults and the listing drops the entry, while the row
/// is still on the books for the sweep to take.
#[tokio::test]
async fn a_lapsed_mute_is_absent_from_flags_and_listing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write:mutes").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    seed_mute(
        &pool,
        &alice,
        &bob,
        MuteOptions {
            notifications: true,
            expires_in: Some(3_600),
        },
    )
    .await;
    seed_mute(&pool, &alice, &carol, MuteOptions::default()).await;
    let app = common::test_app(pool.clone());

    set_expiry(&pool, &alice, &bob, -60).await;

    assert_eq!(
        relationship_of(&app, &token, &bob.id).await,
        relationship(&bob.id, &json!({})),
        "the lapsed mute lights neither flag"
    );
    assert!(
        !mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "and the model treats it as lifted"
    );
    let listed = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        common::ids_of(&listed.json_value()),
        vec![carol.id.to_string()],
        "the listing holds only the live mute"
    );
    assert_eq!(mute_count(&pool).await, 2, "both rows are still stored");
}

/// Re-muting a pair is one row whose options the second write rewrote: the same
/// id (so the listing's cursor does not move), the new `notifications` flag and
/// a duration counted from the latest write.
#[tokio::test]
async fn remuting_replaces_the_options_of_the_same_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let first = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .json(&json!({ "notifications": false }))
        .send(&app)
        .await;
    assert_eq!(first.status, StatusCode::OK, "the first mute is written");
    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 1, "one row");
    assert!(
        notification_ids(&pool, alice.id).await.is_empty(),
        "with notifications left visible"
    );

    let again = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .form(&[("notifications", "true"), ("duration", "600")])
        .send(&app)
        .await;
    assert_eq!(again.status, StatusCode::OK, "the repeat mute is written");
    assert_eq!(
        again.json_value(),
        relationship(
            &bob.id,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "and now both flags light"
    );

    let after = stored_mutes(&pool, &alice).await;
    assert_eq!(after.len(), 1, "still one row for the pair");
    assert_eq!(after[0].0, stored[0].0, "and it kept its id");
    assert!(
        after[0].2.is_some(),
        "with the duration the second write asked for: {:?}",
        after[0].2
    );

    let cleared = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        cleared.status,
        StatusCode::OK,
        "muting with no parameters at all is a rewrite too"
    );
    assert_eq!(
        stored_mutes(&pool, &alice).await[0].2,
        None,
        "and the absent duration clears the stored expiry"
    );
    assert_eq!(mute_count(&pool).await, 1, "one row throughout");
}

/// Unmuting answers the relationship back at its defaults, is idempotent, and
/// takes the row with it; the pair's follows are nobody's business here.
#[tokio::test]
async fn unmuting_lifts_the_mute_without_asking_for_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "alice", "read follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    mute_account(&app, &token, &bob.id).await;

    let lifted = common::TestRequest::post(&action_path(&bob.id, "unmute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(lifted.status, StatusCode::OK, "the unmute answers");
    assert_eq!(
        lifted.json_value(),
        relationship(&bob.id, &json!({})),
        "every flag back at its default"
    );
    assert_eq!(mute_count(&pool).await, 0, "and the row is gone");

    let again = common::TestRequest::post(&action_path(&bob.id, "unmute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(again.status, StatusCode::OK, "lifting twice is no error");
    assert_eq!(
        again.json_value(),
        relationship(&bob.id, &json!({})),
        "and the same unchanged answer"
    );
    assert_eq!(mute_count(&pool).await, 0, "writes nothing");

    let after_unmute = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        after_unmute.json_value(),
        json!([]),
        "and the listing is empty"
    );
}

/// Muting yourself is the 422 the ticket pins, `unmute` is not a path to it,
/// and an unknown or malformed id is the shared 404 on both writes.
#[tokio::test]
async fn a_self_mute_is_422_and_any_other_miss_is_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:mutes").await;
    let app = common::test_app(pool.clone());

    let yourself = common::TestRequest::post(&action_path(&alice.id, "mute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        yourself.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the caller's own id is the 422"
    );
    assert_eq!(yourself.text(), SELF_MUTE, "the ticket's wording");
    assert_eq!(
        stored_mutes(&pool, &alice).await.len(),
        0,
        "and the refusal stored nothing"
    );

    let unknown = Uuid::now_v7().to_string();
    for id in [unknown, "not-an-id".to_owned()] {
        for action in ACTIONS {
            let response = common::TestRequest::post(&raw_action_path(&id, action))
                .bearer(&token)
                .send(&app)
                .await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "a `{action}` of `{id}` is the shared 404"
            );
            assert_eq!(response.text(), NOT_FOUND, "the 404 body");
        }
    }
}

/// A parameter that is not the value the endpoint asks for is the extractor's
/// 422 (decision 10), on either of the two optional keys.
#[tokio::test]
async fn a_malformed_parameter_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let token = common::create_app_and_token(&pool, &alice, "write:mutes").await;
    let app = common::test_app(pool.clone());

    for (key, value) in [("notifications", "maybe"), ("duration", "soon")] {
        let response = common::TestRequest::post(&action_path(&bob.id, "mute"))
            .bearer(&token)
            .form(&[(key, value)])
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{key}={value}` is refused"
        );
        assert!(
            response.text().starts_with(INVALID_PARAMS),
            "the extractor names what did not parse: {}",
            response.text()
        );
        assert_eq!(mute_count(&pool).await, 0, "and nothing was stored");
    }
}
