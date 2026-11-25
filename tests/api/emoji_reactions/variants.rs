//! Unicode reactions that differ only by U+FE0F are one reaction.

use serde_json::json;
use wired_be::models::emoji_reaction::{self, NewReaction};

use super::support::{
    field, listed, names, public_post, react_ok, reacted, reader_and_writer, stored, summary,
    withdraw_ok,
};
use crate::common;

/// Misskey's bare heart: U+2764 with no variation selector.
const BARE_HEART: &str = "\u{2764}";
/// The fully qualified heart Mastodon and Pleroma clients send.
const QUALIFIED_HEART: &str = "\u{2764}\u{FE0F}";
/// A ZWJ sequence with a skin tone: a woman technologist.
const TECHNOLOGIST: &str = "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}";
/// The rainbow flag, a ZWJ sequence that keeps its U+FE0F.
const RAINBOW: &str = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}";

#[tokio::test]
async fn a_local_heart_joins_a_remote_one_and_either_spelling_withdraws_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let carol = common::create_remote_user(&pool, "carol", "misskey.test").await;
    let status_id = public_post(&pool, "bob").await;

    let _remote = emoji_reaction::react_with_ap_id(
        &pool,
        &NewReaction {
            user_id: carol.id,
            status_id,
            emoji: QUALIFIED_HEART,
            url: None,
        },
        "https://misskey.test/activities/like-1",
    )
    .await
    .expect("the remote reaction stores");
    let body = reacted(&app, &token, &status_id, BARE_HEART).await;

    assert_eq!(
        body["pleroma"]["emoji_reactions"],
        json!([summary(
            QUALIFIED_HEART,
            2,
            true,
            &json!(null),
            &[&carol.id, &alice.id]
        )]),
        "one heart chip, named fully qualified, counting both"
    );
    assert_eq!(
        stored(&pool)
            .await
            .into_iter()
            .map(|row| row.2)
            .collect::<Vec<_>>(),
        vec![BARE_HEART, BARE_HEART],
        "both rows hold the grouping key"
    );

    withdraw_ok(&app, &token, &status_id.to_string(), QUALIFIED_HEART).await;
    assert_eq!(
        field(&app, Some(&token), &status_id).await,
        json!([summary(
            QUALIFIED_HEART,
            1,
            false,
            &json!(null),
            &[&carol.id]
        )]),
        "the qualified DELETE removed the local one and left the remote one"
    );
}

#[tokio::test]
async fn reacting_in_both_spellings_is_one_reaction_and_both_read_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    react_ok(&app, &token, &status_id, QUALIFIED_HEART).await;
    react_ok(&app, &token, &status_id, BARE_HEART).await;

    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            BARE_HEART.to_owned()
        )],
        "one row under the grouping key"
    );
    for wanted in [BARE_HEART, QUALIFIED_HEART] {
        let body = listed(&app, Some(&token), &status_id, Some(wanted)).await;
        assert_eq!(
            names(&body),
            vec![QUALIFIED_HEART],
            "the read narrowed by {wanted:?} names the fully qualified heart"
        );
        assert_eq!(body[0]["count"], json!(1), "counting the one reactor");
    }
    assert_eq!(
        names(&listed(&app, None, &status_id, None).await),
        vec![QUALIFIED_HEART],
        "and so does the unfiltered read"
    );
}

#[tokio::test]
async fn zwj_sequences_are_stored_and_grouped_exactly() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    react_ok(&app, &token, &status_id, TECHNOLOGIST).await;
    react_ok(&app, &token, &status_id, RAINBOW).await;

    let mut kept: Vec<String> = stored(&pool).await.into_iter().map(|row| row.2).collect();
    kept.sort_unstable();
    let mut wanted = vec![TECHNOLOGIST.to_owned(), RAINBOW.to_owned()];
    wanted.sort_unstable();
    assert_eq!(kept, wanted, "no selector of a ZWJ sequence is touched");
    let status = field(&app, Some(&token), &status_id).await;
    let mut shown = names(&status);
    shown.sort_unstable();
    assert_eq!(shown, wanted, "and each is named as it was sent");

    withdraw_ok(&app, &token, &status_id.to_string(), RAINBOW).await;
    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            TECHNOLOGIST.to_owned()
        )],
        "the exact spelling withdraws exactly that sequence"
    );
}
