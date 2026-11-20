//! Tests for `GET /api/openapi` (WBE-227): the `OpenAPI` 3.0 document the
//! server generates from `src/api/route_inventory/` at request time —
//! valid JSON, one operation per inventory row, `{name}`-spelled path
//! parameters, the `oAuth2` security scheme and its `security` on the gated
//! routes.

#![cfg(test)]

use std::collections::HashMap;

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    api::route_inventory::{AuthClass, RouteSpec, routes},
    config::{DEFAULT_INSTANCE_NAME, TEST_BASE_URL},
};

use crate::common;

/// The methods a path item may carry operations under.
const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// The document as the anonymous caller gets it: 200, `application/json`,
/// and a body that parses as JSON.
///
/// # Panics
///
/// If the route refuses, mislabels itself, or answers unparsable bytes.
async fn fetch_document() -> Value {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let response = common::TestRequest::get("/api/openapi").send(&app).await;
    assert_eq!(response.status, StatusCode::OK, "GET /api/openapi status");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the document answers as JSON"
    );
    match serde_json::from_slice(&response.body) {
        Ok(document) => document,
        Err(err) => panic!("the body is valid JSON: {err}: {}", response.text()),
    }
}

/// The `operationId` the ticket's formula gives row `route`: the uppercase
/// method, then the inventory path with every non-alphanumeric character
/// replaced by an underscore.
fn expected_operation_id(route: &RouteSpec) -> String {
    let flattened: String = route
        .path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{}_{}", route.method, flattened)
}

/// Every operation of the document, by `operationId`.
///
/// # Panics
///
/// If a path item carries a key that is not a method, an operation without a
/// string `operationId`, or one `operationId` twice — the "exactly once"
/// half of the inventory match.
fn operations(document: &Value) -> HashMap<&str, &Value> {
    let paths = document
        .get("paths")
        .expect("the document carries a paths object")
        .as_object()
        .expect("paths is an object");
    let mut got: HashMap<&str, &Value> = HashMap::with_capacity(routes().count());
    for (path, item) in paths {
        let item = item.as_object().expect("a path item is an object");
        for (method, operation) in item {
            assert!(
                METHODS.contains(&method.as_str()),
                "{method} under {path} is an operation, not a path-item field"
            );
            let id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{method} {path} carries a string operationId"));
            assert!(
                got.insert(id, operation).is_none(),
                "operationId {id} appears twice"
            );
        }
    }
    got
}

/// The anonymous request answers 200 `application/json` with a valid `OpenAPI`
/// 3.0 document: the frame names the spec version, the instance and the crate
/// and offers the one configured server — and the document carries a paths
/// object that describes its own route.
#[tokio::test]
async fn openapi_answers_with_a_valid_document() {
    let document = fetch_document().await;
    assert_eq!(document["openapi"], "3.0.3", "the declared spec version");
    assert_eq!(
        document["info"]["title"],
        format!("{DEFAULT_INSTANCE_NAME} API"),
        "the instance names the document"
    );
    assert_eq!(
        document["info"]["version"],
        env!("CARGO_PKG_VERSION"),
        "the document versions with the crate"
    );
    assert_eq!(
        document["servers"],
        json!([{"url": TEST_BASE_URL}]),
        "the one configured server"
    );
    assert!(
        document["paths"].get("/api/openapi").is_some(),
        "the document describes its own route"
    );
}

/// The inventory and the document agree exactly: every row appears — no
/// more and no fewer operations than there are rows, and each row's own
/// `operationId` is present.
#[tokio::test]
async fn every_inventory_route_appears_exactly_once() {
    let document = fetch_document().await;
    let operations = operations(&document);
    assert_eq!(
        operations.len(),
        routes().count(),
        "one operation per inventory row"
    );
    for route in routes() {
        let id = expected_operation_id(route);
        assert!(
            operations.contains_key(id.as_str()),
            "inventory row {} {} ({id}) is in the document",
            route.method,
            route.path
        );
    }
}

/// Path parameters use `{name}` syntax in `paths`, never the inventory's
/// `:name` — literal suffixes such as `.html` stay outside the braces — and
/// each parameter is declared once as a required string path parameter.
#[tokio::test]
async fn inventory_paths_become_braced_templates() {
    let document = fetch_document().await;
    let paths = document
        .get("paths")
        .expect("the document carries a paths object");
    assert!(
        paths.get("/api/v1/accounts/:id").is_none(),
        "the raw :name spelling is not a template"
    );
    assert_eq!(
        paths["/api/v1/accounts/{id}"]["get"]["parameters"],
        json!([
            {"name": "id", "in": "path", "required": true, "schema": {"type": "string"}},
        ]),
        "one required string path parameter"
    );
    assert_eq!(
        paths["/instance/{name}.html"]["get"]["parameters"],
        json!([
            {"name": "name", "in": "path", "required": true, "schema": {"type": "string"}},
        ]),
        "the parameter is the name; the suffix stays outside the braces"
    );
    assert_eq!(
        paths["/users/{nickname}/statuses/{id}"]["get"]["parameters"],
        json!([
            {"name": "nickname", "in": "path", "required": true, "schema": {"type": "string"}},
            {"name": "id", "in": "path", "required": true, "schema": {"type": "string"}},
        ]),
        "both parameters, in path order"
    );
}

/// Every `User` and `Admin` operation asks for the `oAuth2` scheme and no
/// `Public` one carries a `security` entry.
#[tokio::test]
async fn gated_operations_secure_with_the_scheme_and_public_ones_do_not() {
    let document = fetch_document().await;
    let operations = operations(&document);
    for route in routes() {
        let id = expected_operation_id(route);
        let operation = operations
            .get(id.as_str())
            .unwrap_or_else(|| panic!("row {id} is in the document"));
        if route.auth == AuthClass::Public {
            assert!(
                operation.get("security").is_none(),
                "public {id} carries no security"
            );
        } else {
            assert_eq!(
                operation["security"],
                json!([{"oAuth2": []}]),
                "gated {id} asks for the scheme"
            );
        }
    }
}

/// `components.securitySchemes.oAuth2` is the `oauth2` authorization-code
/// flow on this instance's own `/oauth` endpoints, offering exactly the six
/// standard top-level scopes.
#[tokio::test]
async fn security_scheme_is_the_instance_oauth_flow() {
    let document = fetch_document().await;
    assert_eq!(
        document["components"]["securitySchemes"]["oAuth2"],
        json!({
            "type": "oauth2",
            "flows": {
                "authorizationCode": {
                    "authorizationUrl": format!("{TEST_BASE_URL}/oauth/authorize"),
                    "tokenUrl": format!("{TEST_BASE_URL}/oauth/token"),
                    "scopes": {
                        "read": "Read everything",
                        "write": "Write everything",
                        "follow": "Manage relationships",
                        "push": "Web Push API subscriptions",
                        "admin:read": "Read administrative data",
                        "admin:write": "Write administrative data",
                    },
                },
            },
        }),
        "the whole scheme, on the configured base url"
    );
}
