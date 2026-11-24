//! The inventory of this instance's HTTP surface.
//!
//! One row per route Pleroma 2.10 answers — its method, its path and the
//! credential class its handler demands (`docs/ARCHITECTURE.md` decision 1).
//!
//! [`routes()`] — every row of [`ROUTES`], section by section in table
//! order — is the conformance list the router is measured against:
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
//! the OAuth MFA code-entry page `GET /oauth/mfa` and its
//! `POST /oauth/mfa/verify` form, Phoenix's dev and test routes,
//! `/phoenix/live_dashboard`, the SPA
//! fallbacks (`GET /:maybe_nickname_or_id` and `GET /*path`) and the Shout
//! channel socket (`/socket`) — are deliberately absent; the README's
//! "API coverage" section names them.
//!
//! The table is stored in sections: one child module per surface, each
//! holding a `pub(super)` `ROWS` slice of its rows. [`ROUTES`] gathers the
//! sections in table order and [`routes()`] walks their rows; the unit test
//! below pins the section count, rejects duplicate rows and pins the
//! assembled table's ends, so dropping or moving a section fails the build.

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

mod account_settings;
mod accounts;
mod activitypub;
mod auth;
mod emoji_packs;
mod instance;
mod lists_media_notifications;
mod mastodon_admin;
mod pleroma_admin_other;
mod pleroma_admin_users;
mod pleroma_misc;
mod statuses;

/// The inventory's sections, in table order: one entry per child module,
/// each entry that module's rows.
///
/// [`routes()`] walks the rows. `AuthClass::Public` rows are documented
/// exceptions to the gate the conformance test asserts: they must be answered
/// by a handler, and only they may answer an anonymous request with anything
/// other than 401 or 403.
pub const ROUTES: &[&[RouteSpec]] = &[
    instance::ROWS,
    auth::ROWS,
    accounts::ROWS,
    statuses::ROWS,
    lists_media_notifications::ROWS,
    pleroma_misc::ROWS,
    account_settings::ROWS,
    emoji_packs::ROWS,
    pleroma_admin_users::ROWS,
    pleroma_admin_other::ROWS,
    mastodon_admin::ROWS,
    activitypub::ROWS,
];

/// Every route of the replicated Pleroma 2.10 HTTP surface, in table order:
/// the rows of the [`ROUTES`] sections, one section at a time.
pub fn routes() -> impl Iterator<Item = &'static RouteSpec> {
    ROUTES.iter().copied().flatten()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{AuthClass, ROUTES, RouteSpec, routes};

    /// The assembled table is well formed: twelve sections, none of them
    /// empty, no `(method, path)` pair listed twice, opening with the
    /// instance metadata route and closing with the custom emoji files.
    #[test]
    fn routes_assembles_the_whole_table_in_order() {
        assert_eq!(ROUTES.len(), 12, "one section per surface");
        assert!(
            ROUTES.iter().all(|section| !section.is_empty()),
            "every section contributes rows"
        );
        let rows: Vec<&RouteSpec> = routes().collect();
        let distinct: HashSet<(&str, &str)> = rows.iter().map(|r| (r.method, r.path)).collect();
        assert_eq!(
            distinct.len(),
            rows.len(),
            "no (method, path) pair appears twice"
        );
        assert_eq!(
            rows.first(),
            Some(&&RouteSpec {
                method: "GET",
                path: "/api/v1/instance",
                auth: AuthClass::Public,
            }),
            "the table opens with the instance metadata route"
        );
        assert_eq!(
            rows.last(),
            Some(&&RouteSpec {
                method: "GET",
                path: "/emoji/:pack/:file",
                auth: AuthClass::Public,
            }),
            "the table closes with the custom emoji files"
        );
    }
}
