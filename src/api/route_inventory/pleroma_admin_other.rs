//! The inventory's Pleroma admin API, everything outside the user scope.
//!
//! The rest of the single table's `Admin API, Pleroma's own
//! (/api/v1/pleroma/admin)` section — statuses, chats, reports, relays,
//! config, frontend, rules, webhooks, announcements and the other admin
//! reads — following [`super::pleroma_admin_users`]'s `users` scope in
//! table order.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/statuses",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/statuses/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/admin/statuses/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/statuses/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/chats/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/chats/:id/messages",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/chats/:id/messages/:message_id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/instances/:instance/statuses",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/instances/:instance",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/reports",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/reports/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/reports",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/reports/assign_account",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/reports/:id/notes",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/reports/:report_id/notes/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/relay",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/relay",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/relay",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/instance_document/:name",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/instance_document/:name",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/instance_document/:name",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/config",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/config",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/config/descriptions",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/need_reboot",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/restart",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/oauth_app",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/oauth_app",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/oauth_app/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/oauth_app/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/media_proxy_caches",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/media_proxy_caches/delete",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/media_proxy_caches/purge",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/frontends",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/frontends/install",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/backups",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/rules",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/rules",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/rules/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/rules/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/webhooks",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/webhooks",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/webhooks/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/webhooks/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/webhooks/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/webhooks/:id/enable",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/webhooks/:id/disable",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/webhooks/:id/rotate_secret",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/announcements",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/announcements",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/announcements/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/announcements/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/announcements/:id",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/reload_emoji",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/moderation_log",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/stats",
        auth: AuthClass::Admin,
    },
];
