//! The `status_id` foreign-key indexes behind status deletes (WBE-529).

/// The six `(index name, table)` pairs the migration must create.
const EXPECTED: [(&str, &str); 6] = [
    ("bookmarks_status_id_idx", "bookmarks"),
    ("idempotency_keys_status_id_idx", "idempotency_keys"),
    ("notifications_chat_message_id_idx", "notifications"),
    ("notifications_from_account_id_idx", "notifications"),
    ("notifications_status_id_idx", "notifications"),
    ("pinned_statuses_status_id_idx", "pinned_statuses"),
];

/// The status-FK-indexes migration creates each index on its table.
#[tokio::test]
async fn status_fk_indexes_migration_creates_the_six_indexes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let names: Vec<&str> = EXPECTED.iter().map(|row| row.0).collect();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, tablename
           FROM pg_indexes
          WHERE schemaname = 'public' AND indexname = ANY($1)
       ORDER BY indexname",
    )
    .bind(&names)
    .fetch_all(&pool)
    .await
    .unwrap();
    let expected: Vec<(String, String)> = EXPECTED
        .iter()
        .map(|(name, table)| ((*name).to_owned(), (*table).to_owned()))
        .collect();
    assert_eq!(rows, expected, "each index exists on its table");
}

/// The two nullable columns get partial indexes; the rest are plain.
#[tokio::test]
async fn status_fk_indexes_are_partial_where_the_column_is_nullable() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname IN ('notifications_status_id_idx', 'notifications_chat_message_id_idx')
       ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows[0].1.contains("(chat_message_id)") && rows[0].1.contains("IS NOT NULL"),
        "{}",
        rows[0].1
    );
    assert!(
        rows[1].1.contains("(status_id)") && rows[1].1.contains("IS NOT NULL"),
        "{}",
        rows[1].1
    );
}
