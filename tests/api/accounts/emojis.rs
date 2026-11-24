//! The custom emojis an account's profile uses: `Account.emojis` on the
//! credentials, show and embedded-account renders.

use serde_json::{Value, json};
use wired_be::models::{
    custom_emoji::{self, NewCustomEmoji},
    status::Visibility,
    user::{NewUser, User, insert},
};

use super::support::{show, verify_credentials};
use crate::common;

/// Stores the local emoji `shortcode`.
async fn local_emoji(pool: &sqlx::PgPool, shortcode: &str) {
    let url = format!("https://test.example/emoji/{shortcode}.png");
    let _row = custom_emoji::upsert(pool, &NewCustomEmoji::local(shortcode, &url))
        .await
        .expect("the local emoji stores");
}

/// Stores the emoji `shortcode` announced by `host`.
async fn remote_emoji(pool: &sqlx::PgPool, shortcode: &str, host: &str) {
    let url = format!("https://{host}/emoji/{shortcode}.png");
    custom_emoji::upsert_remote(pool, shortcode, Some(host), &url)
        .await
        .expect("the remote emoji stores");
}

/// A mirror of `nickname` on `host` whose display name is `name`.
async fn remote_named(pool: &sqlx::PgPool, nickname: &str, host: &str, name: &str) -> User {
    let ap_id = format!("https://{host}/users/{nickname}");
    let new = NewUser {
        nickname: format!("{nickname}@{host}"),
        ap_id,
        domain: Some(host.to_owned()),
        name: name.to_owned(),
        ..NewUser::default()
    };
    insert(pool, &new).await.expect("the mirror inserts")
}

/// The shortcodes of the `emojis` array of `body`.
fn shortcodes(body: &Value) -> Vec<&str> {
    body["emojis"]
        .as_array()
        .expect("emojis is an array")
        .iter()
        .filter_map(|emoji| emoji["shortcode"].as_str())
        .collect()
}

/// `verify_credentials` lists the local emojis the caller's name and note use.
#[tokio::test]
async fn verify_credentials_lists_the_local_profile_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let wanda = common::create_local_user_with(&pool, "wanda", |mut new| {
        new.name = "Wanda :wiredcat:".to_owned();
        new.bio = "<p>bio :wiredcat: :missing:</p>".to_owned();
        new
    })
    .await;
    local_emoji(&pool, "wiredcat").await;
    remote_emoji(&pool, "missing", "elsewhere.example").await;
    let token = common::create_app_and_token(&pool, &wanda, "read:accounts").await;
    let app = common::test_app(pool);

    let body = verify_credentials(&app, &token).await.json_value();

    assert_eq!(shortcodes(&body), ["wiredcat"], "only the local emoji");
    assert_eq!(
        body["emojis"][0]["url"], "https://test.example/emoji/wiredcat.png",
        "in the CustomEmoji entity shape"
    );
}

/// A remote account lists the emojis of its own domain only, whatever other
/// domains call their own `:blob:`.
#[tokio::test]
async fn show_lists_a_remote_accounts_emojis_under_its_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = remote_named(&pool, "alice", "mastodon.example", "Alice :mastofox:").await;
    let carol = remote_named(&pool, "carol", "misskey.example", "Carol :misskeycat:").await;
    remote_emoji(&pool, "mastofox", "mastodon.example").await;
    remote_emoji(&pool, "mastofox", "misskey.example").await;
    remote_emoji(&pool, "misskeycat", "misskey.example").await;
    let app = common::test_app(pool);

    let alice_body = show(&app, &alice.id.to_string()).await.json_value();
    let carol_body = show(&app, &carol.id.to_string()).await.json_value();

    assert_eq!(shortcodes(&alice_body), ["mastofox"], "alice's own emoji");
    assert_eq!(
        alice_body["emojis"][0]["url"], "https://mastodon.example/emoji/mastofox.png",
        "the row of her domain, not misskey's"
    );
    assert_eq!(shortcodes(&carol_body), ["misskeycat"], "carol's own emoji");
}

/// An account using no emoji renders the empty array.
#[tokio::test]
async fn an_account_without_shortcodes_lists_none() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let plain = remote_named(&pool, "plain", "mastodon.example", "Plain").await;
    remote_emoji(&pool, "mastofox", "mastodon.example").await;
    let app = common::test_app(pool);

    let body = show(&app, &plain.id.to_string()).await.json_value();

    assert_eq!(body["emojis"], json!([]), "nothing used, nothing listed");
}

/// The account embedded in a status carries its profile emojis too.
#[tokio::test]
async fn a_statuses_embedded_account_lists_its_profile_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let wanda = common::create_local_user_with(&pool, "wanda", |mut new| {
        new.name = "Wanda :wiredcat:".to_owned();
        new
    })
    .await;
    local_emoji(&pool, "wiredcat").await;
    let status = common::create_status(&pool, &wanda, "hello", Visibility::Public).await;
    let app = common::test_app(pool);

    let body = common::TestRequest::get(&format!("/api/v1/statuses/{}", status.id))
        .send(&app)
        .await
        .json_value();

    assert_eq!(
        shortcodes(&body["account"]),
        ["wiredcat"],
        "the author's profile emoji ride the embedded account"
    );
    assert_eq!(body["emojis"], json!([]), "the post's own text uses none");
}
