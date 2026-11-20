//! The chat tables, the chat-message idempotency key and the status
//! idempotency-keys table.

use super::*;

/// The chats migration creates the three tables `src/models/chat.rs`
/// reads and writes — one chat per participant, the shared message,
/// and one reference per participant's chat — and links each of them
/// the way the model's queries assume: the accounts and the media a
/// message may name, the cascades that take a conversation apart with
/// its owners, and the keys its upserts conflict on.
#[tokio::test]
async fn chats_migration_links_both_sides_of_a_conversation() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let keys: Vec<ForeignKey> = sqlx::query_as(
        "SELECT cls.relname, con.conname, con.confdeltype::text, conf.relname AS target
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_class conf ON conf.oid = con.confrelid
          WHERE con.contype = 'f'
            AND cls.relname IN ('chats', 'chat_messages', 'chat_message_references')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_chat_foreign_keys(&keys);
}

/// The unique keys and indexes the chats migration adds: one chat per
/// participant and account, one reference per chat and message, a
/// reserved `ap_id`, and the newest-first and unread-only indexes the
/// list queries read.
#[tokio::test]
async fn chats_migration_keys_and_indexes_the_chat_tables() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let uniques: Vec<(String, String)> = sqlx::query_as(
        "SELECT cls.relname, con.conname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
          WHERE con.contype = 'u'
            AND cls.relname IN ('chats', 'chat_messages', 'chat_message_references')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_chat_unique_keys(&uniques);

    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes
          WHERE schemaname = 'public'
            AND tablename IN ('chats', 'chat_messages', 'chat_message_references')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_chat_indexes(&indexes);
    let (unread_name, unread_def) = indexes
        .iter()
        .find(|(indexname, _)| indexname == "chat_message_references_chat_id_unread_idx")
        .unwrap();
    assert!(
        unread_def.contains("WHERE unread"),
        "the unread index {unread_name} holds only unread references: {unread_def}"
    );
}

/// The column defaults and `NOT NULL`s the chats migration gives the
/// three tables, which are exactly what `src/models/chat.rs` leaves
/// for the database to fill.
#[tokio::test]
async fn chats_migration_defaults_match_the_model() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<SchemaColumn> = sqlx::query_as(
        "SELECT table_name, column_name, column_default, is_nullable
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name IN ('chats', 'chat_messages', 'chat_message_references')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_chat_column_defaults(&columns);
    assert_chat_required_columns(&columns);
    assert!(
        column_of(&columns, "chat_messages", "deleted_at")
            .column_default
            .is_none()
            && column_of(&columns, "chat_messages", "deleted_at").is_nullable == "YES",
        "a live message has no deleted_at until it is withdrawn"
    );
}

/// The chat message idempotency migration (WBE-147) adds the one
/// column the `Idempotency-Key` replay lookup needs — a nullable
/// `text` `idempotency_key` on `chat_messages` — and the partial
/// index that serves the sender-and-key search, keeping every
/// keyless message out of it.
#[tokio::test]
async fn chat_message_idempotency_migration_adds_the_key() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (data_type, is_nullable, default): (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'chat_messages'
            AND column_name = 'idempotency_key'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "text", "the key is stored as text");
    assert_eq!(is_nullable, "YES", "a keyless message keeps NULL");
    assert!(default.is_none(), "and nothing defaults to a key");

    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes
          WHERE schemaname = 'public' AND tablename = 'chat_messages'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let (_, definition) = indexes
        .iter()
        .find(|(name, _)| name == "chat_messages_sender_id_idempotency_key_idx")
        .unwrap_or_else(|| panic!("the replay index exists: {indexes:?}"));
    assert!(
        definition.contains("sender_id")
            && definition.contains("idempotency_key")
            && definition.contains("idempotency_key IS NOT NULL"),
        "the index covers sender and key, keyed rows only: {definition}"
    );
}

/// The foreign keys the idempotency-keys migration declares: both
/// cascade, so a departed account or a hard-deleted post retires
/// the mapping with them.
fn assert_idempotency_foreign_keys(keys: &[(String, String, String)]) {
    for (constraint, action, target) in [
        ("idempotency_keys_user_id_fkey", "c", "users"),
        ("idempotency_keys_status_id_fkey", "c", "statuses"),
    ] {
        let found = keys
            .iter()
            .find(|(name, _, _)| name == constraint)
            .unwrap_or_else(|| panic!("{constraint} links the table"));
        assert_eq!(found.1, action, "{constraint} deletes as {action}");
        assert_eq!(found.2, target, "{constraint} points at {target}");
    }
}

/// The idempotency-keys migration (WBE-87) creates the table
/// `src/models/idempotency_key.rs` claims and replays through: the
/// `(user_id, key)` pair as the primary key the create's
/// `ON CONFLICT DO NOTHING` inserts against, the status the key
/// created, and both foreign keys cascading — a departed account or
/// a hard-deleted post retires the mapping with them.
#[tokio::test]
async fn idempotency_keys_migration_keys_one_status_per_account_and_key() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<Described> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'idempotency_keys'
       ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            (
                "user_id".to_owned(),
                "uuid".to_owned(),
                "NO".to_owned(),
                None
            ),
            ("key".to_owned(), "text".to_owned(), "NO".to_owned(), None),
            (
                "status_id".to_owned(),
                "uuid".to_owned(),
                "NO".to_owned(),
                None
            ),
            (
                "inserted_at".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned(),
                Some("now()".to_owned())
            ),
        ],
        "the ticket's four columns, in the ticket's order, only the timestamp defaulted"
    );

    let (key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'idempotency_keys'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        key_columns, "key,user_id",
        "the pair is the key {key}: one status per account and key"
    );

    let keys: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT con.conname, con.confdeltype::text, conf.relname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_class conf ON conf.oid = con.confrelid
          WHERE con.contype = 'f' AND cls.relname = 'idempotency_keys'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_idempotency_foreign_keys(&keys);

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes
          WHERE schemaname = 'public' AND tablename = 'idempotency_keys'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        indexes.contains(&"idempotency_keys_inserted_at_idx".to_owned()),
        "the sweep's walk is indexed: {indexes:?}"
    );
}
