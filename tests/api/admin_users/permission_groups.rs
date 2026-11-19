//! The permission-group reads and the single-account grants of WBE-197:
//! the two-key answer of both read routes for every kind of account and
//! their 404s for a group off the two, the admin and moderator grants that
//! move one flag at a time and the revokes that take it back, and the
//! moderation-log sentences each write lands.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    NO_ADMIN_READ, NO_SUCH_GROUP, NOT_FOUND, USERS, delete_json, flag_of, group, group_of, logged,
    mint, one_entry, plain, post_json, read, seed, show, tags_of, write_token,
};
use crate::common::{self, TestRequest};

/// Both permission-group reads answer the same two-key object for any
/// account — an admin, a moderator, a plain member, a remote mirror, a row
/// named by its id — and refuse a group off the two and a name nobody
/// answers to with their 404s.
#[tokio::test]
async fn the_permission_group_reads_answer_both_flags() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let cases: &[(&str, Value)] = &[
        ("root", json!({ "is_moderator": false, "is_admin": true })),
        ("moddy", json!({ "is_moderator": true, "is_admin": false })),
        ("plain", json!({ "is_moderator": false, "is_admin": false })),
        (
            "faraway@remote.example",
            json!({ "is_moderator": false, "is_admin": false }),
        ),
    ];
    for (nickname, expected) in cases {
        let bare = read(&seeded.app, &seeded.token, &group(nickname)).await;
        assert_eq!(&bare, expected, "the group-less read of {nickname}");
        assert_eq!(
            bare.as_object().expect("an object").len(),
            2,
            "exactly the two flags"
        );
        let named = read(&seeded.app, &seeded.token, &group_of(nickname, "moderator")).await;
        assert_eq!(&named, expected, "and the named group answers the same");
        let admin = read(&seeded.app, &seeded.token, &group_of(nickname, "admin")).await;
        assert_eq!(&admin, expected, "for either group");
    }

    let by_id = read(
        &seeded.app,
        &seeded.token,
        &group(&seeded.root.id.to_string()),
    )
    .await;
    assert_eq!(
        by_id["is_admin"],
        json!(true),
        "the segment resolves as an id like the show's"
    );

    for verb in ["GET", "POST", "DELETE"] {
        let path = group_of("plain", "owner");
        let bearer = if verb == "GET" {
            seeded.token.clone()
        } else {
            write_token(&seeded).await
        };
        let bogus = match verb {
            "GET" => {
                TestRequest::get(&path)
                    .bearer(&bearer)
                    .send(&seeded.app)
                    .await
            }
            "POST" => post_json(&seeded.app, &bearer, &path, None).await,
            _ => delete_json(&seeded.app, &bearer, &path, None).await,
        };
        common::assert_answer(
            &bogus,
            StatusCode::NOT_FOUND,
            &format!("a group off the two, over {verb}"),
        );
        common::assert_error(&bogus, NO_SUCH_GROUP, "Pleroma's wording");
    }
    let miss = TestRequest::get(&group("ghost"))
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&miss, StatusCode::NOT_FOUND, "a name nobody answers to");
    common::assert_error(&miss, NOT_FOUND, "the module's 404");

    let write_only = write_token(&seeded).await;
    let refused = TestRequest::get(&group("plain"))
        .bearer(&write_only)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the write class must not read",
    );
    common::assert_error(&refused, NO_ADMIN_READ, "in the read class's words");
}

/// The single account's grant and revoke: the one-key answer of the group
/// that moved, the flag in storage, the roles in the entity, the granted
/// account reading the admin API it could not reach before, and one
/// `grant` / `revoke` entry per write.
#[tokio::test]
async fn a_group_grant_lands_and_a_revoke_takes_it_back() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let before = TestRequest::get(USERS)
        .bearer(&mint(&seeded.pool, &plain, "admin:read:accounts").await)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &before,
        StatusCode::FORBIDDEN,
        "the account is no admin yet",
    );

    let granted = post_json(&seeded.app, &token, &group_of("plain", "admin"), None).await;
    common::assert_answer(&granted, StatusCode::OK, "the grant");
    assert_eq!(
        granted.json_value(),
        json!({ "is_admin": true }),
        "one key, the group that moved"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "the column moved"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_moderator FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the other group stayed"
    );

    let now_admin = TestRequest::get(USERS)
        .bearer(&mint(&seeded.pool, &plain, "admin:read:accounts").await)
        .send(&seeded.app)
        .await;
    common::assert_answer(&now_admin, StatusCode::OK, "the grant took effect at once");

    let shown = read(&seeded.app, &seeded.token, &show("plain")).await;
    assert_eq!(
        shown["roles"],
        json!({ "admin": true, "moderator": false }),
        "and the admin entity says so"
    );

    let revoked = delete_json(&seeded.app, &token, &group_of("plain", "admin"), None).await;
    common::assert_answer(&revoked, StatusCode::OK, "the revoke");
    assert_eq!(
        revoked.json_value(),
        json!({ "is_admin": false }),
        "the same one key, cleared"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the column went back"
    );

    let (grant, _) = one_entry(&seeded.pool, "grant").await;
    assert_eq!(
        grant, "@root made @plain admin",
        "one entry per write, Pleroma's sentence with the group it named"
    );
    let (revoke, _) = one_entry(&seeded.pool, "revoke").await;
    assert_eq!(
        revoke, "@root revoked admin role from @plain",
        "and the revoke's own wording"
    );
}

/// The other group of the pair writes under its own key, touches nothing
/// else on the row, and names its permission in the logged data map; a
/// handle nobody answers to is the same 404 the rest of the module gives.
#[tokio::test]
async fn the_moderator_group_writes_under_its_own_key() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let modded = post_json(&seeded.app, &token, &group_of("plain", "moderator"), None).await;
    assert_eq!(
        modded.json_value(),
        json!({ "is_moderator": true }),
        "the other group answers under its own key"
    );

    let revoked = delete_json(&seeded.app, &token, &group_of("plain", "moderator"), None).await;
    assert_eq!(
        revoked.json_value(),
        json!({ "is_moderator": false }),
        "the same one key, cleared"
    );
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        Vec::<String>::new(),
        "a role write touches no other column"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the group it was not asked for stays where it stood"
    );

    let grants = logged(&seeded.pool, "grant").await;
    assert_eq!(
        grants
            .iter()
            .map(|(message, _)| message.as_str())
            .collect::<Vec<_>>(),
        vec!["@root made @plain moderator"],
        "the entry names the group that moved"
    );
    assert_eq!(
        grants[0].1["permission"],
        json!("moderator"),
        "the data map carries the permission"
    );
    assert_eq!(
        grants[0].1["subject"]
            .as_array()
            .expect("the subject list")
            .iter()
            .filter_map(|found| found["nickname"].as_str())
            .collect::<Vec<_>>(),
        vec!["plain"],
        "and the subject map names the account"
    );

    for path in [group_of("ghost", "admin"), group_of("ghost", "moderator")] {
        let miss = if path.ends_with("admin") {
            post_json(&seeded.app, &token, &path, None).await
        } else {
            delete_json(&seeded.app, &token, &path, None).await
        };
        common::assert_answer(&miss, StatusCode::NOT_FOUND, "a name nobody answers to");
        common::assert_error(&miss, NOT_FOUND, "the module's 404");
    }
}
