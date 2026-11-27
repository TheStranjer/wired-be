//! The inventory's lists, filters, tags, markers, media, notifications and
//! push section.
//!
//! The rows the single table listed under its `Lists, filters, tags,
//! markers, media, notifications, push` header.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Lists, filters, tags, markers, media, notifications, push ────────
    RouteSpec {
        method: "GET",
        path: "/api/v1/lists",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/lists",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/lists/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/lists/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/lists/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/lists/:id/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/lists/:id/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/lists/:id/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/filters",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/filters",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/filters",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v2/filters",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v2/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v2/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/filters/:id/keywords",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v2/filters/:id/keywords",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/filters/keywords/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v2/filters/keywords/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v2/filters/keywords/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/filters/:id/statuses",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v2/filters/:id/statuses",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/filters/statuses/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v2/filters/statuses/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/tags/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/tags/:id/follow",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/tags/:id/unfollow",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/followed_tags",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/markers",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/markers",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/preferences",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/media",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/media/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/media/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v2/media",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/notifications/:id/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/notifications/clear",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/notifications/destroy_multiple",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/notifications/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications/unread_count",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications/policy",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/notifications/policy",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications/requests",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications/requests/merged",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications/policy",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v2/notifications/policy",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/notifications/read",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/notifications/read",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications/unread_count",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications/:group_key",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications/:group_key/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v2/notifications/:group_key/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/announcements",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/announcements/:id/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/reports",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/search",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/search",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/streaming/health",
        auth: AuthClass::Public,
    },
];
