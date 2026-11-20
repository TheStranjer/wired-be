//! The per-user feature columns, the suggestions tables and the
//! received-activities ledger.

use super::*;

/// The preferred-frontend migration (WBE-122) adds the one column
/// `PUT /api/v1/pleroma/preferred_frontend` stores: a nullable
/// `text` `preferred_frontend` on `users` with no default — the
/// account that never picked a frontend keeps NULL, and the pick
/// itself is validated against `AVAILABLE_FRONTENDS`, not against
/// the schema.
#[tokio::test]
async fn users_preferred_frontend_migration_adds_the_column() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (data_type, is_nullable, default): (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'users'
            AND column_name = 'preferred_frontend'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "text", "the frontend name is stored as text");
    assert_eq!(is_nullable, "YES", "no choice is stored as NULL");
    assert!(default.is_none(), "and nothing defaults to a frontend");
}

/// The notification-settings migration (WBE-111) adds the one
/// column `PUT /api/pleroma/notification_settings` writes: a
/// `jsonb` `notification_settings` on `users`, `NOT NULL` and
/// defaulting to both flags `false` — the defaults of Pleroma's
/// `User.NotificationSetting` embedded schema, so every account
/// answers the settings before it ever touches them.
#[tokio::test]
async fn users_notification_settings_migration_adds_the_column() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (data_type, is_nullable, default): (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'users'
            AND column_name = 'notification_settings'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "jsonb", "the flags ride one jsonb blob");
    assert_eq!(is_nullable, "NO", "every account answers them");
    assert_eq!(
        default.as_deref(),
        Some(r#"'{"block_from_strangers": false, "hide_notification_contents": false}'::jsonb"#),
        "both flags default to false: {default:?}"
    );
}

/// The suggestions migration (WBE-124) is the pair the suggestion
/// endpoints read and write: an `is_suggested` flag on `users` that
/// defaults to `false` (an account joins the pool only when an admin
/// says so), and `dismissed_suggestions`, whose only columns are the
/// viewer and the account they dismissed and whose primary key is
/// that pair — one dismissal per pair, and either account leaving
/// takes the row with it.
#[tokio::test]
async fn suggestions_migration_flags_the_pool_and_keys_the_dismissals() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (data_type, is_nullable, default): (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'users'
            AND column_name = 'is_suggested'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "boolean", "the pool is a flag");
    assert_eq!(is_nullable, "NO", "every account answers it one way");
    assert_eq!(
        default.as_deref(),
        Some("false"),
        "and nothing starts in the pool: {default:?}"
    );

    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'dismissed_suggestions'
       ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("user_id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
            ("target_id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
        ],
        "the dismissal table is the pair and nothing else"
    );

    let (key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'dismissed_suggestions'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        key_columns, "target_id,user_id",
        "the pair is the key {key}: one dismissal per viewer and account"
    );

    let foreign_keys: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT con.conname, con.confdeltype::text, conf.relname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_class conf ON conf.oid = con.confrelid
          WHERE con.contype = 'f' AND cls.relname = 'dismissed_suggestions'
       ORDER BY con.conname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        foreign_keys,
        vec![
            (
                "dismissed_suggestions_target_id_fkey".to_owned(),
                "c".to_owned(),
                "users".to_owned()
            ),
            (
                "dismissed_suggestions_user_id_fkey".to_owned(),
                "c".to_owned(),
                "users".to_owned()
            ),
        ],
        "both ends are accounts, and deleting either takes the dismissal"
    );
}

/// The email-notifications migration (WBE-220) adds the one column
/// the HTML unsubscribe page (a later ticket) clears: a `NOT NULL`
/// `boolean` `email_notifications` on `users` defaulted to `true`,
/// the flag Pleroma's `User.switch_email_notifications`
/// (`lib/pleroma/user.ex`) moves — an account is opted in to email
/// until it says otherwise.
#[tokio::test]
async fn users_email_notifications_migration_adds_the_column() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (data_type, is_nullable, default): (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'users'
            AND column_name = 'email_notifications'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "boolean", "the mail switch is a flag");
    assert_eq!(is_nullable, "NO", "every account answers it one way");
    assert_eq!(
        default.as_deref(),
        Some("true"),
        "accounts start opted in to email: {default:?}"
    );
}

/// The received-activities migration (WBE-163) creates the dedupe
/// ledger the `federator.incoming` dispatcher claims through: the
/// activity's `ActivityPub` id as the text primary key the
/// `INSERT … ON CONFLICT DO NOTHING` claims against, and the
/// `now()`-stamped `inserted_at` the daily prune ages rows out by
/// — indexed, so the prune reads rather than scans.
#[tokio::test]
async fn received_activities_migration_keys_the_dedupe_ledger() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<Described> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'received_activities'
       ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("ap_id".to_owned(), "text".to_owned(), "NO".to_owned(), None),
            (
                "inserted_at".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned(),
                Some("now()".to_owned())
            ),
        ],
        "the ticket's two columns, in the ticket's order"
    );

    let (primary_key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'received_activities'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        key_columns, "ap_id",
        "the id alone is the key {primary_key}"
    );

    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes
          WHERE schemaname = 'public' AND tablename = 'received_activities'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        indexes
            .iter()
            .any(|(name, _)| name == "received_activities_inserted_at_idx"),
        "the prune's age scan is indexed: {indexes:?}"
    );
}
