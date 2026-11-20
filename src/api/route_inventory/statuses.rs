//! The inventory's statuses, timelines, polls and scheduled posts section.
//!
//! The rows the single table listed under its `Statuses, timelines, polls
//! and scheduled posts` header.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Statuses, timelines, polls and scheduled posts ────────────────────
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/statuses/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/statuses/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses/:id/context",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses/:id/favourited_by",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses/:id/reblogged_by",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses/:id/history",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses/:id/source",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/statuses/:id/quotes",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/reblog",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/unreblog",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/favourite",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/unfavourite",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/pin",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/unpin",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/bookmark",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/unbookmark",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/mute",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/unmute",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/statuses/:id/translate",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/favourites",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/bookmarks",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/timelines/home",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/timelines/direct",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/timelines/public",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/timelines/tag/:tag",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/timelines/list/:list_id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/polls/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/polls/:id/votes",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/scheduled_statuses",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/scheduled_statuses/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/scheduled_statuses/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/scheduled_statuses/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/statuses/:id/quotes",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/statuses/:id/reactions",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/statuses/:id/reactions/:emoji",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/statuses/:id/reactions/:emoji",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/statuses/:id/reactions/:emoji",
        auth: AuthClass::User,
    },
];
