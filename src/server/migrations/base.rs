//! The base extensions, the statuses indexes, the instance settings and
//! rules tables and the list-members position.

use super::*;

/// The base extensions migration installs `citext`, `pg_trgm` and `pgcrypto`.
#[tokio::test]
async fn extensions_migration_installs_all_three_extensions() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let row: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_extension WHERE extname IN ('citext','pg_trgm','pgcrypto')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, 3, "all three extensions should be installed");
}

/// The statuses migrations create every index the timeline, thread
/// and reblog queries need — three of them partial on
/// `deleted_at IS NULL`, so tombstones drop out of them for free,
/// and one of those keeps the live reblog pair unique — and gives
/// `media_attachments.status_id` its foreign key into the
/// new table.
#[tokio::test]
async fn statuses_migration_indexes_statuses_and_links_media() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes
          WHERE schemaname = 'public' AND tablename = 'statuses'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_statuses_indexes(&rows);

    let keys: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT con.conname, con.confdeltype::text, con.confrelid::regclass::text
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
          WHERE cls.relname = 'media_attachments' AND con.contype = 'f'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let status_key = keys
        .iter()
        .find(|(name, _, _)| name == "media_attachments_status_id_fkey")
        .unwrap();
    assert_eq!(
        status_key.1, "c",
        "status_id's foreign key deletes its media"
    );
    assert_eq!(
        status_key.2, "statuses",
        "and points at the table this migration created"
    );
}

/// The instance-settings migration creates the tiny key/value
/// table `src/models/instance_setting.rs` reads: the `text`
/// primary key its upsert conflicts on, the jsonb value and the
/// `now()`-defaulted `updated_at`.
#[tokio::test]
async fn instance_settings_migration_creates_the_settings_table() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'instance_settings'
       ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("key".to_owned(), "text".to_owned(), "NO".to_owned()),
            (
                "updated_at".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned()
            ),
            ("value".to_owned(), "jsonb".to_owned(), "NO".to_owned()),
        ],
        "the ticket's three columns with the ticket's types"
    );

    let (primary_key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'instance_settings'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        key_columns, "key",
        "the key alone is the primary key {primary_key}"
    );

    let updated_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'instance_settings'
            AND column_name = 'updated_at'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        updated_default.as_deref(),
        Some("now()"),
        "updated_at defaults to now()"
    );
}

/// The instance-rules migration creates the ordered rule list
/// `src/models/instance_rule.rs` reads and writes: the minted
/// `uuid` primary key, the rule text, the defaulted hint and
/// priority, and the `now()`-stamped timestamps.
#[tokio::test]
async fn instance_rules_migration_creates_the_rules_table() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<RuleColumn> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'instance_rules'
       ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: Vec<&str> = columns
        .iter()
        .map(|column| column.column_name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "id",
            "text",
            "hint",
            "priority",
            "inserted_at",
            "updated_at"
        ],
        "the ticket's columns, in the ticket's order"
    );
    assert_rule_column_types(&columns);
    assert_eq!(
        rule_column(&columns, "id").column_default,
        None,
        "the id is minted in Rust (decision 4)"
    );
    assert_eq!(
        rule_column(&columns, "text").column_default,
        None,
        "text has no default: the rule is required"
    );
    assert_eq!(
        rule_column(&columns, "hint").column_default.as_deref(),
        Some("''::text"),
        "the hint defaults to empty"
    );
    assert_eq!(
        rule_column(&columns, "priority").column_default.as_deref(),
        Some("0"),
        "the priority defaults to the middle of the list"
    );
    assert_rule_timestamps(&columns);

    let (primary_key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'instance_rules'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        key_columns, "id",
        "the id alone is the primary key {primary_key}"
    );
}

/// The list-members position migration gives `list_members` the
/// ordinal `GET /api/v1/lists/:id/accounts` pages on: a `bigint`
/// `NOT NULL` column defaulted from the sequence it owns (so a plain
/// insert takes the next number, which is the order members were added
/// in), the primary key still the `(list_id, user_id)` pair that makes
/// membership idempotent, and one index serving a list's own positions.
#[tokio::test]
async fn list_members_position_migration_adds_the_ordinal() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (data_type, is_nullable, default): (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'list_members'
            AND column_name = 'position'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "bigint", "position is a bigint ordinal");
    assert_eq!(is_nullable, "NO", "and every membership has one");
    assert_eq!(
        default.as_deref(),
        Some("nextval('list_members_position_seq'::regclass)"),
        "the database fills it: {default:?}"
    );

    let sequence: (String,) =
        sqlx::query_as("SELECT pg_get_serial_sequence('list_members', 'position')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        sequence.0, "public.list_members_position_seq",
        "one sequence, owned by the column and dropped with it"
    );

    let key: Vec<String> = sqlx::query_scalar(
        "SELECT att.attname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'list_members'
       ORDER BY att.attname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        key,
        vec!["list_id".to_owned(), "user_id".to_owned()],
        "the pair is still the key: position is an order, not an id"
    );

    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes
          WHERE schemaname = 'public' AND tablename = 'list_members'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let (_, definition) = indexes
        .iter()
        .find(|(name, _)| name == "list_members_list_id_position_idx")
        .unwrap();
    assert!(
        definition.contains("list_id") && definition.contains("position"),
        "the page's index covers the list filter and its cursor: {definition}"
    );
}
