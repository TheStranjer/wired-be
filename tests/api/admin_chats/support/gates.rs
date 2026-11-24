//! The auth and scope gates on the admin chat routes.

use super::*;

// --- the gate ---------------------------------------------------------------------------------

/// The roles the gate answers before any record or scope: no credentials
/// and an unknown token are the 401s, an app-only token and a plain user
/// are the 403s on all four routes; a moderator is refused the three reads
/// (`NOT_ADMIN`) but is staff enough for the delete, which is walked
/// separately in `a_moderator_deletes_a_chat_message_but_cannot_read_chats`.
#[tokio::test]
async fn every_admin_chat_route_demands_a_user_token_of_the_right_role() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, _) = send(&pool, &alice, &bob, "gate check").await;
    let chat_id = alice_ref.chat_id.to_string();
    let message_id = alice_ref.id.to_string();

    let pleb = common::create_local_user(&pool, "pleb").await;
    let plain = common::create_app_and_token(&pool, &pleb, ALL_ADMIN).await;
    let mod_user = common::create_local_user_with(&pool, "mod", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let staff = common::create_app_and_token(&pool, &mod_user, ALL_ADMIN).await;
    let app_only = common::create_app_only_token(&pool, ALL_ADMIN).await;

    // Each route is walked with the verb it registers: a GET to the
    // DELETE-only route is a router-level 405 ahead of any auth.
    let routes = [
        (false, user_chats_path("alice")),
        (false, chat_path(&chat_id)),
        (false, messages_path(&chat_id)),
        (true, message_path(&chat_id, &message_id)),
    ];
    for (is_delete, path) in &routes {
        let unauthenticated = chat_request(*is_delete, path).send(&app).await;
        common::assert_answer(&unauthenticated, StatusCode::UNAUTHORIZED, "no token");
        assert_eq!(
            unauthenticated.error_field().as_deref(),
            Some(NO_TOKEN),
            "every route challenges alike"
        );

        let bogus = chat_request(*is_delete, path)
            .bearer("not-a-real-token")
            .send(&app)
            .await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token");
        assert_eq!(bogus.error_field().as_deref(), Some(INVALID_TOKEN));

        let mut refusals = vec![
            (app_only.as_str(), USER_REQUIRED),
            (
                plain.as_str(),
                if *is_delete { NOT_STAFF } else { NOT_ADMIN },
            ),
        ];
        if !*is_delete {
            refusals.push((staff.as_str(), NOT_ADMIN));
        }
        for (token, reason) in refusals {
            let refused = chat_request(*is_delete, path)
                .bearer(token)
                .send(&app)
                .await;
            common::assert_answer(&refused, StatusCode::FORBIDDEN, reason);
            assert_eq!(refused.error_field().as_deref(), Some(reason));
        }
    }

    // Nothing the walk refused deleted anything: a scoped admin still
    // lists the message the DELETE-only route guards.
    let (_late, late_token) = admin(&pool, "late-reader", "admin:read:chats").await;
    let still_listed = get(&app, &late_token, &messages_path(&chat_id)).await;
    assert_eq!(
        common::ids_of(&still_listed.json_value()),
        vec![message_id],
        "the refusals deleted nothing"
    );
}

/// A request on the route's own verb: `is_delete` marks the withdrawal
/// route (DELETE), the three reads ride GET — a method the router has no
/// route for would 405 ahead of the auth extractor.
fn chat_request(is_delete: bool, path: &str) -> common::TestRequest {
    if is_delete {
        common::TestRequest::delete(path)
    } else {
        common::TestRequest::get(path)
    }
}

/// The scope half of the gate: the three reads live in `admin:read:chats`
/// and the delete in `admin:write:chats`, the class roots grant both, and
/// the wrong class — in this area or any other — is refused naming the
/// class root, Pleroma's plug wording. A refused delete withdraws nothing.
#[tokio::test]
async fn the_chats_scopes_gate_reads_and_write_apart() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, _) = send(&pool, &alice, &bob, "scoped").await;
    let chat_id = alice_ref.chat_id.to_string();
    let message_id = alice_ref.id.to_string();

    let (_reader, read_only) = admin(&pool, "reader", "admin:read:chats").await;
    let (_writer, write_only) = admin(&pool, "writer", "admin:write:chats").await;
    let (_accountant, wrong_area) = admin(&pool, "accountant", "admin:read:accounts").await;

    for path in [
        user_chats_path("alice"),
        chat_path(&chat_id),
        messages_path(&chat_id),
    ] {
        let granted = get(&app, &read_only, &path).await;
        common::assert_answer(
            &granted,
            StatusCode::OK,
            &format!("admin:read:chats on {path}"),
        );
        let write_on_read = get(&app, &write_only, &path).await;
        common::assert_answer(
            &write_on_read,
            StatusCode::FORBIDDEN,
            &format!("admin:write:chats on {path}"),
        );
        assert_eq!(write_on_read.error_field().as_deref(), Some(NO_ADMIN_READ));
        let foreign = get(&app, &wrong_area, &path).await;
        common::assert_answer(
            &foreign,
            StatusCode::FORBIDDEN,
            "the accounts scope reads no chats",
        );
        assert_eq!(foreign.error_field().as_deref(), Some(NO_ADMIN_READ));
    }

    let read_on_write = delete(&app, &read_only, &message_path(&chat_id, &message_id)).await;
    common::assert_answer(
        &read_on_write,
        StatusCode::FORBIDDEN,
        "a read scope cannot delete",
    );
    assert_eq!(
        read_on_write.error_field().as_deref(),
        Some(NO_ADMIN_WRITE),
        "naming the write root Pleroma's plug asks for"
    );
    let still_there = get(&app, &read_only, &messages_path(&chat_id)).await;
    assert_eq!(
        common::ids_of(&still_there.json_value()),
        vec![message_id.clone()],
        "and the refused delete touched nothing"
    );

    let deleted = delete(&app, &write_only, &message_path(&chat_id, &message_id)).await;
    common::assert_answer(&deleted, StatusCode::OK, "admin:write:chats deletes");
    let empty = get(&app, &read_only, &messages_path(&chat_id)).await;
    assert_eq!(
        empty.json_value(),
        json!([]),
        "and the narrow write scope left the tombstone too"
    );
}

/// A moderator holds Pleroma's `messages_delete` but not `messages_read`:
/// with `admin:write:chats` the withdrawal succeeds and its log entry names
/// the moderator, while the message listing stays the 403 `NOT_ADMIN` even
/// for a token that carries the read scope.
#[tokio::test]
async fn a_moderator_deletes_a_chat_message_but_cannot_read_chats() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, _) = send(&pool, &alice, &bob, "abusive").await;
    let chat_id = alice_ref.chat_id.to_string();
    let message_id = alice_ref.id.to_string();

    let (_mod, token) = moderator(&pool, "mod", "admin:write:chats admin:read:chats").await;
    let refused = get(&app, &token, &messages_path(&chat_id)).await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "a moderator read");
    assert_eq!(refused.error_field().as_deref(), Some(NOT_ADMIN));

    let deleted = delete(&app, &token, &message_path(&chat_id, &message_id)).await;
    common::assert_answer(&deleted, StatusCode::OK, "a moderator deletes");

    let entry: (String, String) =
        sqlx::query_as("SELECT actor_nickname, message FROM moderation_log WHERE action = $1")
            .bind("chat_message_delete")
            .fetch_one(&pool)
            .await
            .expect("the log entry stands");
    assert_eq!(entry.0, "mod", "the entry names the moderator");
    assert_eq!(
        entry.1,
        format!("@mod deleted chat message ##{}", alice_ref.id),
        "and its sentence does too"
    );
}

/// The moderator's delete is still behind the scope: a moderator whose token
/// carries only the read scope is refused the delete, and the message stays.
#[tokio::test]
async fn a_moderator_without_the_write_scope_deletes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let (_, alice_ref, _) = send(&pool, &alice, &bob, "kept").await;
    let (_mod, token) = moderator(&pool, "mod", "admin:read:chats").await;

    let refused = delete(
        &app,
        &token,
        &message_path(&alice_ref.chat_id.to_string(), &alice_ref.id.to_string()),
    )
    .await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "no write scope");
    assert_eq!(refused.error_field().as_deref(), Some(NO_ADMIN_WRITE));
}
