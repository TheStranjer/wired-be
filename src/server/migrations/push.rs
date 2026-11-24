//! The Web Push subscriptions and instance-secrets tables (WBE-144).

/// A column of `web_push_subscriptions` or `instance_secrets`, as
/// `information_schema` describes it: `(name, type, nullability,
/// default)` — the tuple the two WBE-144 migrations are checked
/// against.
type PushColumn = (String, String, String, Option<String>);

/// The row of `columns` describing the column `name`, panicking
/// when the table has no such column.
///
/// # Panics
///
/// If `columns` does not carry that column.
fn push_column<'columns>(columns: &'columns [PushColumn], name: &str) -> &'columns PushColumn {
    columns
        .iter()
        .find(|(column, _, _, _)| column == name)
        .unwrap_or_else(|| panic!("the table has no column {name}"))
}

/// The `information_schema.columns` description of the named
/// table's columns, in declaration order.
async fn push_columns(pool: &sqlx::PgPool, table: &str) -> Vec<PushColumn> {
    sqlx::query_as::<_, PushColumn>(
        "SELECT column_name, data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = $1
       ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The types of every `web_push_subscriptions` column, per the
/// ticket's declaration.
fn assert_push_column_types(columns: &[PushColumn]) {
    for (name, data_type) in [
        ("id", "uuid"),
        ("user_id", "uuid"),
        ("token_id", "uuid"),
        ("endpoint", "text"),
        ("key_p256dh", "text"),
        ("key_auth", "text"),
        ("alerts", "jsonb"),
        ("policy", "text"),
    ] {
        assert_eq!(
            push_column(columns, name).1,
            data_type,
            "{name} is {data_type}"
        );
    }
}

/// The nullability of every `web_push_subscriptions` column: the
/// ticket's `NOT NULL`s, and the one column it leaves nullable.
fn assert_push_column_nullability(columns: &[PushColumn]) {
    for (name, nullable) in [
        ("id", "NO"),
        ("user_id", "YES"),
        ("token_id", "NO"),
        ("endpoint", "NO"),
        ("key_p256dh", "NO"),
        ("key_auth", "NO"),
        ("alerts", "NO"),
        ("policy", "NO"),
        ("inserted_at", "NO"),
        ("updated_at", "NO"),
    ] {
        assert_eq!(
            push_column(columns, name).2,
            nullable,
            "{name} is {nullable}"
        );
    }
}

/// The defaults of `web_push_subscriptions`: the two the API
/// leans on, the two timestamps, and the columns the model
/// always writes itself, which must default nothing.
fn assert_push_column_defaults(columns: &[PushColumn]) {
    for (name, default) in [
        ("alerts", "'{}'::jsonb"),
        ("policy", "'all'::text"),
        ("inserted_at", "now()"),
        ("updated_at", "now()"),
    ] {
        assert_eq!(
            push_column(columns, name).3.as_deref(),
            Some(default),
            "{name} defaults to {default}"
        );
    }
    for name in [
        "id",
        "user_id",
        "token_id",
        "endpoint",
        "key_p256dh",
        "key_auth",
    ] {
        assert_eq!(
            push_column(columns, name).3,
            None,
            "{name} is written, not defaulted"
        );
    }
}

/// The foreign keys of `web_push_subscriptions`: both cascade, so
/// a departed account or token takes the subscription with it.
fn assert_push_foreign_keys(keys: &[(String, String, String)]) {
    for (constraint, action, target) in [
        ("web_push_subscriptions_user_id_fkey", "c", "users"),
        ("web_push_subscriptions_token_id_fkey", "c", "oauth_tokens"),
    ] {
        let found = keys
            .iter()
            .find(|(name, _, _)| name == constraint)
            .unwrap_or_else(|| panic!("{constraint} links the table"));
        assert_eq!(found.1, action, "{constraint} deletes as {action}");
        assert_eq!(found.2, target, "{constraint} points at {target}");
    }
}

/// The Web Push subscription migration (WBE-144) creates the
/// table `src/models/web_push_subscription.rs` reads and writes,
/// keyed the way the API needs it: one subscription per access
/// token (the unique `token_id` the create-upsert conflicts on),
/// the owner and the token each cascading the row away with them,
/// and the ticket's column types, defaults and nullability.
#[tokio::test]
async fn web_push_subscriptions_migration_keys_one_row_per_token() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns = push_columns(&pool, "web_push_subscriptions").await;
    let names: Vec<&str> = columns
        .iter()
        .map(|(name, _, _, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "id",
            "user_id",
            "token_id",
            "endpoint",
            "key_p256dh",
            "key_auth",
            "alerts",
            "policy",
            "inserted_at",
            "updated_at",
        ],
        "the ticket's columns, in the ticket's order"
    );
    assert_push_column_types(&columns);
    assert_push_column_nullability(&columns);
    assert_push_column_defaults(&columns);

    let uniques: Vec<String> = sqlx::query_scalar(
        "SELECT con.conname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
          WHERE con.contype = 'u' AND cls.relname = 'web_push_subscriptions'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        uniques,
        vec!["web_push_subscriptions_token_id_key".to_owned()],
        "one subscription per access token, the create-upsert's target"
    );

    let keys: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT con.conname, con.confdeltype::text, conf.relname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_class conf ON conf.oid = con.confrelid
          WHERE con.contype = 'f' AND cls.relname = 'web_push_subscriptions'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_push_foreign_keys(&keys);
}

/// The instance-secrets migration (WBE-144) creates the tiny
/// name/value table `src/models/instance_secret.rs` reads: the
/// `text` primary key its upserts conflict on and the `NOT NULL`
/// value, with nothing defaulted — every row is a write.
#[tokio::test]
async fn instance_secrets_migration_creates_the_secrets_table() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns = push_columns(&pool, "instance_secrets").await;
    assert_eq!(
        columns,
        vec![
            ("name".to_owned(), "text".to_owned(), "NO".to_owned(), None),
            ("value".to_owned(), "text".to_owned(), "NO".to_owned(), None),
        ],
        "the ticket's two columns, neither defaulted"
    );

    let (key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'instance_secrets'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        key_columns, "name",
        "the name alone is the primary key {key}"
    );
}

/// The user-id index migration (WBE-512) creates the index behind
/// `FOR_USER_SQL`'s `WHERE user_id = $1 ORDER BY id`, keyed on
/// `(user_id, id)` so the per-notification fan-out lookup needs no
/// sequential scan or sort.
#[tokio::test]
async fn web_push_subscriptions_user_id_index_serves_the_fan_out_lookup() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT tablename, indexdef
           FROM pg_indexes
          WHERE schemaname = 'public' AND indexname = 'web_push_subscriptions_user_id_idx'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "the user-id index exists exactly once");
    let (table, definition) = &rows[0];
    assert_eq!(table, "web_push_subscriptions", "the index is on the table");
    assert!(
        definition.contains("(user_id, id)"),
        "keyed on (user_id, id): {definition}"
    );
    assert!(
        !definition.contains("UNIQUE"),
        "not unique, a user has many devices: {definition}"
    );
}
