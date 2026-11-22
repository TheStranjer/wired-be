//! The ownership, cursor and scope gates on the member routes.

use super::*;

/// A list that is malformed, unknown or another account's is one and the same
/// 404 on all three member routes — for a stranger and for an account that is
/// a member of it — and the owner's rows survive every attempt.
#[tokio::test]
async fn a_list_that_is_not_the_callers_is_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let (erin, erin_token) = user_and_token(&pool, "erin", "read:lists write:lists").await;
    let bob_token = token(&pool, &scene.bob, "read:lists write:lists").await;
    let app = common::test_app(pool.clone());
    added(&app, &scene.token, scene.list.id, &[scene.bob.id]).await;
    let bods_own = seed(&pool, &scene.bob, "bob's own").await;
    assert_eq!(bods_own.user_id, scene.bob.id, "bob owns his own list");

    let body = account_ids(&[erin.id]);
    let unknown = Uuid::now_v7();
    for (who, bearer, what, id) in [
        ("a stranger", &erin_token, "alice's list", scene.list.id),
        ("a stranger", &erin_token, "bob's list", bods_own.id),
        ("a stranger", &erin_token, "a never-minted list", unknown),
        ("a member of it", &bob_token, "alice's list", scene.list.id),
        ("a member of it", &bob_token, "a never-minted list", unknown),
    ] {
        let read = members(&app, bearer, id, "").await;
        assert_eq!(read.status, StatusCode::NOT_FOUND, "{who} reads {what}");
        assert_eq!(
            read.json_value()["error"],
            "Record not found",
            "Pleroma's wording on GET, {who} / {what}"
        );
        let write = add_json(&app, bearer, id, body.clone()).await;
        assert_eq!(write.status, StatusCode::NOT_FOUND, "{who} adds to {what}");
        assert_eq!(
            write.json_value()["error"],
            "Record not found",
            "and the 404 outranks the follow rule {who} would fail too"
        );
        let dropped = remove_json(&app, bearer, id, body.clone()).await;
        assert_eq!(
            dropped.status,
            StatusCode::NOT_FOUND,
            "{who} removes from {what}"
        );
    }

    for bearer in [&erin_token, &bob_token] {
        let malformed = common::TestRequest::get(&format!("{ROOT}/not-a-uuid/accounts"))
            .bearer(bearer)
            .send(&app)
            .await;
        assert_eq!(
            malformed.status,
            StatusCode::NOT_FOUND,
            "a malformed list id is the same 404"
        );
    }

    let untouched = listed(&app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(
        accts(&untouched),
        ["bob"],
        "while alice's membership of bob survived every attempt on it"
    );
    assert!(
        listed(&app, &bob_token, bods_own.id, "limit=0")
            .await
            .is_empty(),
        "and bob's own list is still his and still empty"
    );
}

/// A page cursor that is not a position is the pagination 400: the members
/// page keys on ordinals, so an account id is no cursor here.
#[tokio::test]
async fn a_cursor_that_is_not_a_position_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool.clone());
    added(&app, &scene.token, scene.list.id, &[scene.bob.id]).await;

    let uuid_cursor = scene.bob.id.to_string();
    for key in ["max_id", "min_id", "since_id"] {
        for value in ["nope", "1.5", uuid_cursor.as_str()] {
            let response =
                members(&app, &scene.token, scene.list.id, &format!("{key}={value}")).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "`{key}={value}` is a 400"
            );
            assert_eq!(
                response.json_value()["error"],
                "Invalid id",
                "the pagination wording for `{key}={value}`"
            );
        }
    }
}

/// The scope pipeline of all three routes: no token is the 401, a bogus one
/// the 401 that names it, an app-only token the 403 that names the missing
/// user, and after that `read:lists` to read and `write:lists` to change, with
/// the broad `read`/`write` trees passing on both sides and an unrelated
/// sub-scope refused on all three.
#[tokio::test]
async fn the_scope_pipeline_guards_the_member_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let (_reader, reader_token) = user_and_token(&pool, "reader", "read:lists").await;
    let (_mutator, mutator_token) = user_and_token(&pool, "mutator", "write:lists").await;
    let (_narrow, wrong_scope) =
        user_and_token(&pool, "narrow", "read:statuses write:statuses").await;
    let (_broad_reader, broad_read) = user_and_token(&pool, "broad_r", "read").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(&members_path(scene.list.id))
        .send(&app)
        .await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token is a 401");
    common::assert_error(&anonymous, "Invalid credentials", "the absent token");

    let bogus = add_json(
        &app,
        "not-a-real-token",
        scene.list.id,
        json!({"account_ids": []}),
    )
    .await;
    common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token is a 401");
    common::assert_error(
        &bogus,
        "The access token is invalid",
        "the token-naming wording",
    );

    let userless = [
        members(&app, &app_only, scene.list.id, "").await,
        add_json(&app, &app_only, scene.list.id, account_ids(&[])).await,
    ];
    common::assert_all_refused(
        &userless,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only token",
    );

    for (who, bearer) in [
        ("`read:statuses write:statuses`", &wrong_scope),
        ("`write:lists`", &mutator_token),
    ] {
        let read = members(&app, bearer, scene.list.id, "").await;
        common::assert_answer(
            &read,
            StatusCode::FORBIDDEN,
            &format!("{who} may not read members"),
        );
        common::assert_error(
            &read,
            "Insufficient permissions: read:lists.",
            &format!("the read denial for {who}"),
        );
    }

    for (who, bearer) in [
        ("`read:statuses write:statuses`", &wrong_scope),
        ("`read:lists`", &reader_token),
        ("the broad `read`", &broad_read),
    ] {
        let refused = [
            add_json(&app, bearer, scene.list.id, account_ids(&[scene.bob.id])).await,
            remove_json(&app, bearer, scene.list.id, account_ids(&[scene.bob.id])).await,
        ];
        common::assert_all_refused(
            &refused,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: write:lists.",
            &format!("{who} may not change members"),
        );
    }
}

/// The granted side of the same pipeline: `read:lists` and the broad `read`
/// read the members of a list of their own (and still 404 on somebody else's,
/// because the scope check runs first), `write:lists` adds to her own list,
/// and the broad `write` may not reach into another account's.
#[tokio::test]
async fn the_granted_scopes_reach_only_their_own_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let (reader, reader_token) = user_and_token(&pool, "reader", "read:lists").await;
    let readers_own = seed(&pool, &reader, "reader's own").await;
    let (mutator, mutator_token) = user_and_token(&pool, "mutator", "write:lists").await;
    let (broad_reader, broad_read) = user_and_token(&pool, "broad_r", "read").await;
    let broads_own = seed(&pool, &broad_reader, "broad read's own").await;
    let (_broad_write, broad_write) = user_and_token(&pool, "broad_w", "write").await;
    let app = common::test_app(pool.clone());
    let theirs = seed(&pool, &mutator, "mutator's own").await;
    follow_user(&pool, &mutator, &scene.bob).await;

    for (who, bearer, own) in [
        ("`read:lists`", &reader_token, readers_own.id),
        ("the broad `read`", &broad_read, broads_own.id),
    ] {
        let own_list = members(&app, bearer, own, "").await;
        common::assert_answer(
            &own_list,
            StatusCode::OK,
            &format!("{who} reads her own members"),
        );
        let foreign = members(&app, bearer, scene.list.id, "").await;
        common::assert_answer(
            &foreign,
            StatusCode::NOT_FOUND,
            &format!("{who} gets the 404 on alice's list: the scope check passes first"),
        );
    }
    added(&app, &mutator_token, theirs.id, &[scene.bob.id]).await;
    let mutator_read = token(&pool, &mutator, "read:lists").await;
    assert_eq!(
        accts(&listed(&app, &mutator_read, theirs.id, "limit=0").await),
        ["bob"],
        "`write:lists` adds to her own list"
    );
    let foreign = remove_json(&app, &broad_write, theirs.id, account_ids(&[scene.bob.id])).await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "the broad `write` may write, but only to its own lists: this one is \
         mutator's, so the 404 answers it"
    );
    assert_eq!(
        accts(&listed(&app, &mutator_read, theirs.id, "limit=0").await),
        ["bob"],
        "and mutator's member stayed"
    );
}
