//! The keyset query builder and the `Link` headers.

use super::*;

#[test]
fn no_cursor_orders_descending_and_limits() {
    assert_eq!(
        keyset_sql("", "id"),
        "SELECT id FROM statuses WHERE 1=1 ORDER BY id DESC LIMIT $1",
        "first page"
    );
    assert_eq!(
        keyset_sql(&format!("max_id={MAX_ID}"), "id"),
        "SELECT id FROM statuses WHERE 1=1 AND id < $1 ORDER BY id DESC LIMIT $2",
        "max_id reads older rows, newest first"
    );
    assert_eq!(
        keyset_sql(&format!("since_id={SINCE_ID}"), "id"),
        "SELECT id FROM statuses WHERE 1=1 AND id > $1 ORDER BY id DESC LIMIT $2",
        "since_id stays descending"
    );
}

#[test]
fn min_id_orders_ascending() {
    assert_eq!(
        keyset_sql(&format!("min_id={MIN_ID}"), "id"),
        "SELECT id FROM statuses WHERE 1=1 AND id > $1 ORDER BY id ASC LIMIT $2",
        "the page just after the cursor, read forward"
    );
    assert_eq!(
        keyset_sql(&format!("min_id={MIN_ID}&max_id={MAX_ID}"), "id"),
        "SELECT id FROM statuses WHERE 1=1 AND id < $1 AND id > $2 ORDER BY id ASC LIMIT $3",
        "min_id decides the order, and comes after max_id"
    );
    assert_eq!(
        keyset_sql(
            &format!("max_id={MAX_ID}&min_id={MIN_ID}&since_id={SINCE_ID}"),
            "id"
        ),
        "SELECT id FROM statuses WHERE 1=1 AND id < $1 AND id > $2 AND id > $3 \
             ORDER BY id ASC LIMIT $4",
        "conditions in max/min/since order"
    );
}

#[test]
fn keyset_uses_the_given_id_column() {
    assert_eq!(
        keyset_sql(&format!("since_id={SINCE_ID}"), "s.id"),
        "SELECT id FROM statuses WHERE 1=1 AND s.id > $1 ORDER BY s.id DESC LIMIT $2",
        "the alias qualifies both condition and order"
    );
}

#[test]
fn finish_flips_only_min_id_pages() {
    assert_eq!(
        parse_query(&format!("min_id={MIN_ID}")).finish(vec![1, 2, 3]),
        vec![3, 2, 1],
        "an ascending fetch is answered newest-first"
    );
    assert_eq!(
        parse_query(&format!("max_id={MAX_ID}")).finish(vec![1, 2, 3]),
        vec![1, 2, 3],
        "a descending fetch stays as it is"
    );
    assert_eq!(
        parse_query("").finish(vec![1, 2, 3]),
        vec![1, 2, 3],
        "a first page stays as it is"
    );
    assert_eq!(
        parse_query(&format!("min_id={MIN_ID}")).finish(Vec::<i64>::new()),
        Vec::<i64>::new(),
        "nothing to flip on an empty page"
    );
}

#[test]
fn link_header_names_next_and_prev() {
    let link = link_header(
        "https://example.com",
        "/api/v1/timelines/public",
        &[],
        MIN_ID,
        MAX_ID,
    )
    .unwrap();
    assert_eq!(
        link,
        format!(
            "<https://example.com/api/v1/timelines/public?max_id={MAX_ID}>; rel=\"next\", \
                 <https://example.com/api/v1/timelines/public?min_id={MIN_ID}>; rel=\"prev\""
        ),
        "next walks to the oldest row, prev back to the newest"
    );
}

#[test]
fn link_header_repeats_and_encodes_extra_query() {
    let extra = vec![
        ("tag".to_owned(), "fedi verse".to_owned()),
        ("q".to_owned(), "a&b=c".to_owned()),
    ];
    let link = link_header("https://h", "/api/v1/timelines/tag/x", &extra, "n1", "o1").unwrap();
    assert_eq!(
        link,
        "<https://h/api/v1/timelines/tag/x?tag=fedi+verse&q=a%26b%3Dc&max_id=o1>; \
             rel=\"next\", <https://h/api/v1/timelines/tag/x?tag=fedi+verse&q=a%26b%3Dc&min_id=n1>; \
             rel=\"prev\"",
        "extras come before the cursor, form-encoded"
    );
}

#[test]
fn an_empty_keyset_page_gets_no_link_header() {
    for (newest, oldest) in [("", ""), ("n1", ""), ("", "o1")] {
        assert_eq!(
            link_header("https://h", "/p", &[], newest, oldest),
            None,
            "empty ids (newest={newest:?}, oldest={oldest:?}) mean no page"
        );
    }
}

#[test]
fn offset_link_header_walks_by_pages() {
    let base = ("https://h", "/api/pleroma/admin/users", &[][..]);
    let first = offset_link_header(base.0, base.1, base.2, &parse_query("limit=20"), 20).unwrap();
    assert_eq!(
        first, "<https://h/api/pleroma/admin/users?offset=20>; rel=\"next\"",
        "a full first page may have a next, it has no prev"
    );

    let middle = offset_link_header(
        base.0,
        base.1,
        base.2,
        &parse_query("limit=20&offset=40"),
        20,
    )
    .unwrap();
    assert_eq!(
        middle,
        "<https://h/api/pleroma/admin/users?offset=60>; rel=\"next\", \
             <https://h/api/pleroma/admin/users?offset=20>; rel=\"prev\"",
        "a full middle page links both ways"
    );

    let tail = offset_link_header(
        base.0,
        base.1,
        base.2,
        &parse_query("limit=20&offset=40"),
        5,
    )
    .unwrap();
    assert_eq!(
        tail, "<https://h/api/pleroma/admin/users?offset=20>; rel=\"prev\"",
        "a short later page ends the listing"
    );

    let stepped = offset_link_header(
        base.0,
        base.1,
        base.2,
        &parse_query("limit=20&offset=10"),
        20,
    )
    .unwrap();
    assert!(
        stepped.contains("offset=0>; rel=\"prev\""),
        "one page back from 10 stops at 0, not below: {stepped}"
    );

    let filtered = offset_link_header(
        base.0,
        base.1,
        &[("domain".to_owned(), "a b".to_owned())],
        &parse_query("limit=1&offset=1"),
        1,
    )
    .unwrap();
    assert_eq!(
        filtered,
        "<https://h/api/pleroma/admin/users?domain=a+b&offset=2>; rel=\"next\", \
             <https://h/api/pleroma/admin/users?domain=a+b&offset=0>; rel=\"prev\"",
        "extras are repeated on offset links too"
    );
}

#[test]
fn an_empty_or_terminal_offset_page_gets_no_link_header() {
    let params = parse_query("limit=20&offset=40");
    assert_eq!(
        offset_link_header("https://h", "/p", &[], &params, 0),
        None,
        "an empty page links nowhere"
    );
    assert_eq!(
        offset_link_header("https://h", "/p", &[], &parse_query("limit=20"), 5),
        None,
        "a short first page is the whole listing"
    );
}

#[test]
fn offset_link_header_survives_a_saturated_next() {
    let params = parse_query(&format!("limit=1&offset={}", i64::MAX));
    let link = offset_link_header("https://h", "/p", &[], &params, 1).unwrap();
    assert!(
        link.contains(&format!("offset={}>; rel=\"next\"", i64::MAX)),
        "an absurd offset saturates instead of wrapping: {link}"
    );
}

#[test]
fn set_link_header_writes_the_value_or_nothing() {
    let mut written = HeaderMap::new();
    set_link_header(&mut written, Some("<https://h/p?max_id=o>; rel=\"next\"")).unwrap();
    assert_eq!(
        written.get(LINK).unwrap().to_str().unwrap(),
        "<https://h/p?max_id=o>; rel=\"next\"",
        "the header is stored verbatim"
    );

    let mut untouched = HeaderMap::new();
    set_link_header(&mut untouched, None).unwrap();
    assert!(
        !untouched.contains_key(LINK),
        "an empty page sends no Link header"
    );
}

#[test]
fn set_link_header_rejects_bytes_that_are_not_header_safe() {
    let mut headers = HeaderMap::new();
    let err = set_link_header(&mut headers, Some("evil\nvalue"))
        .expect_err("a newline cannot ride in a header");
    assert!(matches!(err, AppError::Internal(_)), "a server bug: {err}");
}

/// Three ids in ascending order: oldest, middle, newest.
fn three_ids() -> (Uuid, Uuid, Uuid) {
    (
        Uuid::parse_str(MAX_ID).unwrap(),
        Uuid::parse_str(MIN_ID).unwrap(),
        Uuid::parse_str(SINCE_ID).unwrap(),
    )
}

#[test]
fn keyset_link_of_an_empty_page_is_none() {
    let rows: [Uuid; 0] = [];
    assert_eq!(
        keyset_link("https://h", "/p", &NO_EXTRA_QUERY, &rows, |row| *row),
        None,
        "an empty page links nowhere"
    );
}

#[test]
fn keyset_link_of_one_row_points_both_ways_at_it() {
    let (only, _, _) = three_ids();
    let link = keyset_link("https://h", "/p", &NO_EXTRA_QUERY, &[only], |row| *row).unwrap();
    assert_eq!(
        link,
        link_header("https://h", "/p", &[], &only.to_string(), &only.to_string()).unwrap(),
        "the same value link_header builds"
    );
    assert!(
        link.contains(&format!("max_id={only}>; rel=\"next\"")),
        "{link}"
    );
    assert!(
        link.contains(&format!("min_id={only}>; rel=\"prev\"")),
        "{link}"
    );
}

#[test]
fn keyset_link_of_three_rows_uses_last_for_next_and_first_for_prev() {
    let (oldest, middle, newest) = three_ids();
    let rows = [(newest, 1), (middle, 2), (oldest, 3)];
    let link = keyset_link("https://h", "/p", &NO_EXTRA_QUERY, &rows, |row| row.0).unwrap();
    assert!(
        link.contains(&format!("max_id={oldest}>; rel=\"next\"")),
        "{link}"
    );
    assert!(
        link.contains(&format!("min_id={newest}>; rel=\"prev\"")),
        "{link}"
    );
    assert!(
        !link.contains(&middle.to_string()),
        "the middle row is not a cursor: {link}"
    );
}

#[test]
fn keyset_link_preserves_the_extra_query() {
    let (oldest, _, newest) = three_ids();
    let extra = [("limit".to_owned(), "5".to_owned())];
    let link = keyset_link("https://h", "/p", &extra, &[newest, oldest], |row| *row).unwrap();
    assert_eq!(
        link,
        link_header(
            "https://h",
            "/p",
            &extra,
            &newest.to_string(),
            &oldest.to_string()
        )
        .unwrap(),
        "the same value link_header builds with the extra pairs"
    );
    assert_eq!(link.matches("limit=5").count(), 2, "on both links: {link}");
}

#[test]
fn link_for_page_handles_empty_one_and_two_entries() {
    let none: [i64; 0] = [];
    assert_eq!(
        link_for_page("https://h", "/p", &none, |n| *n),
        None,
        "an empty page links nowhere"
    );

    let one = link_for_page("https://h", "/p", &[7_i64], |n| *n).unwrap();
    assert_eq!(
        one,
        link_header("https://h", "/p", &[], "7", "7").unwrap(),
        "one entry is both cursors"
    );

    let two = link_for_page("https://h", "/p", &[9_i64, 4], |n| *n).unwrap();
    assert_eq!(
        two,
        link_header("https://h", "/p", &[], "9", "4").unwrap(),
        "first entry is the prev cursor, last the next"
    );
    assert!(two.contains("max_id=4>; rel=\"next\""), "{two}");
    assert!(two.contains("min_id=9>; rel=\"prev\""), "{two}");
}
