//! The inventory's account-settings section.
//!
//! The rows the single table listed under its `Account settings
//! (/api/pleroma)` header: the `/api/pleroma` per-user settings surface.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Account settings (`/api/pleroma`) ────────────────────────────────
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/change_email",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/change_password",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/delete_account",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/disable_account",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/pleroma/notification_settings",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/move_account",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/pleroma/aliases",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/pleroma/aliases",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/pleroma/aliases",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/mutes_import",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/blocks_import",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/follow_import",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/pleroma/accounts/mfa",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/pleroma/accounts/mfa/backup_codes",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/pleroma/accounts/mfa/setup/:method",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/accounts/mfa/confirm/:method",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/pleroma/accounts/mfa/:method",
        auth: AuthClass::User,
    },
];
