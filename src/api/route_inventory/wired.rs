//! The inventory's Wired extension section.
//!
//! Wired's own routes under `/api/v1/wired` (`docs/ARCHITECTURE.md` decision
//! 17). Pleroma has none of them; they are listed so the same conformance
//! test routes and gates them.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Wired extensions ─────────────────────────────────────────────────
    RouteSpec {
        method: "GET",
        path: "/api/v1/wired/protocols",
        auth: AuthClass::User,
    },
];
