//! Which image and origin a delivered reaction's custom emoji is stored
//! under: never the reactor's say-so about an emoji that is not its own.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::super::tests::{app_state, local_post, view};
use super::*;
use crate::{
    models::{
        custom_emoji::{self, NewCustomEmoji},
        emoji_reaction::for_statuses,
        status::Status,
        user::{Flag, NewUser, insert},
    },
    test_fixtures::local_user,
};

/// This instance's own image of `:wiredcat:`.
const WIRED_URL: &str = "https://test.example/emoji/wired/wiredcat.png";
/// `misskey.test`'s image of `:misskeycat:`.
const MISSKEY_URL: &str = "https://misskey.test/files/misskeycat.png";
/// An image an attacker would like shown.
const EVIL_URL: &str = "https://evil.example/emoji/shock.png";

/// A remote account `nickname` of `domain`.
async fn reactor(pool: &PgPool, nickname: &str, domain: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{domain}"),
            ap_id: format!("https://{domain}/users/{nickname}"),
            local: Flag::new(false),
            domain: Some(domain.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the reactor fixture inserts")
}

/// An `Emoji` tag for `code` with the optional `id` and the icon `url`.
fn tag(code: &str, id: Option<&str>, url: &str) -> Value {
    let mut tag = json!({
        "type": "Emoji",
        "name": format!(":{code}:"),
        "icon": {"type": "Image", "url": url},
    });
    if let Some(id) = id {
        tag["id"] = json!(id);
    }
    tag
}

/// Delivers `who`'s `EmojiReact` `n` of `:code:` with `tag` to `post`.
async fn deliver(
    state: &AppState,
    who: &User,
    post: &Status,
    (n, code): (&str, &str),
    tag: Value,
) -> Outcome {
    let react = view(
        "EmojiReact",
        n,
        &who.ap_id,
        json!(post.uri),
        json!({"content": format!(":{code}:"), "tag": [tag]}),
    );
    apply_emoji_react(state.clone(), react, who.clone())
        .await
        .expect("the fold does not fail")
}

/// Delivers a reaction that must fold.
async fn deliver_react(
    state: &AppState,
    who: &User,
    post: &Status,
    which: (&str, &str),
    tag: Value,
) {
    let outcome = deliver(state, who, post, which, tag).await;
    assert_eq!(outcome, Outcome::Handled, "the reaction folds");
}

/// `who`'s stored reactions as `(emoji, emoji_domain, url)`.
async fn stored(pool: &PgPool, who: &User) -> Vec<(String, Option<String>, Option<String>)> {
    sqlx::query_as("SELECT emoji, emoji_domain, url FROM emoji_reactions WHERE user_id = $1")
        .bind(who.id)
        .fetch_all(pool)
        .await
        .expect("the reactions read")
}

/// Every custom emoji row as `(shortcode, domain)`.
async fn emojis(pool: &PgPool) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT shortcode, domain FROM custom_emojis ORDER BY shortcode, domain")
        .fetch_all(pool)
        .await
        .expect("the emojis read")
}

/// The post's roll-up as `(name, count)`.
async fn rollup(pool: &PgPool, post: &Status) -> Vec<(String, i64)> {
    for_statuses(pool, &[post.id], None)
        .await
        .expect("the roll-up reads")
        .remove(&post.id)
        .unwrap_or_default()
        .into_iter()
        .map(|group| (group.name, group.count))
        .collect()
}

/// A claim of this instance's emoji shows the stored image, never the tag's.
#[tokio::test]
async fn a_claim_of_our_own_emoji_shows_our_stored_image() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let _local = custom_emoji::upsert(&pool, &NewCustomEmoji::local("wiredcat", WIRED_URL))
        .await
        .expect("the local emoji stores");
    let wanda = local_user(&pool, "wanda2").await;
    let _row = emoji_reaction::react(
        &pool,
        crate::config::TEST_BASE_URL,
        &NewReaction {
            user_id: wanda.id,
            status_id: post.id,
            emoji: ":wiredcat:",
            url: Some(WIRED_URL),
        },
    )
    .await
    .expect("the local reaction stores");
    let xena = reactor(&pool, "xena", "wired2.test").await;

    let spoof = tag(
        "wiredcat",
        Some("https://test.example/emoji/wiredcat"),
        EVIL_URL,
    );
    deliver_react(&state, &xena, &post, ("r1", "wiredcat"), spoof).await;

    assert_eq!(
        stored(&pool, &xena).await,
        vec![(":wiredcat:".to_owned(), None, Some(WIRED_URL.to_owned()))],
        "the local reaction, with the image we hold"
    );
    assert_eq!(
        emojis(&pool).await,
        vec![("wiredcat".to_owned(), None)],
        "no emoji row is added"
    );
    assert_eq!(
        rollup(&pool, &post).await,
        vec![("wiredcat".to_owned(), 2)],
        "it is one group with the local user's"
    );
}

/// A claim of our domain for a shortcode we do not hold, or hold disabled, is refused.
#[tokio::test]
async fn a_claim_of_an_unknown_or_disabled_local_emoji_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let off = NewCustomEmoji {
        disabled: true,
        ..NewCustomEmoji::local("offcat", WIRED_URL)
    };
    let _off = custom_emoji::upsert(&pool, &off)
        .await
        .expect("the disabled emoji stores");
    let xena = reactor(&pool, "xena", "wired2.test").await;

    for (n, code) in [("r1", "ghost"), ("r2", "offcat")] {
        let claim = tag(code, Some("https://test.example/emoji/x"), EVIL_URL);
        let outcome = deliver(&state, &xena, &post, (n, code), claim).await;
        assert!(
            matches!(outcome, Outcome::Ignored(_)),
            "{code} is refused, got {outcome:?}"
        );
    }
    assert!(stored(&pool, &xena).await.is_empty(), "nothing is stored");
    assert_eq!(
        emojis(&pool).await,
        vec![("offcat".to_owned(), None)],
        "and no emoji row is added"
    );
}

/// An origin that is neither the signer's nor ours is refused and files nothing.
#[tokio::test]
async fn a_third_party_origin_is_refused_and_files_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let xena = reactor(&pool, "xena", "wired2.test").await;

    let trap = tag(
        "trap",
        Some("https://evil.example/emoji/trap.png"),
        EVIL_URL,
    );
    let outcome = deliver(&state, &xena, &post, ("r1", "trap"), trap).await;

    assert!(matches!(outcome, Outcome::Ignored(_)), "got {outcome:?}");
    assert!(stored(&pool, &xena).await.is_empty(), "no reaction row");
    assert!(emojis(&pool).await.is_empty(), "no emoji row");
}

/// The signer's own emoji is stored under the signer's domain without an emoji row.
#[tokio::test]
async fn the_signers_own_emoji_files_no_emoji_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let carol = reactor(&pool, "carol", "misskey.test").await;

    let own = tag("misskeycat", None, MISSKEY_URL);
    deliver_react(&state, &carol, &post, ("r1", "misskeycat"), own).await;

    assert_eq!(
        stored(&pool, &carol).await,
        vec![(
            ":misskeycat:".to_owned(),
            Some("misskey.test".to_owned()),
            Some(MISSKEY_URL.to_owned())
        )],
        "filed under the signer's domain"
    );
    assert!(emojis(&pool).await.is_empty(), "no emoji row is created");
}

/// An emoji of another instance is joined only through the row we already hold.
#[tokio::test]
async fn a_third_instances_emoji_joins_only_through_its_stored_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let carol = reactor(&pool, "carol", "misskey.test").await;
    let frank = reactor(&pool, "frank", "pleroma.test").await;
    let id = "https://misskey.test/emoji/misskeycat";
    let claim = tag("misskeycat", Some(id), EVIL_URL);

    let unknown = deliver(&state, &frank, &post, ("r0", "misskeycat"), claim.clone()).await;
    assert!(
        matches!(unknown, Outcome::Ignored(_)),
        "no stored row, no join: {unknown:?}"
    );

    custom_emoji::upsert_remote(&pool, "misskeycat", Some("misskey.test"), MISSKEY_URL)
        .await
        .expect("the remote emoji stores");
    deliver_react(&state, &carol, &post, ("r1", "misskeycat"), claim.clone()).await;
    deliver_react(&state, &frank, &post, ("r2", "misskeycat"), claim).await;

    let filed = vec![(
        ":misskeycat:".to_owned(),
        Some("misskey.test".to_owned()),
        Some(MISSKEY_URL.to_owned()),
    )];
    assert_eq!(stored(&pool, &carol).await, filed, "the owner's own");
    assert_eq!(stored(&pool, &frank).await, filed, "the joiner's too");
    assert_eq!(
        emojis(&pool).await,
        vec![("misskeycat".to_owned(), Some("misskey.test".to_owned()))],
        "still the one emoji row"
    );
    assert_eq!(
        rollup(&pool, &post).await,
        vec![("misskeycat@misskey.test".to_owned(), 2)],
        "one group of two"
    );
}

/// The tag's `id` names the origin; the icon may sit on a CDN. A stored row
/// of that origin keeps its image.
#[tokio::test]
async fn a_stored_row_keeps_its_image_against_a_later_tag() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let carol = reactor(&pool, "carol", "host-a.test").await;
    let id = "https://host-a.test/emojis/blob";
    custom_emoji::upsert_remote(
        &pool,
        "blob",
        Some("host-a.test"),
        "https://cdn.test/blob.png",
    )
    .await
    .expect("the remote emoji stores");

    let later = tag("blob", Some(id), "https://cdn.test/other.png");
    deliver_react(&state, &carol, &post, ("r1", "blob"), later).await;

    assert_eq!(
        stored(&pool, &carol).await,
        vec![(
            ":blob:".to_owned(),
            Some("host-a.test".to_owned()),
            Some("https://cdn.test/blob.png".to_owned())
        )],
        "the stored image, not the tag's"
    );
}

/// Undoing a reaction filed under the emoji's origin removes that row only.
#[tokio::test]
async fn an_undo_removes_the_row_filed_under_the_emojis_origin() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let carol = reactor(&pool, "carol", "misskey.test").await;
    let frank = reactor(&pool, "frank", "pleroma.test").await;
    custom_emoji::upsert_remote(&pool, "misskeycat", Some("misskey.test"), MISSKEY_URL)
        .await
        .expect("the remote emoji stores");
    let misskey = tag("misskeycat", None, MISSKEY_URL);
    deliver_react(&state, &carol, &post, ("r1", "misskeycat"), misskey.clone()).await;
    deliver_react(&state, &frank, &post, ("r2", "misskeycat"), misskey).await;

    let undo = view(
        "Undo",
        "u1",
        &frank.ap_id,
        json!("https://remote.example/activities/r2"),
        json!({}),
    );
    let outcome = apply_undo_emoji_react(state, undo, frank.clone())
        .await
        .expect("the undo folds");

    assert_eq!(outcome, Outcome::Handled, "the withdrawal lands");
    assert!(
        stored(&pool, &frank).await.is_empty(),
        "frank's row is gone"
    );
    assert_eq!(stored(&pool, &carol).await.len(), 1, "carol's stands");
    assert_eq!(
        rollup(&pool, &post).await,
        vec![("misskeycat@misskey.test".to_owned(), 1)],
        "the group shrinks by one"
    );
}
