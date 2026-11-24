//! The inventory's `ActivityPub`, discovery and other top-level routes
//! section.
//!
//! The rows the single table listed under its `ActivityPub, discovery and
//! the other top-level routes` header: the federation endpoints and the
//! top-level HTML and well-known routes, which close the table.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── ActivityPub, discovery and the other top-level routes ────────────
    RouteSpec {
        method: "GET",
        path: "/.well-known/webfinger",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/.well-known/host-meta",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/.well-known/nodeinfo",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/nodeinfo/:version",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/inbox",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/users/:nickname/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/outbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/users/:nickname/outbox",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/followers",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/following",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/collections/featured",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/statuses/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/statuses/:id/activity",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/objects/:uuid",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/objects/:uuid/replies",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/activities/:uuid",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/notice/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/ap/whoami",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/ap/upload_media",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/relay",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/relay/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/relay/following",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/relay/followers",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/internal/fetch",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/internal/fetch/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/main/ostatus",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/main/ostatus",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/authorize_interaction",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/:sig/:url",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/:sig/:url/:filename",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/preview/:sig/:url",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/preview/:sig/:url/:filename",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/media/:key",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/emoji/:pack/:file",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/instance/:name.html",
        auth: AuthClass::Public,
    },
];
