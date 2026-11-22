//! Parsing page parameters from a query string and from a request.

use super::*;

#[test]
fn a_missing_or_empty_query_gets_the_defaults() {
    for query in [None, Some(""), Some("foo=bar&baz=1")] {
        let params = PageParams::from_query(query, MAX_LIMIT).unwrap();
        assert_eq!(
            params,
            PageParams {
                max_id: None,
                min_id: None,
                since_id: None,
                limit: 20,
                offset: None,
            },
            "defaults for query {query:?}"
        );
    }
}

#[test]
fn limit_is_parsed_capped_and_defaulted() {
    for (query, want) in [
        ("limit=5", 5),
        ("limit=40", 40),
        ("limit=41", 40),
        ("limit=100", 40),
        ("limit=0", 20),
        ("limit=-3", 20),
        ("limit=abc", 20),
        ("limit=3.5", 20),
        ("limit=99999999999999999999999999", 20),
    ] {
        assert_eq!(parse_query(query).limit, want, "limit from {query}");
    }
}

#[test]
fn a_custom_default_sizes_absent_and_unusable_pages() {
    // The follow-request queue's pair: default 40, cap 80.
    for query in [
        None,
        Some(""),
        Some("limit=abc"),
        Some("limit=0"),
        Some("limit=-5"),
    ] {
        let params = PageParams::from_query_with_default(query, 40, 80).unwrap();
        assert_eq!(params.limit, 40, "default of 40 for query {query:?}");
    }
    for (query, want) in [("limit=5", 5), ("limit=60", 60), ("limit=100", 80)] {
        let params = PageParams::from_query_with_default(Some(query), 40, 80).unwrap();
        assert_eq!(params.limit, want, "explicit {query} under cap 80");
    }
    // The plain parse keeps Pleroma's 20 as its default.
    assert_eq!(
        PageParams::from_query(Some("limit=0"), 80).unwrap().limit,
        20,
        "from_query is from_query_with_default at the global default"
    );
}

#[test]
fn a_custom_cap_lets_bigger_pages_through() {
    for (query, cap, want) in [
        ("limit=60", 80, 60),
        ("limit=100", 80, 80),
        ("limit=41", 80, 41),
        ("limit=5", 80, 5),
        ("limit=abc", 80, 20),
    ] {
        let got = PageParams::from_query(Some(query), cap).unwrap();
        assert_eq!(got.limit, want, "limit {query} under cap {cap}");
    }
}

#[test]
fn id_params_are_parsed() {
    let params = parse_query(&format!(
        "max_id={MAX_ID}&min_id={MIN_ID}&since_id={SINCE_ID}"
    ));
    assert_eq!(params.max_id, Some(parse_id(MAX_ID).unwrap()), "max_id");
    assert_eq!(params.min_id, Some(parse_id(MIN_ID).unwrap()), "min_id");
    assert_eq!(
        params.since_id,
        Some(parse_id(SINCE_ID).unwrap()),
        "since_id"
    );
    let short = PageParams::from_query(
        Some(&format!("max_id={}", MAX_ID.replace('-', ""))),
        MAX_LIMIT,
    )
    .unwrap();
    assert_eq!(
        short.max_id,
        Some(parse_id(MAX_ID).unwrap()),
        "the hyphenless spelling is an id too"
    );
}

#[test]
fn an_id_param_that_is_not_an_id_is_a_bad_request() {
    for value in ["nope", "not-an-id", "", SINCE_ID.replace('0', "g").as_str()] {
        for key in ["max_id", "min_id", "since_id"] {
            let query = format!("{key}={value}");
            let err = PageParams::from_query(Some(&query), MAX_LIMIT)
                .expect_err(&format!("{query} must be rejected"));
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{query} is a 400, not {err}"
            );
            assert_eq!(err.to_string(), "Invalid id", "message for {query}");
            let response = err.into_response();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "status for {query}"
            );
        }
    }
}

#[test]
fn offset_must_be_a_number_and_cannot_be_negative() {
    assert_eq!(parse_query("offset=40").offset, Some(40), "a real offset");
    for query in ["offset=x", "offset=-1", "offset=1.5"] {
        assert_eq!(
            parse_query(query).offset,
            None,
            "{query} is no usable offset"
        );
    }
}

#[test]
fn duplicate_keys_keep_the_last_value() {
    let params = parse_query(&format!("limit=5&limit=7&max_id={MIN_ID}&max_id={MAX_ID}"));
    assert_eq!(params.limit, 7, "last limit wins");
    assert_eq!(
        params.max_id,
        Some(parse_id(MAX_ID).unwrap()),
        "last max_id"
    );
}

#[test]
fn percent_encoded_values_are_decoded_before_parsing() {
    let params = parse_query("max_id=01890b9a%2D2c1d%2D7f3a%2D9e4b%2D7c2d5a8b1e3f");
    assert_eq!(
        params.max_id,
        Some(parse_id(MAX_ID).unwrap()),
        "%2D is a hyphen"
    );
}

#[tokio::test]
async fn the_extractor_parses_the_request_uri_with_the_default_cap() {
    let mut parts = parts_for(&format!(
        "/api/v1/timelines/public?limit=100&max_id={MAX_ID}"
    ));
    let params = PageParams::from_request_parts(&mut parts, &())
        .await
        .unwrap();
    assert_eq!(params.limit, 40, "the request cap is MAX_LIMIT");
    assert_eq!(params.max_id, Some(parse_id(MAX_ID).unwrap()), "max_id");

    let capped_parts = parts_for(&format!(
        "/api/v1/timelines/public?limit=100&min_id={MIN_ID}"
    ));
    let capped = PageParams::with_cap(80, &capped_parts).unwrap();
    assert_eq!(capped.limit, 80, "with_cap raises the cap");
    assert_eq!(capped.min_id, Some(parse_id(MIN_ID).unwrap()), "min_id");
}

#[tokio::test]
async fn the_extractor_rejects_a_malformed_id() {
    let mut parts = parts_for("/api/v1/timelines/home?max_id=nope");
    let err = PageParams::from_request_parts(&mut parts, &())
        .await
        .expect_err("a bogus id must reject");
    assert_eq!(err.to_string(), "Invalid id", "the rejection message");
}
