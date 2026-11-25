//! The reactions read: listing, the single-emoji variant, ordering and the
//! blocked-reactor filter.

use serde_json::{Value, json};
use wired_be::{config::TEST_BASE_URL, models::block};

use super::support::{
    CAT, CAT_NAME, CAT_URL, PARTY, THUMB, emoji, entry_keys, field, known, listed, names,
    public_post, react_ok, reader_and_writer, shown, stamp, summary,
};
use crate::common;

#[tokio::test]
async fn the_reactions_read_lists_every_account_behind_each_emoji() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let status_id = public_post(&pool, "carol").await;
    emoji(&pool, "cat").await;

    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, CAT).await;

    let anonymous = listed(&app, None, &status_id, None).await;
    assert_eq!(
        entry_keys(&anonymous, 0),
        vec!["accounts", "count", "me", "name", "url"],
        "the endpoint render names accounts, not ids"
    );
    assert_eq!(
        (
            names(&anonymous),
            shown(&anonymous, 0),
            shown(&anonymous, 1)
        ),
        (
            vec![THUMB.to_owned(), CAT_NAME.to_owned()],
            vec![known(&alice), known(&bob)],
            vec![known(&bob)],
        ),
        "each emoji with its whole reactor list, in reaction order"
    );
    assert_eq!(
        (
            anonymous[0]["count"].as_i64(),
            anonymous[1]["count"].as_i64(),
        ),
        (Some(2), Some(1)),
        "counted per emoji"
    );
    assert_eq!(
        (anonymous[0]["url"].clone(), anonymous[0]["me"].as_bool(),),
        (Value::Null, Some(false)),
        "a unicode emoji has no image, and an anonymous viewer is never `me`"
    );
    assert_eq!(
        anonymous[1]["url"],
        json!(CAT_URL),
        "the custom emoji's image rides along"
    );

    let as_alice = listed(&app, Some(&alice_token), &status_id, None).await;
    assert_eq!(
        (as_alice[0]["me"].as_bool(), as_alice[1]["me"].as_bool()),
        (Some(true), Some(false)),
        "alice is `me` on the emoji she reacted with only"
    );
    assert_eq!(
        field(&app, Some(&alice_token), &status_id).await,
        json!([
            summary(THUMB, 2, true, &Value::Null, &[&alice.id, &bob.id]),
            summary(CAT_NAME, 1, false, &json!(CAT_URL), &[&bob.id]),
        ]),
        "and the status field of the same viewer says the same thing, by id"
    );
}

#[tokio::test]
async fn the_single_emoji_variant_answers_only_that_emoji() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &token, &status_id, THUMB).await;
    react_ok(&app, &token, &status_id, PARTY).await;

    let one = listed(&app, Some(&token), &status_id, Some(THUMB)).await;
    assert_eq!(
        (
            entry_keys(&one, 0),
            one[0]["name"].as_str(),
            one[0]["count"].as_i64(),
            one[0]["me"].as_bool(),
            one[0]["url"].clone(),
            shown(&one, 0),
        ),
        (
            vec!["accounts", "count", "me", "name", "url"],
            Some(THUMB),
            Some(1),
            Some(true),
            Value::Null,
            vec![known(&alice)],
        ),
        "the filtered read answers that one emoji, in the same shape"
    );
    for absent in [":nobody:", "not-an-emoji", CAT] {
        assert_eq!(
            listed(&app, None, &status_id, Some(absent)).await,
            json!([]),
            "{absent} names no reaction: the segment is a filter on a read, \
             not a validated write, so the answer is the empty array"
        );
    }
    assert_eq!(
        names(&listed(&app, None, &status_id, Some(PARTY)).await),
        vec![PARTY.to_owned()],
        "and the other emoji stays out of the answer"
    );
}

#[tokio::test]
async fn the_order_is_each_emoji_s_first_reaction_then_its_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let status_id = public_post(&pool, "carol").await;
    react_ok(&app, &alice_token, &status_id, PARTY).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;

    // alice's party was the first reaction of anything and her thumb the last:
    // a group is ordered by its earliest row, not by its newest.
    stamp(&pool, &status_id, &alice, PARTY, "2024-01-01T00:00:00.000Z").await;
    stamp(&pool, &status_id, &bob, THUMB, "2024-02-01T00:00:00.000Z").await;
    stamp(&pool, &status_id, &alice, THUMB, "2024-06-01T00:00:00.000Z").await;

    let body = listed(&app, None, &status_id, None).await;
    assert_eq!(
        names(&body),
        vec![PARTY.to_owned(), THUMB.to_owned()],
        "january's party leads although its group's newest reaction is June's"
    );
    assert_eq!(
        shown(&body, 1),
        vec![known(&bob), known(&alice)],
        "and inside a group the reactors are in their own reaction order"
    );

    // Two emojis that began at the same instant order by their text.
    let twin = public_post(&pool, "dave").await;
    react_ok(&app, &alice_token, &twin, PARTY).await;
    react_ok(&app, &alice_token, &twin, THUMB).await;
    for wanted in [PARTY, THUMB] {
        stamp(&pool, &twin, &alice, wanted, "2024-03-01T00:00:00.000Z").await;
    }
    let twinned = listed(&app, Some(&alice_token), &twin, None).await;
    assert_eq!(
        names(&twinned),
        vec![PARTY.to_owned(), THUMB.to_owned()],
        "\u{1F389} is the lower text, so the tie breaks towards the party \
         first"
    );
    assert_eq!(
        field(&app, Some(&alice_token), &twin).await[0]["name"],
        json!(PARTY),
        "the status field uses the same order"
    );
}

#[tokio::test]
async fn blocked_reactors_are_hidden_from_the_viewer_who_blocked_them() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let (carol, carol_token) = reader_and_writer(&pool, "carol").await;
    let status_id = public_post(&pool, "dave").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &carol_token, &status_id, PARTY).await;

    let refusal = block::block(&pool, TEST_BASE_URL, &carol, &alice)
        .await
        .expect("carol blocks alice");
    assert_eq!(refusal.blocked_id, alice.id, "the edge the read filters on");

    let as_carol = listed(&app, Some(&carol_token), &status_id, None).await;
    assert_eq!(
        (names(&as_carol), shown(&as_carol, 0)),
        (vec![THUMB.to_owned(), PARTY.to_owned()], vec![known(&bob)],),
        "alice drops out of bob's group, and carol's own emoji is untouched"
    );
    assert_eq!(as_carol[0]["count"], json!(1), "counted without her");
    assert_eq!(
        shown(&as_carol, 1),
        vec![known(&carol)],
        "a blocker still sees her own reaction"
    );
    assert_eq!(
        field(&app, Some(&carol_token), &status_id).await[0]["count"],
        json!(2),
        "the batched status field is the unfiltered roll-up: the ticket puts \
         the exclusion on the reactions endpoints, so Pleroma's status-view \
         filter waits for the renderer to preload relationships"
    );

    // An emoji whose only reactor is blocked leaves the array entirely.
    let alone = public_post(&pool, "erin").await;
    react_ok(&app, &alice_token, &alone, THUMB).await;
    assert_eq!(
        names(&listed(&app, Some(&alice_token), &alone, None).await),
        vec![THUMB.to_owned()],
        "alice sees her own reaction on the other post"
    );
    assert_eq!(
        listed(&app, Some(&carol_token), &alone, None).await,
        json!([]),
        "and carol does not see that emoji at all"
    );
    assert_eq!(
        listed(&app, Some(&bob_token), &status_id, None)
            .await
            .as_array()
            .map(Vec::len),
        Some(2),
        "another viewer's block list has nothing to do with this one"
    );
    assert_eq!(
        listed(&app, None, &status_id, None)
            .await
            .as_array()
            .map(Vec::len),
        Some(2),
        "and an anonymous read hides nobody"
    );
}
