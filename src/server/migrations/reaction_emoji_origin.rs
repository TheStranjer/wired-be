//! The `refile_reaction_emoji_origin` migration: reactions filed under their
//! reactor's domain move to the emoji's own origin and merge there.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        custom_emoji::{self, NewCustomEmoji},
        emoji_reaction::{self, NewReaction},
        status::{self, NewStatus},
        user::{self, Flag, NewUser, User},
    },
    test_fixtures::local_user,
};

/// The migration's own SQL, replayed against a slot that already holds the
/// migrated schema.
const MIGRATION_SQL: &str =
    include_str!("../../../migrations/20261125022322_refile_reaction_emoji_origin.sql");

/// The image of this instance's own `:wiredcat:`.
const WIRED_URL: &str = "https://test.example/emoji/wiredcat.png";
/// The image of `misskey.test`'s `:misskeycat:`.
const MISSKEY_URL: &str = "https://misskey.test/files/misskeycat.png";
/// An image on a host this server knows nothing else about.
const CDN_URL: &str = "https://cdn.unknown.test/blob.png";

/// A remote account `nickname` of `domain`.
async fn remote(pool: &PgPool, nickname: &str, domain: &str) -> User {
    user::insert(
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
    .expect("the remote fixture inserts")
}

/// A public post by the local `nickname`, as its id.
async fn post(pool: &PgPool, nickname: &str) -> Uuid {
    let author = local_user(pool, nickname).await;
    status::insert(pool, TEST_BASE_URL, &NewStatus::local(&author, "a post"))
        .await
        .expect("the status fixture inserts")
        .id
}

/// Stores `reactor`'s reaction `emoji` of `domain` with image `url`.
async fn react(
    pool: &PgPool,
    reactor: &User,
    status_id: Uuid,
    (emoji, url): (&str, Option<&str>),
    domain: Option<&str>,
) {
    let ap_id = format!("https://peer.test/activities/{}", Uuid::now_v7());
    let _row = emoji_reaction::react_in_domain(
        pool,
        &NewReaction {
            user_id: reactor.id,
            status_id,
            emoji,
            url,
        },
        &ap_id,
        domain,
    )
    .await
    .expect("the fixture reaction stores");
}

/// One stored reaction: reactor nickname, nickname of the post's author,
/// emoji and `emoji_domain`.
type StoredReaction = (String, String, String, Option<String>);

/// Every reaction, ordered by reactor, post author and emoji.
async fn reactions(pool: &PgPool) -> Vec<StoredReaction> {
    sqlx::query_as(
        "SELECT u.nickname, a.nickname, r.emoji, r.emoji_domain FROM emoji_reactions r
           JOIN users u ON u.id = r.user_id
           JOIN statuses s ON s.id = r.status_id
           JOIN users a ON a.id = s.user_id
          ORDER BY u.nickname, a.nickname, r.emoji, r.emoji_domain",
    )
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

/// Reactions of a third instance's emoji and of this instance's own, filed
/// under their reactor's domain, move to the emoji's origin; a move onto a
/// reaction that stands merges into it; an image on an unknown host and a
/// correctly filed row stay as they are.
#[tokio::test]
async fn misfiled_reactions_move_to_the_emojis_origin_and_merge() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let xena = remote(&pool, "xena", "wired2.test").await;
    let frank = remote(&pool, "frank", "pleroma.test").await;
    let _carol = remote(&pool, "carol", "misskey.test").await;
    let first = post(&pool, "alice").await;
    let second = post(&pool, "bob").await;
    let _local = custom_emoji::upsert(&pool, &NewCustomEmoji::local("wiredcat", WIRED_URL))
        .await
        .expect("the local emoji stores");
    for (code, domain, url) in [
        ("wiredcat", "wired2.test", WIRED_URL),
        ("misskeycat", "pleroma.test", MISSKEY_URL),
        ("misskeycat", "misskey.test", MISSKEY_URL),
        ("blob", "pleroma.test", CDN_URL),
    ] {
        custom_emoji::upsert_remote(&pool, code, Some(domain), url)
            .await
            .expect("the emoji row stores");
    }
    let wired = (":wiredcat:", Some(WIRED_URL));
    react(&pool, &xena, first, wired, Some("wired2.test")).await;
    react(&pool, &xena, second, wired, Some("wired2.test")).await;
    react(&pool, &xena, second, wired, None).await;
    let misskey = (":misskeycat:", Some(MISSKEY_URL));
    react(&pool, &frank, first, misskey, Some("pleroma.test")).await;
    react(
        &pool,
        &frank,
        first,
        (":blob:", Some(CDN_URL)),
        Some("pleroma.test"),
    )
    .await;

    let _migrated = sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();

    let row = |reactor: &str, author: &str, emoji: &str, domain: Option<&str>| -> StoredReaction {
        (
            reactor.to_owned(),
            author.to_owned(),
            emoji.to_owned(),
            domain.map(str::to_owned),
        )
    };
    assert_eq!(
        reactions(&pool).await,
        vec![
            row(
                "frank@pleroma.test",
                "alice",
                ":blob:",
                Some("pleroma.test")
            ),
            row(
                "frank@pleroma.test",
                "alice",
                ":misskeycat:",
                Some("misskey.test")
            ),
            row("xena@wired2.test", "alice", ":wiredcat:", None),
            row("xena@wired2.test", "bob", ":wiredcat:", None),
        ],
        "the own emoji is local, the third instance's is filed under it, the two of \
         one account merged and the unknown CDN's row stayed"
    );
    assert_eq!(
        emojis(&pool).await,
        vec![
            ("blob".to_owned(), Some("pleroma.test".to_owned())),
            ("misskeycat".to_owned(), Some("misskey.test".to_owned())),
            ("wiredcat".to_owned(), None),
        ],
        "the fake rows are gone and the reaction's emoji stands under its origin"
    );
}

/// Replaying the migration over data it already fixed changes nothing.
#[tokio::test]
async fn the_migration_is_idempotent() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let carol = remote(&pool, "carol", "misskey.test").await;
    let status_id = post(&pool, "alice").await;
    custom_emoji::upsert_remote(&pool, "misskeycat", Some("misskey.test"), MISSKEY_URL)
        .await
        .expect("the emoji row stores");
    react(
        &pool,
        &carol,
        status_id,
        (":misskeycat:", Some(MISSKEY_URL)),
        Some("misskey.test"),
    )
    .await;
    let before = reactions(&pool).await;

    let _migrated = sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();
    let _again = sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();

    assert_eq!(reactions(&pool).await, before, "a filed row stays filed");
    assert_eq!(
        emojis(&pool).await,
        vec![("misskeycat".to_owned(), Some("misskey.test".to_owned()))],
        "and so does its emoji"
    );
}
