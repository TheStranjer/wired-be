//! The auth and scope gates on the mute routes, and the expire job.

use super::*;

/// All three routes demand a user token: no token is the 401, an invalid one
/// the 401, and an app-only credential the 403.
#[tokio::test]
async fn the_routes_require_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "read write follow").await;
    let app = common::test_app(pool);

    for path in [
        MUTES.to_owned(),
        action_path(&bob.id, "mute"),
        action_path(&bob.id, "unmute"),
    ] {
        let anonymous = request(&path).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token cannot use `{path}`"
        );
        assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

        let invalid = request(&path).bearer("bogus-token").send(&app).await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is not treated as anonymous (`{path}`)"
        );
        assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

        let machine = request(&path).bearer(&app_only).send(&app).await;
        assert_eq!(
            machine.status,
            StatusCode::FORBIDDEN,
            "an app-only token is not a user (`{path}`)"
        );
        assert_eq!(machine.text(), USER_REQUIRED, "the user-required body");
    }
}

/// The listing's scope gate is exactly `read:mutes`: broad `read` and the scope
/// itself list; `follow` (the writes' alternative, not this read side), `write`
/// and an unrelated sub-scope answer the insufficient-permissions 403.
#[tokio::test]
async fn the_listing_requires_the_read_mutes_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;

    for scopes in ["read", "read:mutes"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = get(&app, Some(&token), MUTES).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{scopes}` token can list"
        );
    }

    for scopes in ["follow", "write", "read:accounts", ""] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = get(&app, Some(&token), MUTES).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "a `{scopes}` token cannot list"
        );
        assert_eq!(response.text(), NO_READ_SCOPE, "the denial names the scope");
    }
}

/// Both writes' gate is `write:mutes | follow`: broad `write`, the scope itself
/// and the `follow` root all mute and unmute; anything else is the
/// alternatives-joined 403, on both actions.
#[tokio::test]
async fn the_writes_take_write_mutes_or_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for (n, scopes) in ["write", "write:mutes", "follow"].iter().enumerate() {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let target = common::create_local_user(&pool, &format!("granted{n}")).await;
        let response = common::TestRequest::post(&action_path(&target.id, "mute"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{scopes}` token can mute"
        );
        assert_eq!(
            response.json_value(),
            relationship(
                &target.id,
                &json!({ "muting": true, "muting_notifications": true })
            ),
            "and the answer states it"
        );
        let lifted = common::TestRequest::post(&action_path(&target.id, "unmute"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(
            lifted.status,
            StatusCode::OK,
            "a `{scopes}` token can unmute"
        );
    }

    for scopes in ["read", "read:mutes", "write:accounts"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        for action in ACTIONS {
            let response = common::TestRequest::post(&action_path(&bob.id, action))
                .bearer(&token)
                .send(&app)
                .await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "a `{scopes}` token cannot {action}"
            );
            assert_eq!(response.text(), NO_WRITE_SCOPE, "the denial names both");
        }
    }

    assert!(
        stored_mutes(&pool, &alice).await.is_empty(),
        "the granted round trips left nothing behind"
    );
}

/// The `mutes.expire` job's pass deletes the lapsed rows the reads already
/// ignore and leaves every live mute alone, so the table cannot grow and the
/// listing does not change.
#[tokio::test]
async fn the_expire_job_removes_the_lapsed_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:mutes write:mutes").await;
    let lapsed = common::create_local_user(&pool, "lapsed").await;
    let running = common::create_local_user(&pool, "running").await;
    let forever = common::create_local_user(&pool, "forever").await;
    seed_mute(
        &pool,
        &alice,
        &lapsed,
        MuteOptions {
            notifications: true,
            expires_in: Some(60),
        },
    )
    .await;
    seed_mute(
        &pool,
        &alice,
        &running,
        MuteOptions {
            notifications: true,
            expires_in: Some(3_600),
        },
    )
    .await;
    seed_mute(&pool, &alice, &forever, MuteOptions::default()).await;
    set_expiry(&pool, &alice, &lapsed, -1).await;

    let state = common::test_app_state(pool.clone(), common::test_config());
    let outcome = handle_expire(state, FIRST_ATTEMPT, json!({})).await;
    assert_eq!(outcome, Ok(()), "the sweep reports success");

    let survivors = stored_mutes(&pool, &alice)
        .await
        .into_iter()
        .map(|(_, muted, _)| muted)
        .collect::<Vec<String>>();
    assert!(
        !survivors.contains(&lapsed.id.to_string()),
        "the lapsed mute's row is gone"
    );
    assert_eq!(survivors.len(), 2, "the two live mutes stayed");

    let app = common::test_app(pool);
    let listed = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        common::ids_of(&listed.json_value()).len(),
        2,
        "the listing is what it was before the sweep, too"
    );
}
