//! The inventory's auth and OAuth section.
//!
//! The rows the single table listed under its `Auth and OAuth` header:
//! app registration, the token endpoints and the sign-in surface.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Auth and OAuth ───────────────────────────────────────────────────
    RouteSpec {
        method: "POST",
        path: "/api/v1/apps",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/apps/verify_credentials",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/oauth/authorize",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/oauth/authorize",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/oauth/token",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/oauth/revoke",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/oauth/mfa/challenge",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/oauth/mfa",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/oauth/mfa/verify",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/oauth/registration_details",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/oauth/prepare_request",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/oauth/:provider",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/oauth/:provider/callback",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/oauth/register",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/oauth/external/consent",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/oauth/external/consent",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/oauth_tokens",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/oauth_tokens/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/apps",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/auth/password",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/password_reset/:token",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/password_reset",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/account/confirm_email/:user_id/:token",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/accounts/confirmation_resend",
        auth: AuthClass::Public,
    },
];
