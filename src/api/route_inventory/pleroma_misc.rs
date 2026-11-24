//! The inventory's conversations, chats and rest-of-`/pleroma` section.
//!
//! The rows the single table listed under its `Conversations, chats and the
//! rest of /pleroma` header: the `/api/v1/pleroma` surface outside the
//! account-settings and admin scopes.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Conversations, chats and the rest of `/pleroma` ──────────────────
    RouteSpec {
        method: "GET",
        path: "/api/v1/conversations",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/conversations/:id/read",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/conversations/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/conversations/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/conversations/:id/statuses",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/conversations/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/conversations/read",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/chats/by-account-id/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/chats",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/pleroma/chats",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/chats/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/chats/:id/messages",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/chats/:id/messages",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/chats/:id/messages/:message_id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/chats/:id/read",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/chats/:id/messages/:message_id/read",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/chats/:id/pin",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/chats/:id/unpin",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/mascot",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/mascot",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/backups",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/backups",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/bookmark_folders",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/bookmark_folders",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/bookmark_folders/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/bookmark_folders/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/settings/:app",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/settings/:app",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v0/pleroma/reports",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v0/pleroma/reports/:id",
        auth: AuthClass::User,
    },
];
