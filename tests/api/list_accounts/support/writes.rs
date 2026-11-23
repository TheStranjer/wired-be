//! Adding and removing list members, and the pruning an unfollow causes.

use super::*;

/// Both body spellings add; a member who is already there keeps the place they
/// hold instead of moving to the end; an empty batch is accepted and changes
/// nothing.
#[tokio::test]
async fn adding_is_idempotent_in_either_body_spelling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool);

    let form = add_form(&app, &scene.token, scene.list.id, &[scene.bob.id]).await;
    assert_eq!(form.status, StatusCode::OK, "the form spelling adds");
    assert_eq!(form.json_value(), json!({}), "with the same answer");
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["bob"],
        "and bob is the member"
    );

    added(
        &app,
        &scene.token,
        scene.list.id,
        &[scene.bob.id, scene.bob.id, scene.carol.id],
    )
    .await;
    let after = listed(&app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(
        accts(&after),
        ["carol", "bob"],
        "re-adding bob left him first in the list's own order, and carol \
         joined behind him"
    );
    assert_eq!(after.len(), 2, "one membership each, not two for bob");

    let nobody = add_json(
        &app,
        &scene.token,
        scene.list.id,
        json!({"account_ids": []}),
    )
    .await;
    assert_eq!(nobody.status, StatusCode::OK, "an empty batch is accepted");
    assert_eq!(
        nobody.json_value(),
        json!({}),
        "and answers the empty object"
    );
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "limit=0")
            .await
            .len(),
        2,
        "adding nobody changed nobody"
    );
}

/// Only a followed account may be added: one the caller never followed, and
/// one whose request is still waiting, are the ticket's 422 — and a batch that
/// mixes the two writes nothing at all. Accepting the request is what lets the
/// same account in.
#[tokio::test]
async fn only_followed_accounts_can_be_added() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let erin = user(&pool, "erin").await;
    let frank = locked_user(&pool, "frank").await;
    let waiting = follow::follow(&pool, TEST_BASE_URL, &scene.alice, &frank)
        .await
        .expect("the request to a locked account is recorded");
    assert_eq!(
        waiting.state,
        FollowState::Pending,
        "and it waits, unanswered"
    );
    let app = common::test_app(pool.clone());

    for (what, members) in [
        ("an account the caller never followed", vec![erin.id]),
        (
            "a batch of one followed and one not",
            vec![scene.bob.id, erin.id],
        ),
        ("an account whose follow request is pending", vec![frank.id]),
    ] {
        let refused = add_json(&app, &scene.token, scene.list.id, account_ids(&members)).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{what} is a 422"
        );
        assert_eq!(
            refused.json_value(),
            json!({"error": NOT_FOLLOWED}),
            "with exactly the ticket's body for `{what}`"
        );
        assert!(
            listed(&app, &scene.token, scene.list.id, "limit=0")
                .await
                .is_empty(),
            "and the refused batch for `{what}` wrote nothing"
        );
    }

    let granted = follow::accept(&pool, waiting.id)
        .await
        .expect("the request is answered");
    assert_eq!(granted.state, FollowState::Accepted, "now a follow");
    added(&app, &scene.token, scene.list.id, &[frank.id]).await;
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["frank"],
        "which is the fact the add needed"
    );
}

/// An account id that is malformed or names no account is the 404, and it
/// outranks the follow rule: the phantom in a mixed batch is answered before
/// the followed one is ever considered. A body without `account_ids` anywhere
/// is the parameter 422.
#[tokio::test]
async fn unknown_account_ids_answer_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let phantom = Uuid::now_v7();
    let app = common::test_app(pool);

    for (what, texts) in [
        ("never minted", vec![phantom.to_string()]),
        ("not an id", vec!["not-a-uuid".to_owned()]),
        (
            "one real id and one phantom in one batch",
            vec![scene.bob.id.to_string(), phantom.to_string()],
        ),
        (
            "a followed account and a number that is no id",
            vec![scene.bob.id.to_string(), "12".to_owned()],
        ),
    ] {
        let refused = add_json(
            &app,
            &scene.token,
            scene.list.id,
            json!({"account_ids": texts}),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::NOT_FOUND,
            "a {what} id is a 404"
        );
        assert_eq!(
            refused.json_value()["error"],
            "Record not found",
            "Pleroma's wording for `{what}`"
        );
        assert!(
            listed(&app, &scene.token, scene.list.id, "limit=0")
                .await
                .is_empty(),
            "and `{what}` added nothing"
        );
    }

    let missing = add_json(&app, &scene.token, scene.list.id, json!({})).await;
    assert_eq!(
        missing.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no `account_ids` at all is the parameter 422"
    );
    assert!(
        missing.json_value()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("missing field `account_ids`"),
        "and it says what is missing: {}",
        missing.text()
    );
}

/// `DELETE` answers `{}` for every batch: a member goes, a non-member is
/// ignored, the ids arrive in the body or in the query, and neither an unknown
/// account nor a malformed id is an error here.
#[tokio::test]
async fn removing_members_answers_the_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let erin = user(&pool, "erin").await;
    let app = common::test_app(pool);
    added(&app, &scene.token, scene.list.id, &scene.ids()).await;

    let gone = removed(&app, &scene, &[scene.bob.id]).await;
    common::assert_answer(&gone, StatusCode::OK, "a body removes a member");
    assert_eq!(gone.json_value(), json!({}), "and answers the empty object");
    assert_members(&app, &scene, &["dave", "carol"], "one membership went").await;

    let twice = removed(&app, &scene, &[scene.bob.id]).await;
    common::assert_answer(
        &twice,
        StatusCode::OK,
        "removing a non-member is not an error",
    );
    assert_members(&app, &scene, &["dave", "carol"], "and it changed nothing").await;

    let by_query = remove_query(
        &app,
        &scene.token,
        scene.list.id,
        &format!("account_ids[]={}", scene.dave.id),
    )
    .await;
    common::assert_answer(&by_query, StatusCode::OK, "the query spelling removes");
    assert_eq!(
        by_query.json_value(),
        json!({}),
        "and answers the empty object"
    );
    assert_members(&app, &scene, &["carol"], "dave is gone").await;

    let texts: Vec<String> = vec![
        erin.id.to_string(),
        "not-a-uuid".to_owned(),
        scene.carol.id.to_string(),
    ];
    let mixed = remove_json(
        &app,
        &scene.token,
        scene.list.id,
        json!({"account_ids": texts}),
    )
    .await;
    common::assert_answer(
        &mixed,
        StatusCode::OK,
        "an account on nobody's list and a malformed id ride along",
    );
    assert_members(
        &app,
        &scene,
        &[],
        "the real member went and the rest was ignored",
    )
    .await;

    let nothing = common::TestRequest::delete(&members_path(scene.list.id))
        .bearer(&scene.token)
        .send(&app)
        .await;
    assert_eq!(
        nothing.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "`account_ids` is required here too"
    );
}

/// Unfollowing an account takes it out of every list of the account that
/// stopped following it, while another owner's membership of the same account
/// is not hers to prune.
#[tokio::test]
async fn unfollowing_prunes_the_followers_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let carol_token = token(&pool, &scene.carol, "read:lists").await;
    let carols = seed(&pool, &scene.carol, "carol's own").await;
    let seeded = list::add_members(&pool, carols.id, &[scene.bob.id])
        .await
        .expect("carol's fixture membership adds");
    assert_eq!(seeded, 1, "carol has bob on her own list");
    let app = common::test_app(pool.clone());
    added(
        &app,
        &scene.token,
        scene.list.id,
        &[scene.bob.id, scene.carol.id],
    )
    .await;

    let removed = follow::unfollow(&pool, &scene.alice, &scene.bob)
        .await
        .expect("alice unfollows bob");
    assert!(removed, "the relationship was there to remove");

    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["carol"],
        "bob is out of her list and carol stayed"
    );
    let bob_lists = common::TestRequest::get(&format!("/api/v1/accounts/{}/lists", scene.bob.id))
        .bearer(&scene.token)
        .send(&app)
        .await;
    assert_eq!(
        bob_lists.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "and bob is on none of her lists any more"
    );
    assert_eq!(
        accts(&listed(&app, &carol_token, carols.id, "limit=0").await),
        ["bob"],
        "while carol's membership of bob is alice's not to prune"
    );
}

/// How many jobs of any kind the queue holds.
async fn queued_jobs(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
        .fetch_one(pool)
        .await
        .expect("the jobs count reads")
}

/// A list is its owner's private bookmark: adding a followed REMOTE account
/// to it, and removing it again (or "removing" a remote account that was
/// never a member), queues no job — no `federator.publish`, nothing at all.
#[tokio::test]
async fn list_membership_writes_queue_no_federation() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let remote = common::create_remote_user(&pool, "remote", "peer.example").await;
    let stranger = common::create_remote_user(&pool, "stranger", "peer.example").await;
    let pending = follow::follow(&pool, TEST_BASE_URL, &scene.alice, &remote)
        .await
        .expect("the fixture follow records");
    let _granted = follow::accept(&pool, pending.id)
        .await
        .expect("the fixture grants the follow");
    let app = common::test_app(pool.clone());
    assert_eq!(queued_jobs(&pool).await, 0, "the fixtures queue nothing");

    added(&app, &scene.token, scene.list.id, &[remote.id]).await;
    assert_eq!(
        accts(&listed(&app, &scene.token, scene.list.id, "limit=0").await),
        ["remote@peer.example"],
        "the remote account is a member"
    );
    assert_eq!(
        queued_jobs(&pool).await,
        0,
        "adding a remote member queues nothing"
    );

    let dropped = remove_json(
        &app,
        &scene.token,
        scene.list.id,
        account_ids(&[remote.id, stranger.id]),
    )
    .await;
    common::assert_answer(&dropped, StatusCode::OK, "the removal is written");
    assert_members(&app, &scene, &[], "the member is gone").await;
    assert_eq!(
        queued_jobs(&pool).await,
        0,
        "removing a remote member, or a remote non-member, queues nothing"
    );
}
