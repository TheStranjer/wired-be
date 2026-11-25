//! The `canonicalize_unicode_reactions` migration: unicode reactions move to
//! their grouping key and the rows that collide merge.

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus},
        user::User,
    },
    test_fixtures::local_user,
    util::{id::new_id, time::parse_time},
};

/// The migration's own SQL, replayed against a slot that already holds the
/// migrated schema.
const MIGRATION_SQL: &str =
    include_str!("../../../migrations/20261125033926_canonicalize_unicode_reactions.sql");

/// A heart with its variation selector.
const QUALIFIED_HEART: &str = "\u{2764}\u{FE0F}";
/// A heart without it, the grouping key.
const BARE_HEART: &str = "\u{2764}";
/// The rainbow flag, a ZWJ sequence that keeps its selector.
const RAINBOW: &str = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}";

/// Stores `who`'s reaction `emoji` on `status_id`, made at `at`, as `ap_id`.
async fn insert(
    pool: &PgPool,
    who: &User,
    status_id: Uuid,
    emoji: &str,
    (ap_id, at): (&str, &str),
) {
    let inserted = sqlx::query(
        "INSERT INTO emoji_reactions (id, user_id, status_id, emoji, ap_id, inserted_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(new_id())
    .bind(who.id)
    .bind(status_id)
    .bind(emoji)
    .bind(ap_id)
    .bind(parse_time(at).expect("the fixture time parses"))
    .execute(pool)
    .await
    .expect("the fixture reaction stores")
    .rows_affected();
    assert_eq!(inserted, 1, "one row went in");
}

/// Every reaction as `(reactor nickname, emoji, activity id)`.
async fn reactions(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT u.nickname, r.emoji, r.ap_id FROM emoji_reactions r
           JOIN users u ON u.id = r.user_id
          ORDER BY u.nickname, r.ap_id",
    )
    .fetch_all(pool)
    .await
    .expect("the reactions read")
}

/// One expected row of [`reactions`].
fn row(reactor: &str, emoji: &str, ap_id: &str) -> (String, String, String) {
    (reactor.to_owned(), emoji.to_owned(), ap_id.to_owned())
}

/// Seeds `status_id` with the reactions of carol, dave, erin, frank and gina.
///
/// Carol holds both spellings of the heart, the earlier one second.
async fn seed(pool: &PgPool, status_id: Uuid) {
    let at = "2024-03-01T00:00:00Z";
    let rows = [
        (
            "carol",
            BARE_HEART,
            "https://a.test/1",
            "2024-02-01T00:00:00Z",
        ),
        (
            "carol",
            QUALIFIED_HEART,
            "https://a.test/2",
            "2024-01-01T00:00:00Z",
        ),
        ("dave", QUALIFIED_HEART, "https://a.test/3", at),
        ("erin", "1\u{FE0F}\u{20E3}", "https://a.test/4", at),
        ("frank", RAINBOW, "https://a.test/5", at),
        ("gina", ":cat:", "https://a.test/6", at),
        ("gina", "\u{1F44D}", "https://a.test/7", at),
    ];
    let mut users: HashMap<&str, User> = HashMap::new();
    for nickname in ["carol", "dave", "erin", "frank", "gina"] {
        let created = local_user(pool, nickname).await;
        let _previous = users.insert(nickname, created);
    }
    for (nickname, emoji, ap_id, made) in rows {
        let who = users.get(nickname).expect("the reactor was created above");
        insert(pool, who, status_id, emoji, (ap_id, made)).await;
    }
}

/// Unicode reactions lose their U+FE0F unless they hold a ZWJ; an account's
/// two spellings of one emoji merge into the earliest; custom emoji and ZWJ
/// sequences stay as they are.
#[tokio::test]
async fn unicode_reactions_move_to_their_grouping_key_and_merge() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let author = local_user(&pool, "alice").await;
    let status_id = status::insert(&pool, TEST_BASE_URL, &NewStatus::local(&author, "a post"))
        .await
        .expect("the status fixture inserts")
        .id;
    seed(&pool, status_id).await;

    let _migrated = sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();

    assert_eq!(
        reactions(&pool).await,
        vec![
            row("carol", BARE_HEART, "https://a.test/2"),
            row("dave", BARE_HEART, "https://a.test/3"),
            row("erin", "1\u{20E3}", "https://a.test/4"),
            row("frank", RAINBOW, "https://a.test/5"),
            row("gina", ":cat:", "https://a.test/6"),
            row("gina", "\u{1F44D}", "https://a.test/7"),
        ],
        "hearts and the keycap are keyed, carol's two merged into her earliest, \
         and the ZWJ sequence, the custom emoji and the thumb are as they were"
    );

    let _again = sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();
    assert_eq!(
        reactions(&pool).await.len(),
        6,
        "replaying the migration changes nothing"
    );
}
