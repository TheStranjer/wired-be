//! Unit tests for the nested renders: a reblog nesting its original, a
//! quote nesting its quoted status one level deep behind the batch's
//! visibility read, and the admin view that lifts that read.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    ViewerMode, render_status,
    test_support::{BASE, post, test_state, user},
};
use crate::models::{
    status::{NewStatus, Status as StatusRow, Visibility, insert},
    user::User,
};

/// Quotes `original` as `author` at `visibility`, straight through the
/// model — the batch tests' quote fixture.
async fn quote(
    pool: &PgPool,
    author: &User,
    original: &StatusRow,
    visibility: Visibility,
) -> StatusRow {
    insert(
        pool,
        BASE,
        &NewStatus {
            visibility,
            quote_of_id: Some(original.id),
            ..NewStatus::local(author, "quoting this")
        },
    )
    .await
    .expect("a quote of a stored status inserts")
}

/// A followers-only post by `author`, straight through the model — the
/// quoted original an anonymous viewer may not reach.
async fn private_post(pool: &PgPool, author: &User) -> StatusRow {
    insert(
        pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Private,
            ..NewStatus::local(author, "followers only")
        },
    )
    .await
    .expect("a private post inserts")
}

/// The quoting ticket's nested render inside the fixed query set: the
/// batch's one visibility read decides which quoted originals render,
/// a quote nests exactly one level deep, an invisible one leaves
/// `quote` null with `quote_visible` false while `quote_id` still
/// names the target, and the quote's author rides the page's batch.
#[tokio::test]
async fn the_quote_nests_from_the_batch_with_its_visibility_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let source = post(&pool, &bob, "the source").await;
    let public_quote = quote(&pool, &carol, &source, Visibility::Public).await;
    let secret = private_post(&pool, &alice).await;
    // C ← B (alice's private quote) ← A (carol's public quote of B):
    // the depth chain, seeded past the endpoint's quotability rule.
    let deep = quote(&pool, &alice, &secret, Visibility::Private).await;
    let chain = quote(&pool, &carol, &deep, Visibility::Public).await;

    let anonymous = render_status(&state, ViewerMode::ANONYMOUS, &public_quote)
        .await
        .expect("the public quote renders")
        .expect("a live public row renders");
    let open = serde_json::to_value(&anonymous).expect("it serializes");
    assert_eq!(
        open["quote_id"],
        source.id.to_string(),
        "the hyphenated id of the quoted row (decision 4)"
    );
    assert_eq!(open["quote"]["state"], "accepted");
    let quoted = &open["quote"]["quoted_status"];
    assert_eq!(quoted["id"], source.id.to_string());
    assert_eq!(
        quoted["content"], "<p>the source</p>",
        "the nested entity is the quoted row's own render"
    );
    assert_eq!(
        quoted["account"]["acct"], "bob",
        "the quote's author rode the page's user read"
    );
    assert_eq!(
        open["pleroma"]["quote"], *quoted,
        "the pleroma mirror is the bare quoted status"
    );
    assert_eq!(open["pleroma"]["quote_visible"], json!(true));

    let for_owner = render_status(&state, ViewerMode::user(&alice), &chain)
        .await
        .expect("the chain renders for alice")
        .expect("and answers");
    let owned = serde_json::to_value(&for_owner).expect("it serializes");
    let owned_quoted = &owned["quote"]["quoted_status"];
    assert_eq!(
        owned_quoted["id"],
        deep.id.to_string(),
        "alice sees her own private quote nested even though it is private"
    );
    assert_eq!(
        owned_quoted["quote"],
        json!({ "state": "accepted", "quoted_status_id": secret.id.to_string() }),
        "depth 1: the quoted status's own quote is only a reference"
    );
    assert_eq!(owned_quoted["quote_id"], secret.id.to_string());
    assert_eq!(owned_quoted["pleroma"]["quote_visible"], json!(false));

    let anonymous_chain = render_status(&state, ViewerMode::ANONYMOUS, &chain)
        .await
        .expect("the chain renders anonymously")
        .expect("and answers");
    let quiet = serde_json::to_value(&anonymous_chain).expect("it serializes");
    assert_eq!(
        quiet["quote_id"],
        deep.id.to_string(),
        "the id names the invisible quote all the same"
    );
    assert_eq!(
        quiet["quote"],
        json!({ "state": "unauthorized", "quoted_status": null }),
        "alice's private row is invisible to the anonymous viewer"
    );
    assert_eq!(quiet["pleroma"]["quote"], Value::Null);
    assert_eq!(quiet["pleroma"]["quote_visible"], json!(false));
}

/// The one lift [`ViewerMode::Admin`] buys, on the renderer's own
/// surface: a quoted original its viewer cannot reach still nests for
/// staff, where the anonymous view — and any [`ViewerMode::User`] —
/// leaves it null with `quote_visible` false. The post's own render is
/// the standard entity either way: the mode adds no admin key, exactly
/// as Pleroma's `AdminAPI.StatusView` delegates to `MastodonAPI.StatusView`.
#[tokio::test]
async fn the_admin_view_nests_a_quote_its_viewer_cannot_reach() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let root = user(&pool, "root").await;
    let secret = private_post(&pool, &alice).await;
    let quoting = quote(&pool, &alice, &secret, Visibility::Public).await;

    let anonymous_row = render_status(&state, ViewerMode::ANONYMOUS, &quoting)
        .await
        .expect("the quoting row renders")
        .expect("a live public row renders");
    let anonymous = serde_json::to_value(&anonymous_row).expect("it serializes");
    assert_eq!(
        anonymous["quote"],
        json!({ "state": "unauthorized", "quoted_status": null }),
        "an anonymous viewer may not reach alice's private quote"
    );
    assert_eq!(anonymous["pleroma"]["quote_visible"], json!(false));

    let staff_row = render_status(&state, ViewerMode::admin(&root), &quoting)
        .await
        .expect("the same row renders for staff")
        .expect("and answers");
    let seen = serde_json::to_value(&staff_row).expect("it serializes");
    assert_eq!(
        seen["quote"]["quoted_status"]["id"],
        secret.id.to_string(),
        "the admin view admits the private quote and nests it"
    );
    assert_eq!(seen["pleroma"]["quote_visible"], json!(true));
    assert_eq!(
        seen["visibility"], anonymous["visibility"],
        "the outer entity is the same standard Status in both views"
    );
}

#[tokio::test]
async fn a_reblog_nests_the_original_and_keeps_its_own_identity() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = insert(
        &pool,
        BASE,
        &NewStatus {
            spoiler_text: "cats".to_owned(),
            sensitive: true,
            ..NewStatus::local(&alice, "original text")
        },
    )
    .await
    .unwrap();
    let reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .unwrap();

    let entity = render_status(&state, ViewerMode::ANONYMOUS, &reblog)
        .await
        .unwrap()
        .expect("a reblog of a live original renders");
    let value = serde_json::to_value(&entity).unwrap();

    assert_eq!(value["id"], reblog.id.to_string());
    assert_eq!(value["uri"], reblog.uri, "the reblog's own uri");
    assert_eq!(value["url"], json!(reblog.url), "and its own url");
    assert_eq!(value["content"], "", "the outer content is empty");
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": "" }),
        "a reblog has no source text of its own"
    );
    assert_eq!(value["account"]["acct"], "bob", "the reblogger speaks");
    let nested = &value["reblog"];
    assert_eq!(nested["id"], original.id.to_string());
    assert_eq!(nested["uri"], original.uri);
    assert_eq!(nested["content"], "<p>original text</p>");
    assert_eq!(nested["account"]["acct"], "alice");
    assert_eq!(nested["sensitive"], true, "the original keeps its flags");
    assert_eq!(nested["spoiler_text"], "cats");
    assert_eq!(
        nested["reblog"],
        Value::Null,
        "one level deep, Pleroma's non-recursive Announce arm"
    );

    assert_eq!(
        value["reblogs_count"], 0,
        "the reblog row's own counter, not the original's"
    );
    let counted: (i32,) = sqlx::query_as("SELECT reblogs_count FROM statuses WHERE id = $1")
        .bind(original.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(counted.0, 1, "the original is the one counted");
}
