//! The profile-tag pair of WBE-197: `PUT /users/tag` appending the
//! submitted tags to every named account — lowercased, deduped, the order
//! kept — `DELETE /users/tag` taking exactly the named ones back, both
//! reading their parameters from JSON, the query string and a form body
//! alike, and skipping every handle nobody answers to inside the 204.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{
    TAG, account_named, assert_no_content, delete_json, logged, plain, put_json, read, seed, show,
    tags_of, write_token,
};
use crate::common::{self, TestRequest};

// ===== WBE-197: the profile tags, the permission groups, the pool =====

/// `PUT /users/tag` appends the submitted tags to every named account —
/// lowercased and deduped Pleroma-style, stored tags and their order kept —
/// and the write is logged whether or not it changed anything.
#[tokio::test]
async fn the_tag_write_adds_the_submitted_tags() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let beeper = account_named(&seeded.pool, "beeper").await;
    let body = json!({"nicknames": ["plain", "beeper"], "tags": ["staff", "VIP", "staff"]});

    let added = put_json(&seeded.app, &token, TAG, Some(&body)).await;
    assert_no_content(&added, "the tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff", "vip"],
        "the submission is lowercased, its duplicate dropped, the order kept"
    );
    assert_eq!(
        tags_of(&seeded.pool, &beeper).await,
        vec!["bots", "staff", "vip"],
        "the stored tag leads and the new ones follow"
    );

    let again = put_json(&seeded.app, &token, TAG, Some(&body)).await;
    assert_no_content(&again, "the repeated tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff", "vip"],
        "tagging the held tags again changes nothing"
    );

    let shown = read(&seeded.app, &seeded.token, &show("plain")).await;
    assert_eq!(
        shown["tags"],
        json!(["staff", "vip"]),
        "and the admin entity reads the new list back"
    );

    let entries = logged(&seeded.pool, "tag").await;
    assert_eq!(
        entries.len(),
        2,
        "one entry per tag write, even the one that changed nothing"
    );
    assert_eq!(
        entries[0].0, "@root added tags: staff, VIP, staff to users: @plain, @beeper",
        "the sentence names the accounts found and the tags as they were spelled"
    );
    assert_eq!(
        entries[0].1["tags"],
        json!(["staff", "VIP", "staff"]),
        "the data map carries the raw submission, Pleroma-style"
    );
}

/// `DELETE /users/tag` takes exactly the named tags back out of the named
/// accounts, leaves every other tag and account alone, is idempotent, and
/// lands its own sentence.
#[tokio::test]
async fn the_untag_write_takes_only_the_named_tags() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let beeper = account_named(&seeded.pool, "beeper").await;
    let setup = put_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["plain", "beeper"], "tags": ["staff", "vip"]})),
    )
    .await;
    assert_no_content(&setup, "the write this test undoes");

    let held = json!({"nicknames": ["plain"], "tags": ["staff"]});
    let removed = delete_json(&seeded.app, &token, TAG, Some(&held)).await;
    assert_no_content(&removed, "the untag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["vip"],
        "only the named tag went"
    );
    assert_eq!(
        tags_of(&seeded.pool, &beeper).await,
        vec!["bots", "staff", "vip"],
        "and the account nobody named kept every tag"
    );

    let again = delete_json(&seeded.app, &token, TAG, Some(&held)).await;
    assert_no_content(&again, "the repeated untag");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["vip"],
        "removing an absent tag is no error and no change"
    );

    let entries = logged(&seeded.pool, "untag").await;
    assert_eq!(
        entries[0].0, "@root removed tags: staff from users: @plain",
        "and the pair's own sentence"
    );
}

/// Both tag routes read their parameters from the query string and from a
/// form body as well as from JSON, an MRF tag is an ordinary accepted tag,
/// and a request that states only one of the two lists is the 422.
#[tokio::test]
async fn the_tag_routes_read_their_parameters_from_every_spelling() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let via_query = put_json(
        &seeded.app,
        &token,
        &format!("{TAG}?nicknames[]=plain&tags[]=mrf_tag%3Asandbox&tags[]=ops"),
        None,
    )
    .await;
    assert_no_content(&via_query, "the query-string tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["mrf_tag:sandbox", "ops"],
        "a known MRF tag lands as the switch spells it, and the list came from the query"
    );

    let missing_tags = delete_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["plain"]})),
    )
    .await;
    common::assert_answer(&missing_tags, StatusCode::UNPROCESSABLE_ENTITY, "no `tags`");
    let via_form = TestRequest::put(TAG)
        .bearer(&token)
        .form(&[("nicknames[]", "plain"), ("tags[]", "OPS")])
        .send(&seeded.app)
        .await;
    assert_no_content(&via_form, "the form-body tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["mrf_tag:sandbox", "ops"],
        "the bracketed form key nests, and the fold leaves `ops` as it already stood"
    );

    let half = put_json(&seeded.app, &token, TAG, Some(&json!({"tags": ["x"]}))).await;
    common::assert_answer(
        &half,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a tag write with no nicknames",
    );
    assert!(
        half.text().contains("missing field `nicknames`"),
        "and the 422 names the key it needs: {}",
        half.text()
    );
    let bare = put_json(&seeded.app, &token, TAG, Some(&json!({}))).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a tag write with neither list",
    );
    let bare_delete = delete_json(&seeded.app, &token, TAG, Some(&json!({"nicknames": []}))).await;
    common::assert_answer(
        &bare_delete,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an untag with no tags",
    );
    assert_eq!(
        logged(&seeded.pool, "tag").await.len(),
        2,
        "the two accepted writes above logged, and the three refusals logged nothing"
    );
    assert!(
        logged(&seeded.pool, "untag").await.is_empty(),
        "and no refused request left an entry behind"
    );
}

/// The tag routes skip the handles nobody answers to — inside the same 204
/// — and a tag list naming nothing writes nothing while the entry still
/// names every account that was found.
#[tokio::test]
async fn the_tag_routes_skip_what_they_cannot_resolve() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let ghosted = put_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["ghost", "plain"], "tags": ["staff"]})),
    )
    .await;
    assert_no_content(&ghosted, "a list with one unknown handle");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff"],
        "the known one was written"
    );

    let empty = delete_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["plain", "ghost"], "tags": []})),
    )
    .await;
    assert_no_content(&empty, "a tag list with nothing in it");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff"],
        "and nothing was subtracted"
    );
    let entries = logged(&seeded.pool, "untag").await;
    assert_eq!(entries.len(), 1, "the write logged all the same");
    assert_eq!(
        entries[0].0, "@root removed tags:  from users: @plain",
        "Pleroma's clause spells the empty tag list as it reads it"
    );
    assert_eq!(
        entries[0].1["nicknames"],
        json!(["plain"]),
        "the unknown handle is no subject"
    );
}
