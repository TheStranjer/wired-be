//! The partial indexes behind the expiry and poll-end sweeps (WBE-552).

/// The `(index name, table, definition)` rows of `pg_indexes` for
/// the two sweep indexes, by name.
async fn sweep_index_rows(pool: &sqlx::PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT indexname, tablename, indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname IN ('statuses_expires_at_live_idx', 'polls_expires_at_unnotified_idx')
       ORDER BY indexname",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The sweep-indexes migration creates both partial indexes, on the
/// tables and with the predicates `EXPIRED_SQL` and
/// `FETCH_ENDED_UNNOTIFIED_SQL` filter on.
#[tokio::test]
async fn sweep_indexes_migration_creates_both_partial_indexes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let rows = sweep_index_rows(&db.pool().clone()).await;
    let names: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "polls_expires_at_unnotified_idx",
            "statuses_expires_at_live_idx"
        ],
        "both sweep indexes exist"
    );

    let polls = &rows[0];
    assert_eq!(polls.1, "polls", "the poll index is on polls");
    assert!(
        polls.2.contains("(expires_at)"),
        "keyed on expires_at: {}",
        polls.2
    );
    assert!(
        polls.2.contains("WHERE") && polls.2.contains("ended_notified"),
        "partial on ended_notified: {}",
        polls.2
    );

    let statuses = &rows[1];
    assert_eq!(statuses.1, "statuses", "the status index is on statuses");
    assert!(
        statuses.2.contains("(expires_at, id)"),
        "keyed on (expires_at, id): {}",
        statuses.2
    );
    assert!(
        statuses.2.contains("deleted_at IS NULL"),
        "partial on live rows: {}",
        statuses.2
    );
}
