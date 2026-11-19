//! `GET /api/openapi`: the `OpenAPI` 3.0 document of this instance
//! (`docs/ARCHITECTURE.md` decision 1, WBE-227).
//!
//! Pleroma builds its spec at compile time from the Phoenix router
//! (`lib/pleroma/web/api_spec.ex`, `OpenApiSpex.Paths.from_router`). Wired's
//! one surface table is [`crate::api::route_inventory::ROUTES`], so the
//! document is generated at request time from it: one `paths` entry per
//! inventory path, one operation per row, every `:name` parameter spelled as
//! a `{name}` template segment and declared as a required string path
//! parameter, and the `oAuth2` security scheme named by every gated
//! operation. The document therefore says exactly what the router is measured
//! against: a route that joins the inventory joins the spec with it.
//!
//! The shape is the ticket's: `openapi` `3.0.3`; `info` names the instance
//! (`Config::instance_name`) and the crate version; `servers` is the one
//! `Config::base_url`; every operation carries the generic `200`-`OK`
//! response and, when its row demands a credential,
//! `"security": [{"oAuth2": []}]`.

use std::collections::BTreeMap;

use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Map, Value, json};

use crate::{
    api::route_inventory::{AuthClass, ROUTES, RouteSpec},
    state::AppState,
};

/// The `OpenAPI` specification version the document declares.
const OPENAPI_VERSION: &str = "3.0.3";

/// The router of the surface document, merged by [`crate::api::router`] at
/// its `/api` root.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/openapi", get(spec))
}

/// `GET /api/openapi` — the `OpenAPI` 3.0 document, generated from [`ROUTES`]
/// at request time. No authentication: the surface description is public.
async fn spec(State(state): State<AppState>) -> Json<Value> {
    Json(document(
        &state.config.instance_name,
        &state.config.base_url,
    ))
}

/// The whole document, as the instance named `instance_name` and based at
/// `base_url` answers it.
#[must_use]
fn document(instance_name: &str, base_url: &str) -> Value {
    json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": format!("{instance_name} API"),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "servers": [{"url": base_url}],
        "paths": paths(),
        "components": {
            "securitySchemes": {"oAuth2": security_scheme(base_url)},
        },
    })
}

/// The `paths` object: one entry per inventory path, each entry carrying one
/// operation per row routed on that path, keyed by the lowercase method.
fn paths() -> Value {
    let mut items: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for route in ROUTES {
        let item = items.entry(openapi_path(route.path)).or_default();
        let _replaced = item.insert(
            route.method.to_ascii_lowercase(),
            Value::Object(operation(route)),
        );
    }
    items
        .into_iter()
        .map(|(path, item)| (path, Value::Object(item)))
        .collect()
}

/// One row's operation: its `operationId`, the `parameters` its path spells,
/// the `security` its credential class demands (none for a public row) and
/// the generic `200` answer.
fn operation(route: &RouteSpec) -> Map<String, Value> {
    let mut operation = Map::new();
    let _id = operation.insert(
        "operationId".to_owned(),
        Value::String(operation_id(route.method, route.path)),
    );
    let parameters = path_parameters(route.path);
    if !parameters.is_empty() {
        let _params = operation.insert("parameters".to_owned(), Value::Array(parameters));
    }
    if route.auth != AuthClass::Public {
        let _security = operation.insert("security".to_owned(), json!([{"oAuth2": []}]));
    }
    let _responses = operation.insert(
        "responses".to_owned(),
        json!({"200": {"description": "OK"}}),
    );
    operation
}

/// The row's `operationId`: the uppercase method, then the inventory path
/// with every non-alphanumeric character — separators, parameter sigils and
/// literal dots alike — replaced by an underscore.
fn operation_id(method: &str, path: &str) -> String {
    let flattened: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{method}_{flattened}")
}

/// The inventory path as an `OpenAPI` path template: every `:name` parameter
/// becomes `{name}`, and a literal suffix the inventory keeps outside the
/// parameter (`/instance/:name.html`) stays outside the braces.
fn openapi_path(path: &str) -> String {
    path.split('/')
        .map(|segment| match parameter_of(segment) {
            Some((name, suffix)) => format!("{{{name}}}{suffix}"),
            None => segment.to_owned(),
        })
        .collect::<Vec<String>>()
        .join("/")
}

/// The `:name` parameter one path `segment` spells, as its name and its
/// literal suffix — `:name.html` is `("name", ".html")` — or `None` for a
/// literal segment.
fn parameter_of(segment: &str) -> Option<(&str, &str)> {
    let rest = segment.strip_prefix(':')?;
    let name_len = rest
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(rest.len(), |(index, _)| index);
    let (name, suffix) = rest.split_at(name_len);
    Some((name, suffix))
}

/// The `parameters` array of a row: one required string path parameter per
/// `:name` the inventory path spells, in path order.
fn path_parameters(path: &str) -> Vec<Value> {
    path.split('/')
        .filter_map(parameter_of)
        .map(|(name, _suffix)| {
            json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": {"type": "string"},
            })
        })
        .collect()
}

/// The `oAuth2` scheme: an `oauth2` authorization-code flow on this
/// instance's own `/oauth` endpoints, offering the six standard top-level
/// scopes (`docs/ARCHITECTURE.md` decision 9).
fn security_scheme(base_url: &str) -> Value {
    json!({
        "type": "oauth2",
        "flows": {
            "authorizationCode": {
                "authorizationUrl": format!("{base_url}/oauth/authorize"),
                "tokenUrl": format!("{base_url}/oauth/token"),
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The methods a path item carries its operations under.
    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

    /// A document built for a fixed instance, as every test reads it.
    fn sample() -> Value {
        document("Wired", "https://wired.example")
    }

    /// The `paths` object of `document`.
    ///
    /// # Panics
    ///
    /// If the document carries no paths object, which [`document`] cannot
    /// produce.
    fn paths_of(document: &Value) -> &Map<String, Value> {
        document
            .get("paths")
            .expect("paths is an object")
            .as_object()
            .expect("paths is an object")
    }

    /// The operation the document holds for inventory row `route`.
    ///
    /// # Panics
    ///
    /// If the row is absent from the document, which [`paths`] guarantees
    /// against [`ROUTES`].
    fn operation_of<'doc>(paths: &'doc Map<String, Value>, route: &RouteSpec) -> &'doc Value {
        paths
            .get(openapi_path(route.path).as_str())
            .unwrap_or_else(|| panic!("{} is templated in the document", route.path))
            .get(route.method.to_ascii_lowercase())
            .unwrap_or_else(|| panic!("{} {} has its operation", route.method, route.path))
    }

    /// The document's frame names the `OpenAPI` version, the instance and the
    /// crate version, and offers the one configured server.
    #[test]
    fn document_describes_the_instance() {
        let document = sample();
        assert_eq!(document["openapi"], "3.0.3", "the declared spec version");
        assert_eq!(
            document["info"]["title"], "Wired API",
            "the instance names the document"
        );
        assert_eq!(
            document["info"]["version"],
            env!("CARGO_PKG_VERSION"),
            "the document versions with the crate"
        );
        assert_eq!(
            document["servers"][0]["url"], "https://wired.example",
            "the one server"
        );
    }

    /// Every row of the inventory appears exactly once: one operation per
    /// row, no operation for a route the table does not hold, and each
    /// operation identified by its row's `operationId`.
    #[test]
    fn every_inventory_route_appears_once() {
        let document = sample();
        let paths = paths_of(&document);
        let mut ids: Vec<&str> = Vec::new();
        for item in paths.values() {
            let item = item.as_object().expect("a path item is an object");
            for (method, operation) in item {
                assert!(
                    METHODS.contains(&method.as_str()),
                    "operation key {method} is a method"
                );
                let id = operation["operationId"]
                    .as_str()
                    .expect("an operation has its id");
                ids.push(id);
            }
        }
        assert_eq!(ids.len(), ROUTES.len(), "one operation per inventory row");
        for route in ROUTES {
            let operation = operation_of(paths, route);
            assert_eq!(
                operation["operationId"],
                operation_id(route.method, route.path),
                "{} {} documents itself",
                route.method,
                route.path
            );
        }
    }

    /// `:name` segments become `{name}` template segments, a literal suffix
    /// stays outside the braces, and a literal path is untouched.
    #[test]
    fn inventory_paths_become_openapi_templates() {
        assert_eq!(
            openapi_path("/api/v1/instance"),
            "/api/v1/instance",
            "a literal path"
        );
        assert_eq!(
            openapi_path("/api/v1/accounts/:id"),
            "/api/v1/accounts/{id}",
            "a plain parameter"
        );
        assert_eq!(
            openapi_path("/api/account/confirm_email/:user_id/:token"),
            "/api/account/confirm_email/{user_id}/{token}",
            "two parameters"
        );
        assert_eq!(
            openapi_path("/instance/:name.html"),
            "/instance/{name}.html",
            "a parameter with a literal suffix"
        );
        let document = sample();
        let paths = paths_of(&document);
        assert!(
            paths.contains_key("/api/v1/accounts/{id}"),
            "the template is the entry"
        );
        assert!(
            !paths.contains_key("/api/v1/accounts/:id"),
            "the raw spelling is not"
        );
    }

    /// Each `:param` is declared once, required, in path, of string type —
    /// and a route without parameters carries no `parameters` key.
    #[test]
    fn parameters_are_required_path_strings() {
        let document = sample();
        let paths = paths_of(&document);
        assert_eq!(
            paths["/users/{nickname}/statuses/{id}"]["get"]["parameters"],
            json!([
                {"name": "nickname", "in": "path", "required": true, "schema": {"type": "string"}},
                {"name": "id", "in": "path", "required": true, "schema": {"type": "string"}},
            ]),
            "both parameters, in path order"
        );
        let plain = paths["/api/v1/instance"]["get"]
            .as_object()
            .expect("an operation object");
        assert!(
            !plain.contains_key("parameters"),
            "a route without parameters declares none"
        );
    }

    /// The operation id is the uppercase method and the inventory path with
    /// every non-alphanumeric character replaced by one underscore.
    #[test]
    fn operation_ids_flatten_the_inventory_path() {
        assert_eq!(
            operation_id("GET", "/api/openapi"),
            "GET__api_openapi",
            "the document names itself"
        );
        assert_eq!(
            operation_id("GET", "/instance/:name.html"),
            "GET__instance__name_html",
            "sigils and dots flatten like separators"
        );
        assert_eq!(
            operation_id(
                "POST",
                "/api/v1/pleroma/admin/users/:nickname/permission_group/:permission_group"
            ),
            "POST__api_v1_pleroma_admin_users__nickname_permission_group__permission_group",
            "parameter underscores survive, each separator adds one"
        );
    }

    /// Every `User` and `Admin` row carries the `oAuth2` security entry and
    /// no `Public` row does.
    #[test]
    fn gated_routes_ask_for_the_scheme_and_public_ones_do_not() {
        let document = sample();
        let paths = paths_of(&document);
        for route in ROUTES {
            let operation = operation_of(paths, route);
            if route.auth == AuthClass::Public {
                assert!(
                    !operation
                        .as_object()
                        .expect("an operation object")
                        .contains_key("security"),
                    "public {} {} carries no security",
                    route.method,
                    route.path
                );
            } else {
                assert_eq!(
                    operation["security"],
                    json!([{"oAuth2": []}]),
                    "{} {} ({}) names the scheme",
                    route.method,
                    route.path,
                    if route.auth == AuthClass::Admin {
                        "admin"
                    } else {
                        "user"
                    }
                );
            }
        }
    }

    /// Every operation answers with the generic `200` `OK` response.
    #[test]
    fn operations_answer_with_the_generic_ok() {
        let document = sample();
        let paths = paths_of(&document);
        for route in ROUTES {
            assert_eq!(
                operation_of(paths, route)["responses"],
                json!({"200": {"description": "OK"}}),
                "the generic response of {} {}",
                route.method,
                route.path
            );
        }
    }

    /// The scheme is an `oauth2` authorization-code flow on the instance's
    /// own `/oauth` endpoints, offering exactly the six standard top-level
    /// scopes.
    #[test]
    fn scheme_is_the_oauth2_authorization_code_flow() {
        let document = sample();
        let scheme = &document["components"]["securitySchemes"]["oAuth2"];
        assert_eq!(scheme["type"], "oauth2", "an oauth2 scheme");
        let flow = &scheme["flows"]["authorizationCode"];
        assert_eq!(
            flow["authorizationUrl"], "https://wired.example/oauth/authorize",
            "the instance's own authorize endpoint"
        );
        assert_eq!(
            flow["tokenUrl"], "https://wired.example/oauth/token",
            "the instance's own token endpoint"
        );
        let scopes = flow["scopes"].as_object().expect("scopes is an object");
        for scope in [
            "read",
            "write",
            "follow",
            "push",
            "admin:read",
            "admin:write",
        ] {
            assert!(
                scopes.contains_key(scope),
                "the standard scope {scope} is offered"
            );
        }
        assert_eq!(scopes.len(), 6, "exactly the six top-level scopes");
    }
}
