//! [`keyset_page`] and [`keyset_page_by`] against a real database, over a
//! small set of fixed ids the statement itself supplies (no schema changes).

use super::*;

/// How many ids the fixture holds: `1..=SEEDED`.
const SEEDED: u128 = 5;

/// The fixture id numbered `n`; a bigger number is a newer id.
const fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// The fixture ids `ns`, in the order given.
fn ids(ns: &[u128]) -> Vec<Uuid> {
    ns.iter().copied().map(id).collect()
}

/// Page parameters with only `limit` set.
const fn page(limit: i64) -> PageParams {
    PageParams {
        max_id: None,
        min_id: None,
        since_id: None,
        limit,
        offset: None,
    }
}

/// A statement over the fixture ids that has the keyset's `WHERE` to
/// continue.
fn fixture_builder() -> QueryBuilder<Postgres> {
    let mut builder = QueryBuilder::<Postgres>::new("SELECT id FROM unnest(");
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _ids: &mut _ = builder
        .push_bind((1..=SEEDED).map(id).collect::<Vec<Uuid>>())
        .push(") AS t(id) WHERE true");
    builder
}

/// Fetches the page `page` describes through [`keyset_page`], as fixture ids.
async fn fetch(pool: &PgPool, page: &PageParams) -> Vec<Uuid> {
    let rows: Vec<(Uuid,)> = keyset_page(pool, fixture_builder(), "id", page)
        .await
        .unwrap();
    rows.into_iter().map(|(row,)| row).collect()
}

#[tokio::test]
async fn first_page_is_newest_first_and_capped_at_limit() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    assert_eq!(fetch(&pool, &page(2)).await, ids(&[5, 4]), "the newest two");
    assert_eq!(
        fetch(&pool, &page(10)).await,
        ids(&[5, 4, 3, 2, 1]),
        "every row, newest-first"
    );
}

#[tokio::test]
async fn max_id_walks_to_older_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let older = PageParams {
        max_id: Some(id(4)),
        ..page(2)
    };
    assert_eq!(
        fetch(&pool, &older).await,
        ids(&[3, 2]),
        "the two rows below 4"
    );
    let last = PageParams {
        max_id: Some(id(2)),
        ..page(2)
    };
    assert_eq!(fetch(&pool, &last).await, ids(&[1]), "only 1 is below 2");
}

#[tokio::test]
async fn since_id_reads_newest_first_like_a_first_page() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let newer = PageParams {
        since_id: Some(id(2)),
        ..page(2)
    };
    assert_eq!(
        fetch(&pool, &newer).await,
        ids(&[5, 4]),
        "the newest two above 2"
    );
}

#[tokio::test]
async fn min_id_reads_ascending_then_flips_to_newest_first() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    // Ascending from just after the cursor is 3, 4; the page comes back
    // flipped, and it is not the newest rows (5, 4).
    let after = PageParams {
        min_id: Some(id(2)),
        ..page(2)
    };
    assert_eq!(
        fetch(&pool, &after).await,
        ids(&[4, 3]),
        "the two just above 2, flipped"
    );
}

#[tokio::test]
async fn keyset_page_by_binds_the_leading_placeholder() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let array = (1..=SEEDED)
        .map(|n| format!("'{}'::uuid", id(n)))
        .collect::<Vec<_>>()
        .join(", ");
    let stem = format!("SELECT id FROM unnest(ARRAY[{array}]) AS t(id) WHERE id <> ");
    // Bind the id 5 as the excluded row.
    let first: Vec<(Uuid,)> = keyset_page_by(&pool, &stem, id(5), "id", &page(3))
        .await
        .unwrap();
    let first_ids: Vec<Uuid> = first.into_iter().map(|(row,)| row).collect();
    assert_eq!(
        first_ids,
        ids(&[4, 3, 2]),
        "the bound id 5 is excluded, the rest newest-first"
    );

    let after = PageParams {
        min_id: Some(id(1)),
        ..page(2)
    };
    let flipped: Vec<(Uuid,)> = keyset_page_by(&pool, &stem, id(5), "id", &after)
        .await
        .unwrap();
    let flipped_ids: Vec<Uuid> = flipped.into_iter().map(|(row,)| row).collect();
    assert_eq!(
        flipped_ids,
        ids(&[3, 2]),
        "a min_id page after the bind is read ascending and flipped"
    );
}
