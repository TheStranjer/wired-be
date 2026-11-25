//! Which instance a delivered reaction's custom emoji is filed under: the
//! one the `Emoji` tag names, not the reactor's.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::super::tests::{app_state, local_post, signer, view};
use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        custom_emoji::NewCustomEmoji,
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
async fn deliver_react(
    state: &AppState,
    who: &User,
    post: &Status,
    (n, code): (&str, &str),
    tag: Value,
) {
    let react = view(
        "EmojiReact",
        n,
        &who.ap_id,
        json!(post.uri),
        json!({"content": format!(":{code}:"), "tag": [tag]}),
    );
    let outcome = apply_emoji_react(state.clone(), react, who.clone())
        .await
        .expect("the fold does not fail");
    assert_eq!(outcome, Outcome::Handled, "the reaction folds");
}

/// `who`'s stored reactions as `(emoji, emoji_domain)`.
async fn stored(pool: &PgPool, who: &User) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT emoji, emoji_domain FROM emoji_reactions WHERE user_id = $1")
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

/// Another instance's reaction with this instance's own emoji is the local
/// reaction: it counts with a local user's and files no emoji of the reactor's.
#[tokio::test]
async fn a_reaction_with_our_own_emoji_is_the_local_reaction() {
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
        TEST_BASE_URL,
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

    let own = tag("wiredcat", None, WIRED_URL);
    deliver_react(&state, &xena, &post, ("r1", "wiredcat"), own).await;

    assert_eq!(
        stored(&pool, &xena).await,
        vec![(":wiredcat:".to_owned(), None)],
        "the reaction names no foreign domain"
    );
    assert_eq!(
        emojis(&pool).await,
        vec![("wiredcat".to_owned(), None)],
        "and no `wiredcat` of wired2.test appears"
    );
    assert_eq!(
        rollup(&pool, &post).await,
        vec![("wiredcat".to_owned(), 2)],
        "it is one group with the local user's"
    );
}

/// The emoji's own image URL marks it ours even when the tag sits on a CDN.
#[tokio::test]
async fn a_tag_carrying_our_emoji_image_is_ours_on_any_host() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let cdn = "https://cdn.example/wiredcat.png";
    let _local = custom_emoji::upsert(&pool, &NewCustomEmoji::local("wiredcat", cdn))
        .await
        .expect("the local emoji stores");
    let xena = reactor(&pool, "xena", "wired2.test").await;

    deliver_react(
        &state,
        &xena,
        &post,
        ("r1", "wiredcat"),
        tag("wiredcat", None, cdn),
    )
    .await;

    assert_eq!(
        stored(&pool, &xena).await,
        vec![(":wiredcat:".to_owned(), None)],
        "the local emoji's image is enough"
    );
}

/// A Pleroma user joining a Misskey emoji files it under Misskey and groups
/// with the Misskey user's own reaction.
#[tokio::test]
async fn a_third_instances_emoji_files_under_its_own_host() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let carol = reactor(&pool, "carol", "misskey.test").await;
    let frank = reactor(&pool, "frank", "pleroma.test").await;

    let misskey = tag("misskeycat", None, MISSKEY_URL);
    deliver_react(&state, &carol, &post, ("r1", "misskeycat"), misskey.clone()).await;
    deliver_react(&state, &frank, &post, ("r2", "misskeycat"), misskey).await;

    let filed = vec![(":misskeycat:".to_owned(), Some("misskey.test".to_owned()))];
    assert_eq!(stored(&pool, &carol).await, filed, "the owner's own");
    assert_eq!(stored(&pool, &frank).await, filed, "the joiner's too");
    assert_eq!(
        emojis(&pool).await,
        vec![("misskeycat".to_owned(), Some("misskey.test".to_owned()))],
        "one emoji row, none under pleroma.test"
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
async fn a_tag_id_wins_over_the_icons_cdn_and_a_stored_emoji_is_reused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let post = local_post(&pool, "wanda").await;
    let carol = reactor(&pool, "carol", "pleroma.test").await;
    let frank = reactor(&pool, "frank", "pleroma.test").await;
    let id = "https://host-a.test/emojis/blob";

    deliver_react(
        &state,
        &carol,
        &post,
        ("r1", "blob"),
        tag("blob", Some(id), "https://cdn.test/blob.png"),
    )
    .await;
    deliver_react(
        &state,
        &frank,
        &post,
        ("r2", "blob"),
        tag("blob", Some(id), "https://cdn.test/other.png"),
    )
    .await;

    assert_eq!(
        stored(&pool, &carol).await,
        vec![(":blob:".to_owned(), Some("host-a.test".to_owned()))],
        "filed under the tag id's host"
    );
    let image: String =
        sqlx::query_scalar("SELECT image_url FROM custom_emojis WHERE domain = 'host-a.test'")
            .fetch_one(&pool)
            .await
            .expect("the emoji row reads");
    assert_eq!(
        image, "https://cdn.test/blob.png",
        "a second reactor does not replace the stored image"
    );
}

/// Without a usable origin the signer's domain files the emoji, as it did
/// before tags were read.
#[tokio::test]
async fn an_emoji_without_a_usable_origin_falls_back_to_the_signers_domain() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = signer(&pool).await;
    let anonymous = Reaction {
        emoji: ":cat:".to_owned(),
        url: Some("https://cdn.test/cat.png".to_owned()),
        origin: None,
    };
    let anonymous_domain = remember_emoji(&state, &bob, &anonymous)
        .await
        .expect("the emoji is recorded");
    assert_eq!(
        anonymous_domain.as_deref(),
        Some("remote.example"),
        "the signer's"
    );

    let ours_but_gone = Reaction {
        emoji: ":gone:".to_owned(),
        url: Some("https://test.example/gone.png".to_owned()),
        origin: Some("test.example".to_owned()),
    };
    let gone_domain = remember_emoji(&state, &bob, &ours_but_gone)
        .await
        .expect("the emoji is recorded");
    assert_eq!(
        gone_domain.as_deref(),
        Some("remote.example"),
        "our host with no such local emoji is not a local reaction"
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
