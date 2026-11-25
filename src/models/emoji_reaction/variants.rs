//! Tests of the grouping key: unicode reactions that differ only by U+FE0F
//! are one reaction, and ZWJ sequences are kept exactly.

use super::{
    NewReaction,
    delete::unreact,
    read::reactors,
    test_support::{BASE, HEART, post_by, react_with, rollup, stored, summary, user},
    write::react,
};

/// Misskey's heart: U+2764 with no variation selector.
const BARE_HEART: &str = "\u{2764}";
/// A ZWJ sequence that carries a U+FE0F.
const RAINBOW: &str = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}";
/// The same flag with the selector left out, which is a different sequence.
const RAINBOW_NO_SELECTOR: &str = "\u{1F3F3}\u{200D}\u{1F308}";

#[tokio::test]
async fn both_spellings_of_a_heart_are_one_group_named_fully_qualified() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let status_id = post_by(&pool, "carol").await;

    react_with(&pool, &alice, status_id, BARE_HEART, None).await;
    react_with(&pool, &bob, status_id, HEART, None).await;

    assert_eq!(
        stored(&pool)
            .await
            .into_iter()
            .map(|row| row.2)
            .collect::<Vec<_>>(),
        vec![BARE_HEART, BARE_HEART],
        "both rows hold the grouping key"
    );
    assert_eq!(
        rollup(&pool, status_id, Some(&bob)).await,
        vec![summary(HEART, 2, true, None, &[alice.id, bob.id])],
        "one chip, named with emoji presentation, counting both"
    );
    for wanted in [BARE_HEART, HEART] {
        let groups = reactors(&pool, status_id, Some(wanted), None)
            .await
            .expect("the reactors read");
        assert_eq!(groups.len(), 1, "{wanted:?} finds the one group");
        assert_eq!(groups[0].name, HEART, "named fully qualified");
        assert_eq!(
            groups[0].reactor_ids,
            vec![alice.id, bob.id],
            "both reactors"
        );
    }
}

#[tokio::test]
async fn one_account_reacting_in_both_spellings_holds_one_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;
    let first = react(
        &pool,
        BASE,
        &NewReaction {
            user_id: alice.id,
            status_id,
            emoji: HEART,
            url: None,
        },
    )
    .await
    .expect("the qualified reaction stores");

    react_with(&pool, &alice, status_id, BARE_HEART, None).await;

    let rows = stored(&pool).await;
    assert_eq!(rows.len(), 1, "the second spelling is the same reaction");
    assert_eq!(rows[0].2, BARE_HEART, "stored under the grouping key");
    assert_eq!(
        first.emoji, BARE_HEART,
        "the row the first write returned already holds the key"
    );
}

#[tokio::test]
async fn withdrawing_in_either_spelling_removes_the_reaction() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;

    for (reacted, withdrawn) in [(BARE_HEART, HEART), (HEART, BARE_HEART)] {
        react_with(&pool, &alice, status_id, reacted, None).await;
        let removed = unreact(&pool, alice.id, status_id, withdrawn)
            .await
            .expect("the withdrawal runs");
        assert!(
            removed.is_some(),
            "{withdrawn:?} withdraws a reaction made as {reacted:?}"
        );
        assert!(stored(&pool).await.is_empty(), "nothing is left");
    }
}

#[tokio::test]
async fn a_zwj_sequence_is_kept_and_its_selectorless_twin_is_another_emoji() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;

    react_with(&pool, &alice, status_id, RAINBOW, None).await;
    react_with(&pool, &alice, status_id, RAINBOW_NO_SELECTOR, None).await;

    let mut kept: Vec<String> = stored(&pool).await.into_iter().map(|row| row.2).collect();
    kept.sort_unstable();
    let mut wanted = vec![RAINBOW.to_owned(), RAINBOW_NO_SELECTOR.to_owned()];
    wanted.sort_unstable();
    assert_eq!(kept, wanted, "a ZWJ sequence is stored exactly as sent");
}
