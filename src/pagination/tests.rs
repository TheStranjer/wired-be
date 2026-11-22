//! Tests for pagination: the fixtures every group shares, and the groups as
//! child modules.

mod keyset_links;
mod ordinal;
mod params;

use axum::{
    http::{Request, StatusCode, Uri},
    response::IntoResponse as _,
};
use sqlx::SqlStr;

use super::*;

/// A valid `max_id` cursor.
const MAX_ID: &str = "01890b9a-2c1d-7f3a-9e4b-7c2d5a8b1e3f";
/// A valid `min_id` cursor, a newer id than `MAX_ID`.
const MIN_ID: &str = "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d";
/// A valid `since_id` cursor, a newer id than `MIN_ID`.
const SINCE_ID: &str = "01890b9a-2c1f-7d7e-9a0b-3c4d5e6f7a8b";

/// Parses `query` with the default cap, expecting success.
fn parse_query(query: &str) -> PageParams {
    PageParams::from_query(Some(query), MAX_LIMIT).unwrap()
}

/// Request parts carrying `uri`, the way the extractor receives them.
fn parts_for(uri: &str) -> Parts {
    Request::get(uri.parse::<Uri>().unwrap())
        .body(())
        .unwrap()
        .into_parts()
        .0
}

/// The SQL these parameters append to a dummy select on `column`.
fn keyset_sql(query: &str, column: &str) -> SqlStr {
    let params = parse_query(query);
    let mut qb = QueryBuilder::new("SELECT id FROM statuses WHERE 1=1");
    params.apply_keyset(&mut qb, column);
    qb.into_sql()
}
