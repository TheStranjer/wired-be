//! Ordinal page parameters: defaults, cursors and the keyset.

use super::*;

#[test]
fn ordinal_pages_default_to_twenty_and_read_zero_as_every_row() {
    for (query, want) in [
        (None, Some(20)),
        (Some(""), Some(20)),
        (Some("limit=5"), Some(5)),
        (Some("limit=40"), Some(40)),
        (Some("limit=41"), Some(40)),
        (Some("limit=100"), Some(40)),
        (Some("limit=0"), None),
        (Some("limit=-3"), Some(20)),
        (Some("limit=abc"), Some(20)),
    ] {
        let params = OrdinalPageParams::from_query(query, MAX_LIMIT).unwrap();
        assert_eq!(params.limit, want, "`limit` of query {query:?}");
    }
}

#[test]
fn ordinal_cursors_are_numbers() {
    let params =
        OrdinalPageParams::from_query(Some("max_id=9&min_id=4&since_id=7"), MAX_LIMIT).unwrap();
    assert_eq!(
        params,
        OrdinalPageParams {
            max_id: Some(9),
            min_id: Some(4),
            since_id: Some(7),
            limit: Some(20),
        },
        "the three cursors and the default page"
    );
}

#[test]
fn an_ordinal_cursor_that_is_not_a_number_is_a_bad_request() {
    for value in ["", "nope", "1.5", MAX_ID] {
        for key in ["max_id", "min_id", "since_id"] {
            let query = format!("{key}={value}");
            let err = OrdinalPageParams::from_query(Some(&query), MAX_LIMIT)
                .expect_err(&format!("{query} must be rejected"));
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{query} is a 400, not {err}"
            );
            assert_eq!(err.to_string(), "Invalid id", "message for {query}");
        }
    }
}

#[test]
fn ordinal_keyset_orders_on_the_given_column() {
    let mut qb = QueryBuilder::new("SELECT position FROM list_members WHERE 1=1");
    let params = OrdinalPageParams::from_query(Some("max_id=9&limit=5"), MAX_LIMIT).unwrap();
    params.apply_keyset(&mut qb, "lm.position");
    assert_eq!(
        qb.into_sql(),
        "SELECT position FROM list_members WHERE 1=1 AND lm.position < $1 \
             ORDER BY lm.position DESC LIMIT $2",
        "the cursor walks to smaller positions, newest first"
    );
}

#[test]
fn ordinal_keyset_with_all_three_cursors_has_exact_sql() {
    let mut qb = QueryBuilder::new("SELECT position FROM list_members WHERE 1=1");
    OrdinalPageParams::from_query(Some("max_id=9&min_id=4&since_id=7&limit=5"), MAX_LIMIT)
        .unwrap()
        .apply_keyset(&mut qb, "lm.position");
    assert_eq!(
        qb.into_sql(),
        "SELECT position FROM list_members WHERE 1=1 AND lm.position < $1 \
             AND lm.position > $2 AND lm.position > $3 ORDER BY lm.position ASC LIMIT $4",
        "max, min and since conditions in order, ascending because min_id is present"
    );
}

#[test]
fn an_ordinal_page_without_a_limit_fetches_every_row() {
    let mut qb = QueryBuilder::new("SELECT position FROM list_members WHERE 1=1");
    OrdinalPageParams::from_query(Some("limit=0"), MAX_LIMIT)
        .unwrap()
        .apply_keyset(&mut qb, "position");
    assert_eq!(
        qb.into_sql(),
        "SELECT position FROM list_members WHERE 1=1 ORDER BY position DESC",
        "limit=0 appends no LIMIT at all"
    );
}

#[test]
fn an_ordinal_min_id_page_is_read_ascending_and_flipped() {
    let mut qb = QueryBuilder::new("SELECT position FROM list_members WHERE 1=1");
    let params = OrdinalPageParams::from_query(Some("min_id=4"), MAX_LIMIT).unwrap();
    params.apply_keyset(&mut qb, "position");
    assert_eq!(
        qb.into_sql(),
        "SELECT position FROM list_members WHERE 1=1 AND position > $1 \
             ORDER BY position ASC LIMIT $2",
        "the page just after the cursor, read forward"
    );
    assert_eq!(
        params.finish(vec![4, 5, 6]),
        vec![6, 5, 4],
        "and answered newest-first"
    );
    let first = OrdinalPageParams::from_query(None, MAX_LIMIT).unwrap();
    assert_eq!(first.finish(vec![4, 5, 6]), vec![4, 5, 6], "no flip");
}
