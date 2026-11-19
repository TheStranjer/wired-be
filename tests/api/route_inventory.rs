//! The route-inventory conformance gate (WBE-221): every row of
//! [`wired_be::api::route_inventory::ROUTES`] is requested through the real
//! application router without credentials, and the answer must show that a
//! handler matched the route and that the row's [`AuthClass`] describes what
//! the handler demands.
//!
//! The router-level fallback in [`wired_be::app`] marks its own 404 with
//! `x-wired-fallback: 1`, so a row whose answer carries that header is a route
//! the router does not register at all — the difference between "no handler"
//! and "a handler that answered 404 because the sample id names nothing" is
//! exactly what this test is for. A gated row that answers anything but 401 or
//! 403 without credentials is the other half: a route that lets an anonymous
//! request through its auth gate.
//!
//! Both tests walk the whole table and report every bad row at once, so one
//! run names every gap rather than only the first.

#![cfg(test)]

use std::collections::HashSet;

use axum::{Router, http::StatusCode};
use uuid::Uuid;
use wired_be::api::route_inventory::{AuthClass, ROUTES, RouteSpec};

use crate::common;

/// The header the app-wide fallback answers with (`x-wired-fallback: 1`);
/// present means "no route matched".
const FALLBACK_HEADER: &str = "x-wired-fallback";

/// The parameter names the test fills with a freshly minted UUID: they are
/// entity ids, and a UUID is the only spelling that reaches the handler's own
/// not-found path instead of a malformed-id refusal.
const UUID_PARAMS: [&str; 7] = [
    "id",
    "uuid",
    "user_id",
    "report_id",
    "message_id",
    "account_id",
    "list_id",
];

/// A plausible fixed value for every other parameter name in the table.
///
/// `method` is `totp` because that MFA route is registered per method rather
/// than with a wildcard (the only method Wired implements); `name` is a
/// document name, which both `/instance/:name.html` and
/// `/api/v1/pleroma/admin/instance_document/:name` read.
const FIXED_PARAMS: [(&str, &str); 18] = [
    ("nickname", "alice"),
    ("instance", "remote.example"),
    ("tag", "wired"),
    ("token", "a1b2c3d4e5f6"),
    ("name", "terms"),
    ("app", "pleroma-fe"),
    ("group_key", "9f8e7d6c5b4a39281706f5e4d3c2b1a0"),
    ("provider", "passport"),
    ("method", "totp"),
    ("emoji", "thumbsup"),
    (
        "sig",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ),
    ("url", "aHR0cHM6Ly9leGFtcGxlLmNvbS9pbWFnZS5wbmc"),
    ("filename", "image.png"),
    ("version", "2.1"),
    ("permission_group", "admin"),
    ("key", "2026/01/02/media.png"),
    ("pack", "test-pack"),
    ("file", "emoji.png"),
];

/// `path` with every `:name` parameter replaced by a concrete segment, so the
/// request can match a route.
fn concrete_path(path: &str) -> String {
    let mut segments: Vec<String> = Vec::new();
    for segment in path.split('/') {
        segments.push(concrete_segment(segment));
    }
    segments.join("/")
}

/// One `/`-separated `segment` of an inventory path: either it is literal, or
/// it starts with a `:name` parameter and may carry a literal suffix after it
/// — Pleroma writes the instance-document route `/instance/:name.html`, where
/// `.html` is part of the path and `name` is what the handler reads.
fn concrete_segment(segment: &str) -> String {
    let Some(rest) = segment.strip_prefix(':') else {
        return (*segment).to_owned();
    };
    let name_len = rest
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(rest.len(), |(index, _)| index);
    let (name, suffix) = rest.split_at(name_len);
    let sample = sample_segment(name);
    format!("{sample}{suffix}")
}

/// The concrete segment standing in for the parameter `name`.
///
/// # Panics
///
/// If the table names a parameter this harness has no sample value for: a new
/// row needs its sample listed in [`UUID_PARAMS`] or [`FIXED_PARAMS`].
fn sample_segment(name: &str) -> String {
    if UUID_PARAMS.contains(&name) {
        return Uuid::now_v7().to_string();
    }
    FIXED_PARAMS
        .iter()
        .find(|(key, _)| *key == name)
        .map_or_else(
            || panic!("no sample value for the inventory parameter :{name}"),
            |(_key, value)| (*value).to_owned(),
        )
}

/// The request builder for the row's method, credential-less.
///
/// # Panics
///
/// If the table names a method this harness cannot send.
fn anonymous_request(spec: &RouteSpec) -> common::TestRequest {
    let path = concrete_path(spec.path);
    match spec.method {
        "GET" => common::TestRequest::get(&path),
        "POST" => common::TestRequest::post(&path),
        "PUT" => common::TestRequest::put(&path),
        "PATCH" => common::TestRequest::patch(&path),
        "DELETE" => common::TestRequest::delete(&path),
        other => panic!("unsupported inventory method {other}"),
    }
}

/// Whether `status` is the refusal of a credential the route demands.
fn refused_for_want_of_credentials(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

/// How an [`AuthClass`] is spelled in a failure message.
fn class_label(auth: AuthClass) -> &'static str {
    match auth {
        AuthClass::Public => "no credential",
        AuthClass::User => "a user token",
        AuthClass::Admin => "an admin token",
    }
}

/// What `spec`'s answer gets wrong, `None` when it conforms.
async fn problem_in(app: &Router, spec: &RouteSpec) -> Option<String> {
    let response = anonymous_request(spec).send(app).await;
    if response.header(FALLBACK_HEADER).is_some() {
        return Some(format!(
            "{} {} reached the fallback, so no handler is routed on it",
            spec.method, spec.path
        ));
    }
    if spec.auth == AuthClass::Public {
        return None;
    }
    if refused_for_want_of_credentials(response.status) {
        return None;
    }
    Some(format!(
        "{} {} needs {} and answers {} without credentials",
        spec.method,
        spec.path,
        class_label(spec.auth),
        response.status
    ))
}

/// Every row of the inventory, requested without credentials: a handler must
/// match it, and a `User` or `Admin` row must refuse the request.
#[tokio::test]
async fn every_inventory_route_is_routed_and_gated() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let mut problems: Vec<String> = Vec::new();
    for spec in ROUTES {
        if let Some(problem) = problem_in(&app, spec).await {
            problems.push(problem);
        }
    }
    assert!(
        problems.is_empty(),
        "{} of {} inventory routes are wrong:\n{}",
        problems.len(),
        ROUTES.len(),
        problems.join("\n")
    );
}

/// The table names each route once: two rows on one `(method, path)` would
/// measure the same route twice and hide a third that nobody asked for.
#[test]
fn inventory_names_no_route_twice() {
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut duplicates: Vec<String> = Vec::new();
    for spec in ROUTES {
        if !seen.insert((spec.method, spec.path)) {
            duplicates.push(format!("{} {}", spec.method, spec.path));
        }
    }
    assert!(
        duplicates.is_empty(),
        "{} duplicated inventory rows:\n{}",
        duplicates.len(),
        duplicates.join("\n")
    );
}
