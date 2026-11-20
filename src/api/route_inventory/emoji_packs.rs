//! The inventory's emoji-packs section.
//!
//! The rows the single table listed under its `Emoji packs` header:
//! Pleroma's admin pack API, whose three reads are public.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Emoji packs (Pleroma's admin pack API; the three reads are public) ─
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/emoji/packs",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/emoji/packs/archive",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/emoji/pack",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/emoji/pack",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/emoji/pack",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/emoji/pack",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/emoji/packs/import",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/emoji/packs/remote",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/emoji/packs/download",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/emoji/packs/download_zip",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/emoji/packs/files",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/emoji/packs/files",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/emoji/packs/files",
        auth: AuthClass::Admin,
    },
];
