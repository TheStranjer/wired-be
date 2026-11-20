//! The inventory's instance-metadata and public-read section.
//!
//! The rows the single table opened with, under its `Instance and other
//! public reads` header: Pleroma's public `/api/v1/instance` family, the
//! credential-free reads scattered across the API, and Wired's own
//! `GET /api/openapi` surface document.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Instance and other public reads ──────────────────────────────────
    // Wired's own route, not Pleroma's: the OpenAPI document this table
    // generates (WBE-227).
    RouteSpec {
        method: "GET",
        path: "/api/openapi",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/instance",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/instance/peers",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/instance/rules",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/instance/domain_blocks",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/instance/translation_languages",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/instance",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/custom_emojis",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/trends",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/directory",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/pleroma/frontend_configurations",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/emoji",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/captcha",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/healthcheck",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/federation_status",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/remote_interaction",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/manifest.json",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/user_exists",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/check_password",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/registration/:token",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/health",
        auth: AuthClass::Public,
    },
];
