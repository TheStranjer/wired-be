//! The inventory's accounts and social-graph section.
//!
//! The rows the single table listed under its `Accounts and the social
//! graph` header: `/api/v1/accounts` and the follow, block, mute, pin,
//! endorsement and relationship routes around an account.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Accounts and the social graph ────────────────────────────────────
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/verify_credentials",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/accounts/update_credentials",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/relationships",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/familiar_followers",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/search",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/lookup",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/:id/statuses",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/:id/followers",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/:id/following",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/:id/endorsements",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/accounts/:id/lists",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/follows",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/follow",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/unfollow",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/block",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/unblock",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/mute",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/unmute",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/note",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/pin",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/unpin",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/endorse",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/unendorse",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/accounts/:id/remove_from_followers",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/endorsements",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/blocks",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/mutes",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/domain_blocks",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/domain_blocks",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/domain_blocks",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/follow_requests",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/follow_requests/:id/authorize",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/follow_requests/:id/reject",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/suggestions",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/suggestions/:account_id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/suggestions",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/outgoing_follow_requests",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/accounts/:id/favourites",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/accounts/:id/endorsements",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/accounts/:id/scrobbles",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/accounts/:id/subscribe",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/accounts/:id/unsubscribe",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/birthdays",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/scrobble",
        auth: AuthClass::User,
    },
];
