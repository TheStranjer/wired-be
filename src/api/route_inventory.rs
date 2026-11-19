//! The inventory of this instance's HTTP surface.
//!
//! One row per route Pleroma 2.10 answers — its method, its path and the
//! credential class its handler demands (`docs/ARCHITECTURE.md` decision 1).
//!
//! [`ROUTES`] is the conformance list the router is measured against:
//! `tests/api/route_inventory.rs` walks it, sends one credential-less request
//! per row through the real application router, and refuses a row the router
//! does not match (only the router-level fallback answers with its
//! `x-wired-fallback: 1` marker) or a gated row that answers anything but 401
//! or 403 without credentials. A route that lands here is therefore fixed in
//! three ways at once: its path and verb, the API surface it belongs to, and
//! who has to present a token to reach it.
//!
//! Paths are written the way Pleroma's own `lib/pleroma/web/router.ex`
//! writes them, with `:name` path parameters, so a row can be read straight
//! against the reference: the row `/api/v1/pleroma/accounts/:id` is the
//! router's `/api/v1/pleroma/accounts/{id}`, and [`RouteSpec::path`] is where
//! the table spells it that way. The test turns a parameter into a concrete
//! segment before sending.
//!
//! The table holds one row per Pleroma route this backend replicates, in the
//! order of the router file's scopes. Routes Pleroma has and Wired does not —
//! the frontend uploader's `POST /api/v1/pleroma/uploader_callback/:upload_path`,
//! Phoenix's dev and test routes, `/phoenix/live_dashboard`, the SPA
//! fallbacks (`GET /:maybe_nickname_or_id` and `GET /*path`) and the Shout
//! channel socket (`/socket`) — are deliberately absent; the README's
//! "API coverage" section names them.
//!
//! One row belongs to no Pleroma router: `GET /api/openapi`, Wired's own
//! surface document. [`crate::api::openapi`] generates that document from
//! this table, so the route is inventoried beside the ones it describes and
//! measured by the same gate.

/// Who has to present a bearer token to reach a route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthClass {
    /// No credential is needed: the handler answers an anonymous request,
    /// whether that is the whole story (`GET /api/v1/instance`) or the
    /// request carries its own credential in its body
    /// (`POST /api/v1/apps`, `POST /oauth/token`).
    Public,
    /// A user token: the handler takes [`AuthUser`](crate::auth::AuthUser)
    /// (or the app-credential [`AppAuth`](crate::auth::AppAuth), which is a
    /// token too) and refuses a request that presents none.
    User,
    /// A staff token: the handler takes [`AdminUser`](crate::auth::admin::AdminUser)
    /// or [`StaffUser`](crate::auth::admin::StaffUser) and checks an
    /// `admin:read`/`admin:write` scope, so an anonymous request is refused.
    Admin,
}

/// One row of the inventory: a route, and who may call it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteSpec {
    /// The uppercase HTTP method the router registers, e.g. `GET`.
    pub method: &'static str,
    /// The full path from the site root, with `:name` parameters, e.g.
    /// `/api/v1/pleroma/accounts/:id`.
    pub path: &'static str,
    /// The credential class [`RouteSpec::path`] demands.
    pub auth: AuthClass,
}

/// Every route of the replicated Pleroma 2.10 HTTP surface.
///
/// `AuthClass::Public` rows are documented exceptions to the gate the
/// conformance test asserts: they must be answered by a handler, and only
/// they may answer an anonymous request with anything other than 401 or 403.
pub const ROUTES: &[RouteSpec] = &[
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
    // ── Lists, filters, tags, markers, media, notifications, push ────────
    RouteSpec {
        method: "GET",
        path: "/api/v1/lists",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/lists",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/lists/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/lists/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/lists/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/lists/:id/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/lists/:id/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/lists/:id/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/filters",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/filters",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/filters/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/tags/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/tags/:id/follow",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/tags/:id/unfollow",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/followed_tags",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/markers",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/markers",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/media",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/media/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/media/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v2/media",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/notifications/:id",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/notifications/:id/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/notifications/clear",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/notifications/destroy_multiple",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/notifications/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/pleroma/notifications/read",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications/unread_count",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications/:group_key",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/notifications/:group_key/accounts",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v2/notifications/:group_key/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/push/subscription",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/announcements",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/announcements/:id/dismiss",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/reports",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/search",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v2/search",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/streaming/health",
        auth: AuthClass::Public,
    },
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
        path: "/api/v1/pleroma/preferred_frontend/available",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/preferred_frontend",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v0/pleroma/reports",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v0/pleroma/reports/:id",
        auth: AuthClass::Admin,
    },
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
    // ── Admin API, Pleroma's own (`/api/v1/pleroma/admin`) ───────────────
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/invites",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/activate",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/deactivate",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/approve",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/:nickname/toggle_activation",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/admin/users/tag",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users/tag",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/follow",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/unfollow",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/suggest",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/unsuggest",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/credentials",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/:nickname/credentials",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/password_reset",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/force_password_reset",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/confirm_email",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/resend_confirmation_email",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/admin/users/disable_mfa",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/invite_token",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/revoke_invite",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/email_invite",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/statuses",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/chats",
        auth: AuthClass::Admin,
    },
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
    // ── ActivityPub, discovery and the other top-level routes ────────────
    RouteSpec {
        method: "GET",
        path: "/.well-known/webfinger",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/.well-known/host-meta",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/.well-known/nodeinfo",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/nodeinfo/:version",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/feed",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/inbox",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/users/:nickname/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/outbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/users/:nickname/outbox",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/followers",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/following",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/collections/featured",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/statuses/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/users/:nickname/statuses/:id/activity",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/objects/:uuid",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/objects/:uuid/replies",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/activities/:uuid",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/notice/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/notice/:id/embed_player",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/embed/:id",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/tags/:tag",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/mailer/unsubscribe/:token",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/frontend_switcher",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/frontend_switcher",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/api/ap/whoami",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "POST",
        path: "/api/ap/upload_media",
        auth: AuthClass::User,
    },
    RouteSpec {
        method: "GET",
        path: "/relay",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/relay/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/relay/following",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/relay/followers",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/internal/fetch",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/internal/fetch/inbox",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/main/ostatus",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/main/ostatus",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/ostatus_subscribe",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "POST",
        path: "/ostatus_subscribe",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/authorize_interaction",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/:sig/:url",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/:sig/:url/:filename",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/preview/:sig/:url",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/proxy/preview/:sig/:url/:filename",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/media/:key",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/emoji/:pack/:file",
        auth: AuthClass::Public,
    },
    RouteSpec {
        method: "GET",
        path: "/instance/:name.html",
        auth: AuthClass::Public,
    },
];
