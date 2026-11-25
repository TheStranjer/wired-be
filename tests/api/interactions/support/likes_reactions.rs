//! Delivered likes and emoji reactions, and their undos.

use super::*;

// --- Like -----------------------------------------------------------------------

#[tokio::test]
async fn a_delivered_like_folds_a_favourite_a_count_and_a_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "a cat photo").await;

    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;

    let like_id = interaction_id("Like", BOB, 1);
    assert_eq!(
        favourite_rows(&pool).await,
        vec![(BOB.to_owned(), posted.uri.clone(), like_id.clone())],
        "one favourite keyed to the delivered pair and its activity id"
    );
    let seen = viewer(&pool, &alice).await;
    assert_eq!(seen.status(posted.id).await["favourites_count"], json!(1));
    let told = seen.told_of(TYPE_FAVOURITE).await;
    assert_eq!(told.len(), 1, "the author is told exactly once");
    assert_eq!(told[0]["account"]["id"], json!(bob.id.to_string()));
    assert_eq!(told[0]["status"]["id"], json!(posted.id.to_string()));
    assert_eq!(told[0]["status"]["uri"], json!(posted.uri));
}

#[tokio::test]
async fn a_restated_like_folds_one_row_and_tells_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "twice over").await;

    // The same pair restated under a fresh activity id marks nothing twice:
    // the pair, not the id, is what the row is keyed on.
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 2, Value::Null),
        &bob,
    )
    .await;

    assert_eq!(favourite_rows(&pool).await.len(), 1);
    let seen = viewer(&pool, &alice).await;
    assert_eq!(
        seen.status(posted.id).await["favourites_count"],
        json!(1),
        "the count follows the row, not the deliveries"
    );
    assert_eq!(seen.told_of(TYPE_FAVOURITE).await.len(), 1);
}

#[tokio::test]
async fn an_undo_of_a_like_takes_back_row_count_and_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "here today").await;
    let like_id = interaction_id("Like", BOB, 1);

    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;
    // Mastodon peers undo with the id alone, which is enough: the row
    // carries it. A second undo of the empty id completes and refuses.
    deliver(&state, &undo(BOB, &json!(like_id.clone()), 2), &bob).await;
    let seen = viewer(&pool, &alice).await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "the undo emptied the table"
    );
    assert_eq!(seen.status(posted.id).await["favourites_count"], json!(0));
    assert!(
        seen.told_of(TYPE_FAVOURITE).await.is_empty(),
        "the telling is gone with the row"
    );

    deliver(&state, &undo(BOB, &json!(like_id), 3), &bob).await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "the second undo undoes nothing"
    );

    // A re-delivery after the undo folds again: the withdrawal freed the
    // pair, unlike a tombstoned boost id.
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 4, Value::Null),
        &bob,
    )
    .await;
    assert_eq!(favourite_rows(&pool).await.len(), 1);
    assert_eq!(seen.status(posted.id).await["favourites_count"], json!(1));
}

#[tokio::test]
async fn an_undo_carrying_the_inner_like_takes_back_the_same_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "nested undo").await;

    let like = interaction("Like", BOB, &target_of(&posted), 1, Value::Null);
    deliver(&state, &like, &bob).await;
    deliver(&state, &undo(BOB, &like, 2), &bob).await;

    assert!(
        favourite_rows(&pool).await.is_empty(),
        "the embedded inner is unwrapped and routed by its type"
    );
    assert_eq!(
        viewer(&pool, &alice).await.status(posted.id).await["favourites_count"],
        json!(0)
    );
}

#[tokio::test]
async fn an_undo_signed_by_another_actor_withdraws_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (_alice, posted) = alice_post(&pool, "not yours to undo").await;

    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;

    // Mallory undoes bob's like by its id: the row is keyed per actor, so
    // bob's favourite stands.
    let like_id = interaction_id("Like", BOB, 1);
    deliver(&state, &undo(MALLORY, &json!(like_id.clone()), 2), &mallory).await;
    // ...and wrapped in an inner naming bob: refused at the dispatcher's
    // own gate before the handler ever runs.
    deliver(
        &state,
        &undo(
            MALLORY,
            &interaction("Like", BOB, &target_of(&posted), 3, Value::Null),
            4,
        ),
        &mallory,
    )
    .await;
    assert_eq!(
        favourite_rows(&pool).await,
        vec![(BOB.to_owned(), posted.uri.clone(), like_id)],
        "bob's like survives both attempts"
    );
}

// --- EmojiReact ------------------------------------------------------------------

#[tokio::test]
async fn a_delivered_unicode_reaction_lands_with_its_telling_and_roll_up() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "himalayan cat").await;

    deliver(
        &state,
        &interaction(
            "EmojiReact",
            BOB,
            &target_of(&posted),
            1,
            json!({"content": "😺"}),
        ),
        &bob,
    )
    .await;

    assert_eq!(
        reaction_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            posted.uri.clone(),
            "😺".to_owned(),
            None,
            interaction_id("EmojiReact", BOB, 1),
        )],
        "one bare-emoji row, no url, keyed to the activity"
    );
    let seen = viewer(&pool, &alice).await;
    let status = seen.status(posted.id).await;
    assert_eq!(status["pleroma"]["emoji_reactions"][0]["count"], json!(1));
    assert_eq!(status["pleroma"]["emoji_reactions"][0]["name"], json!("😺"));
    let told = seen.told_of(TYPE_EMOJI_REACTION).await;
    assert_eq!(told.len(), 1, "the author is told once");
    assert_eq!(told[0]["account"]["id"], json!(bob.id.to_string()));
    assert_eq!(told[0]["emoji"], json!("😺"), "and told with what");
}

#[tokio::test]
async fn a_shortcode_reaction_needs_its_emoji_tag_and_bad_content_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "custom emoji please").await;

    // Refused content: a bare shortcode, two emojis, a shortcode whose tag
    // names another one, and an icon url no http(s) fetch could serve.
    for (n, extra) in [
        (1, json!({"content": ":blobcat:"})),
        (2, json!({"content": "😺😹"})),
        (
            3,
            json!({"content": ":blobcat:", "tag": [{"type": "Emoji", "name": ":other:", "icon": {"url": "https://remote.example/e.png"}}]}),
        ),
        (
            4,
            json!({"content": ":blobcat:", "tag": [{"type": "Emoji", "name": ":blobcat:", "icon": {"url": "ftp://remote.example/e.png"}}]}),
        ),
    ] {
        deliver(
            &state,
            &interaction("EmojiReact", BOB, &target_of(&posted), n, extra),
            &bob,
        )
        .await;
    }
    assert!(
        reaction_rows(&pool).await.is_empty(),
        "content the local rules refuse stores nothing"
    );

    let tagged = json!({
        "content": ":blobcat:",
        "tag": [{"type": "Emoji", "name": ":blobcat:", "icon": {"type": "Image", "mediaType": "image/png", "url": "https://remote.example/emoji/blobcat.png"}}],
    });
    deliver(
        &state,
        &interaction("EmojiReact", BOB, &target_of(&posted), 5, tagged),
        &bob,
    )
    .await;
    assert_eq!(
        reaction_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            posted.uri.clone(),
            ":blobcat:".to_owned(),
            Some("https://remote.example/emoji/blobcat.png".to_owned()),
            interaction_id("EmojiReact", BOB, 5),
        )],
        "the activity's own emoji url is stored with the shortcode"
    );
    let status = viewer(&pool, &alice).await.status(posted.id).await;
    assert_eq!(
        status["pleroma"]["emoji_reactions"][0]["url"],
        json!("https://remote.example/emoji/blobcat.png"),
        "and the status json hands it to clients"
    );
}

#[tokio::test]
async fn an_undo_takes_back_one_emojis_reaction_and_its_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "react away").await;

    // Mallory parties and cats, bob parties: the undo of mallory's party
    // leaves the other two standing.
    for (actor, n, emoji) in [(BOB, 1, "🎉"), (MALLORY, 2, "🎉"), (MALLORY, 3, "😺")] {
        let who = if n == 1 { &bob } else { &mallory };
        deliver(
            &state,
            &interaction(
                "EmojiReact",
                actor,
                &target_of(&posted),
                n,
                json!({"content": emoji}),
            ),
            who,
        )
        .await;
    }
    assert_eq!(reaction_rows(&pool).await.len(), 3);

    let party_id = interaction_id("EmojiReact", MALLORY, 2);
    deliver(&state, &undo(MALLORY, &json!(party_id), 4), &mallory).await;

    let rows = reaction_rows(&pool).await;
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0.as_str(), rows[0].2.as_str()), (BOB, "🎉"));
    assert_eq!((rows[1].0.as_str(), rows[1].2.as_str()), (MALLORY, "😺"));
    let seen = viewer(&pool, &alice).await;
    let told = seen.told_of(TYPE_EMOJI_REACTION).await;
    assert_eq!(
        told.iter()
            .filter(|t| t["account"]["id"] == json!(mallory.id.to_string())
                && t["emoji"] == json!("🎉"))
            .count(),
        0,
        "mallory's withdrawn party is untold: {told:?}"
    );
    assert_eq!(told.len(), 2, "bob's party and mallory's cat still tell");
    let status = seen.status(posted.id).await;
    assert_eq!(
        status["pleroma"]["emoji_reactions"],
        json!([
            {"name": "🎉", "count": 1, "me": false, "url": null, "account_ids": [bob.id.to_string()]},
            {"name": "😺", "count": 1, "me": false, "url": null, "account_ids": [mallory.id.to_string()]},
        ]),
        "one party (bob's), one cat (mallory's), in first-reaction order"
    );
}

// --- U+FE0F variants --------------------------------------------------------------

/// Misskey's bare heart: U+2764 with no variation selector.
const BARE_HEART: &str = "\u{2764}";
/// The fully qualified heart Pleroma and Mastodon clients send.
const QUALIFIED_HEART: &str = "\u{2764}\u{FE0F}";
/// The Misskey-side reactor of the heart tests.
const CAROL: &str = "https://remote.example/users/carol";
/// A second Misskey-side reactor.
const DAVE: &str = "https://remote.example/users/dave";
/// The Pleroma-side reactor.
const FRANK: &str = "https://remote.example/users/frank";

#[tokio::test]
async fn a_misskey_heart_and_a_pleroma_heart_are_one_reaction_group() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let carol = common::create_remote_user(&pool, "carol", "remote.example").await;
    let dave = common::create_remote_user(&pool, "dave", "remote.example").await;
    let frank = common::create_remote_user(&pool, "frank", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "a post worth a heart").await;

    // Misskey's `Like` carries the selector-free heart in `content`, Pleroma's
    // `EmojiReact` the fully qualified one.
    for (actor, who, n) in [(CAROL, &carol, 1), (DAVE, &dave, 2)] {
        let like = interaction(
            "Like",
            actor,
            &target_of(&posted),
            n,
            json!({"content": BARE_HEART, "_misskey_reaction": BARE_HEART}),
        );
        deliver(&state, &like, who).await;
    }
    let react = interaction(
        "EmojiReact",
        FRANK,
        &target_of(&posted),
        3,
        json!({"content": QUALIFIED_HEART}),
    );
    deliver(&state, &react, &frank).await;

    let rows = reaction_rows(&pool).await;
    assert_eq!(
        rows.iter().map(|row| row.2.as_str()).collect::<Vec<_>>(),
        vec![BARE_HEART; 3],
        "all three are stored under the one grouping key"
    );
    let seen = viewer(&pool, &alice).await;
    let status = seen.status(posted.id).await;
    assert_eq!(
        status["pleroma"]["emoji_reactions"],
        json!([{
            "name": QUALIFIED_HEART,
            "count": 3,
            "me": false,
            "url": null,
            "account_ids": [carol.id.to_string(), dave.id.to_string(), frank.id.to_string()],
        }]),
        "one chip, named fully qualified, counting all three"
    );
    let told = seen.told_of(TYPE_EMOJI_REACTION).await;
    assert!(
        told.iter()
            .all(|note| note["emoji"] == json!(QUALIFIED_HEART)),
        "every telling names the heart fully qualified: {told:?}"
    );

    // Misskey takes its Like back: the merged group loses that one reactor.
    let like_id = interaction_id("Like", CAROL, 1);
    deliver(&state, &undo(CAROL, &json!(like_id), 4), &carol).await;

    let after_undo = seen.status(posted.id).await;
    assert_eq!(
        after_undo["pleroma"]["emoji_reactions"],
        json!([{
            "name": QUALIFIED_HEART,
            "count": 2,
            "me": false,
            "url": null,
            "account_ids": [dave.id.to_string(), frank.id.to_string()],
        }]),
        "the undo of the bare heart leaves the other two in the group"
    );
    assert_eq!(
        seen.told_of(TYPE_EMOJI_REACTION).await.len(),
        2,
        "and its telling is withdrawn"
    );
}
