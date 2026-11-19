//! The bulk permission-group writes of WBE-197: the remote mirror no role
//! can be set for, the acting admin whose own grant no revoke takes, and
//! the nickname-list pair that writes the whole named set at once — over
//! the body or the query string — logging and answering only the accounts
//! that really moved.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{
    NO_SUCH_GROUP, account_named, delete_json, flag_of, group_bulk, group_of, logged, one_entry,
    plain, post_json, seed, write_token,
};
use crate::common;

/// The 400 body of a role write aimed at a remote mirror.
const REMOTE_ROLE: &str = "Can't set role for a remote user";
/// The 403 body of the admin who revokes a group from their own account.
const SELF_REVOKE: &str = "You can't revoke your own admin status.";

/// A role is this instance's own to give: a remote mirror is the 400 both
/// the grant and the revoke answer, on its handle and on its id, and
/// nothing is written or logged.
#[tokio::test]
async fn no_group_is_set_for_a_remote_account() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    for path in [
        group_of("faraway@remote.example", "admin"),
        group_of(&seeded.mirror.id.to_string(), "moderator"),
    ] {
        let granted = post_json(&seeded.app, &token, &path, None).await;
        common::assert_answer(&granted, StatusCode::BAD_REQUEST, "a mirror grant");
        common::assert_error(&granted, REMOTE_ROLE, "the pinned wording");
        let revoked = delete_json(&seeded.app, &token, &path, None).await;
        common::assert_answer(&revoked, StatusCode::BAD_REQUEST, "a mirror revoke");
        common::assert_error(&revoked, REMOTE_ROLE, "the same words");
    }
    assert!(
        logged(&seeded.pool, "grant").await.is_empty()
            && logged(&seeded.pool, "revoke").await.is_empty(),
        "and no refusal logged anything"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.mirror
        )
        .await,
        "the mirror holds nothing"
    );

    let bulk = post_json(
        &seeded.app,
        &token,
        &group_bulk("admin"),
        Some(&json!({"nicknames": ["faraway@remote.example", "plain"]})),
    )
    .await;
    common::assert_answer(
        &bulk,
        StatusCode::OK,
        "a bulk grant with a mirror on the list",
    );
    assert_eq!(
        bulk.json_value(),
        json!({ "is_admin": true }),
        "the list write answers the group either way"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.mirror
        )
        .await,
        "the mirror was skipped, not written"
    );
    let plain = plain(&seeded.pool).await;
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the local account was written"
    );
    let (grant, subjects) = one_entry(&seeded.pool, "grant").await;
    assert_eq!(
        grant, "@root made @plain admin",
        "the entry names only who moved"
    );
    assert_eq!(subjects, vec!["plain"], "and its subject map says so");
}

/// The self-protection: neither group can be revoked from the acting admin,
/// and the bulk revoke leaves the caller's row alone instead of refusing
/// the whole list — which is the divergence from Pleroma the ticket pins.
#[tokio::test]
async fn an_admin_cannot_revoke_their_own_group() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    for name in ["admin", "moderator"] {
        let refused = delete_json(&seeded.app, &token, &group_of("root", name), None).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the self-revoke of {name}"),
        );
        common::assert_error(&refused, SELF_REVOKE, "Pleroma's wording");
    }
    let plain = plain(&seeded.pool).await;
    let _granted = post_json(&seeded.app, &token, &group_of("plain", "admin"), None).await;
    let mixed = delete_json(
        &seeded.app,
        &token,
        &group_bulk("admin"),
        Some(&json!({"nicknames": ["root", "plain"]})),
    )
    .await;
    common::assert_answer(
        &mixed,
        StatusCode::OK,
        "the bulk revoke that names the caller",
    );
    assert_eq!(
        mixed.json_value(),
        json!({ "is_admin": false }),
        "Pleroma's update map, as the ticket pins it"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.root
        )
        .await,
        "the caller's own grant stands"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and everybody else named lost it"
    );
    let (revoke, subjects) = one_entry(&seeded.pool, "revoke").await;
    assert_eq!(
        revoke, "@root revoked admin role from @plain",
        "one entry, naming only the account that moved"
    );
    assert_eq!(subjects, vec!["plain"], "the caller is no subject");
    assert_eq!(
        logged(&seeded.pool, "revoke").await.len(),
        1,
        "and the two refused self-revokes logged nothing"
    );
}

/// The nickname-list grant: everyone named and local joins the group, the
/// unknown handles are skipped inside the same success, and one entry names
/// the whole moved list in the submitted order.
#[tokio::test]
async fn the_bulk_group_grant_writes_to_everyone_named() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let moddy = account_named(&seeded.pool, "moddy").await;
    let names = json!({"nicknames": ["plain", "moddy", "ghost"]});

    let granted = post_json(&seeded.app, &token, &group_bulk("moderator"), Some(&names)).await;
    common::assert_answer(&granted, StatusCode::OK, "the bulk grant");
    assert_eq!(
        granted.json_value(),
        json!({ "is_moderator": true }),
        "the group's key, the state it was written to"
    );
    for account in [&plain, &moddy] {
        assert!(
            flag_of(
                &seeded.pool,
                "SELECT is_moderator FROM users WHERE id = $1",
                account
            )
            .await,
            "{} joined the group",
            account.nickname
        );
    }
    let (grant, subjects) = one_entry(&seeded.pool, "grant").await;
    assert_eq!(
        grant, "@root made @plain, @moddy moderator",
        "one entry for the whole list, in the submitted order"
    );
    assert_eq!(
        subjects,
        vec!["plain", "moddy"],
        "the unknown name is not a subject"
    );
}

/// The revoke half of the pair takes the group back the same way — the list
/// may come over the query string too — and a route that states no list or a
/// group off the two writes nothing at all.
#[tokio::test]
async fn the_bulk_group_revoke_takes_the_group_back() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let moddy = account_named(&seeded.pool, "moddy").await;
    let names = json!({"nicknames": ["plain", "moddy"]});
    let setup = post_json(&seeded.app, &token, &group_bulk("moderator"), Some(&names)).await;
    common::assert_answer(&setup, StatusCode::OK, "the grant this test undoes");

    let revoked = delete_json(
        &seeded.app,
        &token,
        &format!(
            "{}?nicknames[]=plain&nicknames[]=moddy",
            group_bulk("moderator")
        ),
        None,
    )
    .await;
    common::assert_answer(
        &revoked,
        StatusCode::OK,
        "the bulk revoke over the query string",
    );
    assert_eq!(
        revoked.json_value(),
        json!({ "is_moderator": false }),
        "the same answer, cleared"
    );
    for account in [&plain, &moddy] {
        assert!(
            !flag_of(
                &seeded.pool,
                "SELECT is_moderator FROM users WHERE id = $1",
                account
            )
            .await,
            "{} left the group",
            account.nickname
        );
    }
    let (revoke, _) = one_entry(&seeded.pool, "revoke").await;
    assert_eq!(
        revoke, "@root revoked moderator role from @plain, @moddy",
        "and the revoke names both"
    );

    let bare = post_json(&seeded.app, &token, &group_bulk("admin"), None).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no nicknames at all",
    );
    let bogus = post_json(
        &seeded.app,
        &token,
        &group_bulk("owner"),
        Some(&json!({"nicknames": []})),
    )
    .await;
    common::assert_answer(&bogus, StatusCode::NOT_FOUND, "a group off the two");
    common::assert_error(&bogus, NO_SUCH_GROUP, "Pleroma's wording");
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.actor
        )
        .await,
        "and the refused writes granted nobody — the instance actor included"
    );
}
