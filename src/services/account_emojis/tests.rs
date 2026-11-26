//! Tests for [`AccountEmojis`]: the profile texts it scans, the per-domain
//! lookup and the filled `Account` entity.

use serde_json::json;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        custom_emoji::{self, NewCustomEmoji},
        user::{NewUser, insert, local_ap_id},
    },
};

/// A local account `nickname` whose profile columns `customize` sets.
async fn local<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(&mut NewUser),
{
    let mut new = NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname));
    customize(&mut new);
    insert(pool, &new).await.expect("the local fixture inserts")
}

/// A mirror of `nickname` on `host` whose profile columns `customize` sets.
async fn remote<F>(pool: &PgPool, nickname: &str, host: &str, customize: F) -> User
where
    F: FnOnce(&mut NewUser),
{
    let mut new = NewUser {
        nickname: format!("{nickname}@{host}"),
        ap_id: format!("https://{host}/users/{nickname}"),
        domain: Some(host.to_owned()),
        ..NewUser::default()
    };
    customize(&mut new);
    insert(pool, &new)
        .await
        .expect("the remote fixture inserts")
}

/// Stores the remote emoji `shortcode` of `domain` (`None` = local).
async fn emoji(pool: &PgPool, shortcode: &str, domain: Option<&str>) {
    let url = format!(
        "https://{}/emoji/{shortcode}.png",
        domain.unwrap_or("test.example")
    );
    if domain.is_some() {
        custom_emoji::upsert_remote(pool, shortcode, domain, &url)
            .await
            .expect("the remote emoji stores");
    } else {
        let _row = custom_emoji::upsert(pool, &NewCustomEmoji::local(shortcode, &url))
            .await
            .expect("the local emoji stores");
    }
}

/// The shortcodes `rows` carry, in order.
fn codes(rows: &[EmojiRow]) -> Vec<&str> {
    rows.iter().map(|row| row.shortcode.as_str()).collect()
}

#[tokio::test]
async fn the_profile_texts_are_name_note_and_every_field_name_and_value() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let alice = local(db.pool(), "alice", |new| {
        new.name = "Alice :a:".to_owned();
        new.bio = "<p>:b:</p>".to_owned();
        new.fields = json!([
            {"name": ":c:", "value": ":d:"},
            {"name": "no value"},
            "stray",
        ]);
    })
    .await;

    let item = profile_texts(&alice);

    assert_eq!(item.key, alice.id, "filed under the account's id");
    assert_eq!(item.domain, None, "a local account has no domain");
    assert_eq!(
        item.texts,
        ["Alice :a:", "<p>:b:</p>", ":c:", ":d:", "no value"],
        "name, note, then each well-formed field's name and value"
    );
}

#[tokio::test]
async fn a_remote_account_uses_only_the_emojis_of_its_own_domain() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let carol = remote(pool, "carol", "misskey.example", |new| {
        new.name = "Carol :misskeycat: :other:".to_owned();
    })
    .await;
    emoji(pool, "misskeycat", Some("misskey.example")).await;
    emoji(pool, "misskeycat", Some("elsewhere.example")).await;
    emoji(pool, "misskeycat", None).await;
    emoji(pool, "other", Some("elsewhere.example")).await;

    let loaded = AccountEmojis::load(pool, &[&carol]).await.unwrap();

    let found = loaded.of(&carol);
    assert_eq!(
        codes(found),
        ["misskeycat"],
        "only the account's own domain"
    );
    assert_eq!(
        found.first().and_then(|row| row.domain.as_deref()),
        Some("misskey.example"),
        "and it is that domain's row"
    );
}

#[tokio::test]
async fn a_local_account_uses_the_local_emojis() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let wanda = local(pool, "wanda", |new| {
        new.name = "Wanda :wiredcat:".to_owned();
        new.bio = "<p>bio :wiredcat: :missing:</p>".to_owned();
    })
    .await;
    emoji(pool, "wiredcat", None).await;
    emoji(pool, "wiredcat", Some("remote.example")).await;

    let loaded = AccountEmojis::load(pool, &[&wanda]).await.unwrap();

    assert_eq!(
        codes(loaded.of(&wanda)),
        ["wiredcat"],
        "the local row, once, and the unknown shortcode skipped"
    );
}

#[tokio::test]
async fn note_and_field_emojis_count_and_a_disabled_one_does_not() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let alice = remote(pool, "alice", "mastodon.example", |new| {
        new.bio = "<p>masto bio :mastofox:</p>".to_owned();
        new.fields = json!([{"name": ":fieldname:", "value": "<p>:fieldvalue: :off:</p>"}]);
    })
    .await;
    for code in ["mastofox", "fieldname", "fieldvalue", "off"] {
        emoji(pool, code, Some("mastodon.example")).await;
    }
    let _disabled = sqlx::query("UPDATE custom_emojis SET disabled = true WHERE shortcode = 'off'")
        .execute(pool)
        .await
        .unwrap();

    let loaded = AccountEmojis::load(pool, &[&alice]).await.unwrap();

    assert_eq!(
        codes(loaded.of(&alice)),
        ["mastofox", "fieldname", "fieldvalue"],
        "note, field name and field value, in order of appearance; \
         the disabled emoji is hidden"
    );
}

#[tokio::test]
async fn fill_renders_the_custom_emoji_entities_and_leaves_others_empty() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let config = Config::for_tests();
    let bob = remote(pool, "bob", "remote.example", |new| {
        new.name = "Bob :blob:".to_owned();
    })
    .await;
    let plain = local(pool, "plain", |_new| {}).await;
    emoji(pool, "blob", Some("remote.example")).await;

    let loaded = AccountEmojis::load(pool, &[&bob, &plain]).await.unwrap();
    let filled = loaded.fill(Account::from_user(&bob, &config), &bob, &config);
    let untouched = loaded.fill(Account::from_user(&plain, &config), &plain, &config);

    let value = serde_json::to_value(&filled).unwrap();
    assert_eq!(
        value["emojis"],
        json!([{
            "shortcode": "blob",
            "url": "https://remote.example/emoji/blob.png",
            "static_url": "https://remote.example/emoji/blob.png",
            "visible_in_picker": true,
            "category": null,
            "tags": [],
        }]),
        "the CustomEmoji entity shape"
    );
    assert!(
        untouched.emojis.is_empty(),
        "an account using no emoji renders []"
    );
}
