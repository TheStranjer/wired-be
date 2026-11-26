//! The inventory's Mastodon admin API section.
//!
//! The rows the single table listed under its `Admin API, Mastodon's own
//! (/api/v1/admin, /api/v2/admin)` header.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Admin API, Mastodon's own (`/api/v1/admin`, `/api/v2/admin`) ──────
    RouteSpec {
        method: "GET",
        path: "/api/v1/admin/accounts",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/admin/accounts/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/admin/accounts/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/accounts/:id/enable",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/accounts/:id/unsuspend",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/accounts/:id/action",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/accounts/:id/approve",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/accounts/:id/reject",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/admin/reports",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/admin/reports/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/reports/:id/resolve",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/reports/:id/reopen",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/reports/:id/assign_to_self",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/admin/reports/:id/unassign",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/admin/accounts",
        auth: AuthClass::Admin,
    },
];
