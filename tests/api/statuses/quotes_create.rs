//! Quoting on the way in: a body's `quote_id` (and Pleroma's
//! `quoted_status_id` alias) naming a live, quotable target stores the quote
//! and steps the original's counter; everything else the target could be is
//! the shared 404 or the pinned refusal — and the render nests the quoted
//! original exactly one level deep, visibility respected.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    config::TEST_BASE_URL,
    models::status::{self, NewStatus, Visibility},
};

use super::support::{
    NOT_FOUND, SCOPES, STATUSES, UNKNOWN_ID, WRITE, WRITE_AND_READ, context, context_ids,
    created_row, deleted, direct_to, followed, posted, publish, quote_of, reblog_of, refused, show,
    stored, stored_count, user_with_token,
};
use crate::common;

/// 422 body for quoting a status the poster may see but may not quote —
/// somebody else's `private`, `direct` or `local` post.
const QUOTE_NOT_ALLOWED: &str = "You can only quote public or unlisted statuses";

/// A post naming a live, visible, quotable `quote_id` is stored as a quote
/// of the original, and the original's `pleroma.quotes_count` answers the
/// insert-time bump through the show route.
#[tokio::test]
async fn a_post_can_quote_a_public_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE_AND_READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let original = common::create_status(&pool, &bob, "quote me", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &token,
        json!({"status": "look at this", "quote_id": original.id.to_string()}),
    )
    .await;
    let row = created_row(&pool, &value).await;
    assert_eq!(
        row.quote_of_id,
        Some(original.id),
        "the quoting row carries the original's id"
    );
    assert_eq!(row.user_id, alice.id, "and the post is the caller's own");

    let shown = show(&app, Some(&token), &original.id.to_string()).await;
    common::assert_answer(&shown, StatusCode::OK, "the quoted original");
    assert_eq!(
        shown.json_value()["pleroma"]["quotes_count"],
        1,
        "status::insert stepped the original's counter with the row"
    );
}

/// Pleroma's older `quoted_status_id` key names the same target as
/// `quote_id`, from a JSON body and from a form body alike (decision 10).
#[tokio::test]
async fn the_quoted_status_id_alias_names_the_same_quote() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let original = common::create_status(&pool, &bob, "quote me", Visibility::Unlisted).await;
    let app = common::test_app(pool.clone());
    let id = original.id.to_string();

    let value = posted(
        &app,
        &token,
        json!({"status": "alias quote", "quoted_status_id": &id}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &value).await.quote_of_id,
        Some(original.id),
        "the alias is the same parameter under a different key"
    );

    let form = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "form quote"), ("quote_id", id.as_str())])
        .send(&app)
        .await;
    common::assert_answer(&form, StatusCode::OK, "a form body quoting a post");
    assert_eq!(
        created_row(&pool, &form.json_value()).await.quote_of_id,
        Some(original.id),
        "and the form spelling quotes it exactly the same way"
    );
}

/// A `quote_id` naming nothing the poster may address is the shared 404:
/// malformed text, an unknown id, a deleted target and a target the poster
/// cannot see are indistinguishable from each other, and none of them
/// posts.
#[tokio::test]
async fn an_unusable_quote_target_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let doomed = common::create_status(&pool, &bob, "doomed", Visibility::Public).await;
    deleted(&pool, &doomed).await;
    let secret = common::create_status(&pool, &carol, "not for you", Visibility::Private).await;
    let app = common::test_app(pool.clone());

    let doomed_id = doomed.id.to_string();
    let secret_id = secret.id.to_string();
    for id in [
        "not-an-id",
        UNKNOWN_ID,
        doomed_id.as_str(),
        secret_id.as_str(),
    ] {
        let response = publish(
            &app,
            Some(&token),
            json!({"status": "quoting nothing", "quote_id": id}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            &format!("quote target `{id}`"),
        );
        common::assert_error(&response, NOT_FOUND, &format!("quote target `{id}`"));
    }
    assert_eq!(
        stored_count(&pool).await,
        2,
        "the two fixtures are all the table holds: no refusal wrote a row"
    );
}

/// A target the poster may see but may not quote — somebody else's `private`
/// post, as an accepted follower, and a `direct` addressed to the poster —
/// is refused 422 in the pinned wording, where the poster's own `private`
/// post is quotable.
#[tokio::test]
async fn only_public_and_unlisted_posts_may_be_quoted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    followed(&pool, &alice, &bob).await;
    let private = common::create_status(&pool, &bob, "followers only", Visibility::Private).await;
    let dm = direct_to(&pool, &bob, &alice).await;
    let mine = common::create_status(&pool, &alice, "my own secret", Visibility::Private).await;
    let app = common::test_app(pool.clone());

    for target in [&private, &dm] {
        refused(
            &app,
            &token,
            json!({"status": "over my station", "quote_id": target.id.to_string()}),
            QUOTE_NOT_ALLOWED,
        )
        .await;
    }
    assert_eq!(stored_count(&pool).await, 3, "both 422s wrote nothing");

    let value = posted(
        &app,
        &token,
        json!({"status": "quoting myself", "quote_id": mine.id.to_string()}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &value).await.quote_of_id,
        Some(mine.id),
        "an author may quote their own private post"
    );
}

/// A `quote_id` naming a reblog row quotes through to the original it
/// boosts: the stored id is the original's, the original's counter steps,
/// and the reblog row stays uncounted.
#[tokio::test]
async fn quoting_a_reblog_stores_the_original_it_boosts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE_AND_READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let original = common::create_status(&pool, &carol, "the source", Visibility::Public).await;
    let boost = reblog_of(&pool, &bob, &original).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &token,
        json!({"status": "quoting a boost", "quote_id": boost.id.to_string()}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &value).await.quote_of_id,
        Some(original.id),
        "the quote links the original, not the boost row"
    );
    assert_eq!(
        stored(&pool, boost.id).await.quotes_count,
        0,
        "the reblog row counts no quotes of its own"
    );

    let shown = show(&app, Some(&token), &original.id.to_string()).await;
    common::assert_answer(&shown, StatusCode::OK, "the quoted original");
    assert_eq!(
        shown.json_value()["pleroma"]["quotes_count"],
        1,
        "the original takes the bump"
    );
}

/// The show route of a quoting status renders the quoted original nested in
/// `quote` — its own id, content and account — mirrored identically in
/// `pleroma.quote` with `pleroma.quote_visible` true, and names the target
/// in `quote_id`; the original's own entity carries the counter bump.
#[tokio::test]
async fn a_quoting_status_renders_the_quoted_original_nested() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let original = common::create_status(&pool, &bob, "quote me", Visibility::Public).await;
    let quote = quote_of(&pool, &alice, &original, Visibility::Public).await;
    let app = common::test_app(pool);

    let shown = show(&app, None, &quote.id.to_string()).await;
    common::assert_answer(&shown, StatusCode::OK, "the quoting status");
    let value = shown.json_value();
    assert_eq!(
        value["quote_id"],
        original.id.to_string(),
        "the quoted target's id (decision 4), visible one or not"
    );
    assert_eq!(
        value["quote"]["id"],
        original.id.to_string(),
        "the nested entity is the quoted row's own"
    );
    assert_eq!(
        value["quote"]["content"], "<p>quote me</p>",
        "rendered as the author wrote it"
    );
    assert_eq!(
        value["quote"]["account"]["acct"], "bob",
        "with the quoted row's own author"
    );
    assert_eq!(
        value["pleroma"]["quote"], value["quote"],
        "pleroma mirrors the same rendered object"
    );
    assert_eq!(
        value["pleroma"]["quote_visible"],
        json!(true),
        "and says the viewer may see it"
    );

    let original_shown = show(&app, None, &original.id.to_string()).await;
    common::assert_answer(&original_shown, StatusCode::OK, "the quoted original");
    assert_eq!(
        original_shown.json_value()["pleroma"]["quotes_count"],
        1,
        "the original counts the quote on its own entity"
    );
}

/// Depth limit, one nested render: A quotes B quotes C answers
/// `A.quote.quote == null` — a quoted status's own quote never renders,
/// though its `quote_id` still names its target and its `quote_visible`
/// is false — while B's own entity renders B.quote == C fully.
#[tokio::test]
async fn a_rendered_quote_nests_exactly_one_level_deep() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let c = common::create_status(&pool, &carol, "the source", Visibility::Public).await;
    let b = quote_of(&pool, &bob, &c, Visibility::Public).await;
    let a = quote_of(&pool, &alice, &b, Visibility::Public).await;
    let app = common::test_app(pool);

    let shown = show(&app, None, &a.id.to_string()).await;
    common::assert_answer(&shown, StatusCode::OK, "the outer quote");
    let value = shown.json_value();
    assert_eq!(value["quote"]["id"], b.id.to_string(), "A nests B");
    assert_eq!(
        value["quote"]["quote"],
        Value::Null,
        "and the nested render's own quote is forced null"
    );
    assert_eq!(
        value["quote"]["quote_id"],
        c.id.to_string(),
        "while its quote_id still names C"
    );
    assert_eq!(
        value["quote"]["pleroma"]["quote_visible"],
        json!(false),
        "the mirror's flag follows the null render"
    );

    let middle = show(&app, None, &b.id.to_string()).await;
    common::assert_answer(&middle, StatusCode::OK, "the middle quote");
    let middle_value = middle.json_value();
    assert_eq!(
        middle_value["quote"]["id"],
        c.id.to_string(),
        "B nests C fully"
    );
    assert_eq!(middle_value["quote"]["content"], "<p>the source</p>");
    assert_eq!(middle_value["quote"]["account"]["acct"], "carol");
    assert_eq!(middle_value["pleroma"]["quote_visible"], json!(true));
}

/// A quote of somebody else's `private` post (seeded straight through the
/// model — the write endpoint would have refused it): a viewer who may not
/// see the target renders `quote: null` with `pleroma.quote_visible` false
/// while `quote_id` still names it, and the target's own author always sees
/// it rendered.
#[tokio::test]
async fn an_invisible_quote_renders_null_and_keeps_its_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (carol, carol_token) = user_with_token(&pool, "carol", SCOPES).await;
    let secret = common::create_status(&pool, &carol, "followers only", Visibility::Private).await;
    let quote = quote_of(&pool, &alice, &secret, Visibility::Public).await;
    let app = common::test_app(pool);

    for (who, token) in [
        ("anonymous", None),
        ("a stranger", Some(alice_token.as_str())),
    ] {
        let shown = show(&app, token, &quote.id.to_string()).await;
        common::assert_answer(&shown, StatusCode::OK, &format!("the public quoter, {who}"));
        let value = shown.json_value();
        assert_eq!(
            value["quote_id"],
            secret.id.to_string(),
            "{who}: quote_id is emitted regardless of visibility"
        );
        assert_eq!(value["quote"], Value::Null, "{who}: the render is withheld");
        assert_eq!(
            value["pleroma"]["quote"],
            Value::Null,
            "{who}: the mirror too"
        );
        assert_eq!(
            value["pleroma"]["quote_visible"],
            json!(false),
            "{who}: false exactly when quote is null"
        );
    }

    let as_author = show(&app, Some(carol_token.as_str()), &quote.id.to_string()).await;
    common::assert_answer(
        &as_author,
        StatusCode::OK,
        "the quoter, seen by the quoted author",
    );
    let value = as_author.json_value();
    assert_eq!(
        value["quote"]["id"],
        secret.id.to_string(),
        "the author of the private original always sees their own quoted post"
    );
    assert_eq!(value["quote"]["visibility"], "private");
    assert_eq!(value["pleroma"]["quote_visible"], json!(true));
}

/// A context page whose thread carries a quoting reply renders the nested
/// quote from the same batch the page already runs: the descendants arm's
/// entry names the quoted target and carries its full render, with the
/// `pleroma` mirror beside it.
#[tokio::test]
async fn a_thread_page_renders_the_nested_quote_from_its_batch() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let source = common::create_status(&pool, &carol, "look at this", Visibility::Public).await;
    let quoter = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            in_reply_to_id: Some(root.id),
            in_reply_to_account_id: Some(alice.id),
            quote_of_id: Some(source.id),
            ..NewStatus::local(&bob, "a quoting reply")
        },
    )
    .await
    .expect("a quoting reply inserts");
    let app = common::test_app(pool);

    let shown = context(&app, None, &root.id.to_string()).await;
    common::assert_answer(&shown, StatusCode::OK, "the thread around the root");
    let body = shown.json_value();
    assert_eq!(context_ids(&body, "descendants"), [quoter.id.to_string()]);
    let entry = &body["descendants"][0];
    assert_eq!(
        entry["quote"]["id"],
        source.id.to_string(),
        "the page's own batch brought the quoted original along"
    );
    assert_eq!(entry["quote"]["content"], "<p>look at this</p>");
    assert_eq!(entry["quote"]["account"]["acct"], "carol");
    assert_eq!(
        entry["pleroma"]["quote"], entry["quote"],
        "the mirror rides the nested render too"
    );
    assert_eq!(entry["pleroma"]["quote_visible"], json!(true));
}
