//! Remote interaction: the endpoints a remote instance uses to follow a
//! local account or interact with a local object from its own frontend
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `RemoteInteractionController`
//! (`lib/pleroma/web/remote_interaction/remote_interaction_controller.ex`)
//! and the routes `lib/pleroma/web/router.ex` serves it on; the ticket
//! rules here are authoritative where the two differ. The three surfaces
//! (Pleroma's `GET/POST /ostatus_subscribe` password-login follow is not
//! served: it took a password with neither a rate limit nor the second
//! factor, so remote follow lands on the frontend's `/authorize_interaction`
//! page and follows with the user's OAuth token):
//!
//! - `POST /api/v1/pleroma/remote_interaction` (no auth, form or JSON):
//!   given `ap_id` (a local actor or object URL on *this* instance) and
//!   `profile` (the visitor's remote `user@domain` handle), `WebFinger`s
//!   the profile, reads the link with
//!   `rel="http://ostatus.org/schema/1.0/subscribe"` and answers its
//!   template with `{uri}` replaced by the percent-encoded `ap_id`. A
//!   profile that resolves to no subscribe template is the 400
//!   `{"error":"Couldn't find user"}`; an absent or blank parameter is
//!   the 400 `{"error":"Missing parameter: ap_id"}` (or `profile`).
//!   `ap_id` itself is not validated: it is the value the remote instance
//!   is asked to carry back, and Pleroma substitutes whatever it is
//!   given.
//! - `GET /authorize_interaction?uri=<url or handle>` sends the browser
//!   to the frontend page for whatever the `uri` resolves to: an actor to
//!   `{FRONTEND_BASE_URL}/users/{id}`, a status to
//!   `{FRONTEND_BASE_URL}/notice/{id}`, nothing resolvable to the 404
//!   JSON.
//! - `GET /main/ostatus` renders the remote-subscribe form (the local
//!   account under `user[nickname]`, the remote handle under
//!   `user[profile]`), and `POST /main/ostatus` computes exactly the URL
//!   the JSON endpoint answers and redirects to it (302), re-rendering
//!   the form with an error line whenever that fails.
//!
//! Every page this module renders is a full HTML document with all
//! request- and database-sourced values escaped, under a
//! `content-security-policy` that allows the one interaction the pages
//! exist for: a form submit to this same origin (see the `PAGE_CSP`
//! constant).
//!
//! The subscribe lookup reads the full `WebFinger` answer through the
//! client's whole-document reader (see
//! [`crate::federation::webfinger_client`]), the same URL, host-meta
//! fallback and format sniffing the actor resolver uses. The mock-server
//! tests in the children drive it against loopback, through that
//! client's scheme-and-authority override.
//!
//! This module is a tree. `interaction` answers the JSON endpoint and
//! `GET /authorize_interaction`: the shared subscribe lookup, its
//! template reading and macro expansion, and the frontend redirect;
//! `subscribe` runs the `GET/POST /main/ostatus` remote-subscribe pair,
//! with the redirects they answer with; `pages` renders every page this
//! feature shows — the body, the document, the headers — and holds the
//! parameter trimming the routes read their requests through; and
//! `test_support` holds the mock-`WebFinger` fixtures the children's unit
//! tests share.

use axum::{
    Router,
    http::HeaderName,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

use self::{
    interaction::{authorize_interaction, remote_interaction},
    subscribe::{do_remote_subscribe, show_remote_subscribe},
};

mod interaction;
mod pages;
mod subscribe;

#[cfg(test)]
mod test_support;

/// The `rel` naming an instance's remote-follow (subscribe) endpoint.
const SUBSCRIBE_REL: &str = "http://ostatus.org/schema/1.0/subscribe";
/// The 400 body message of the JSON endpoint, Pleroma's wording.
const COULD_NOT_FIND_USER: &str = "Couldn't find user";
/// The 400 body of an absent or blank `ap_id` (the shape of the shared
/// `Missing parameter: <name>` refusals, see [`crate::api::accounts`]).
const MISSING_AP_ID: &str = "Missing parameter: ap_id";
/// The 400 body of an absent or blank `profile`.
const MISSING_PROFILE: &str = "Missing parameter: profile";
/// The page error line for a handle this instance cannot resolve,
/// Pleroma's `follow`-page wording.
const COULD_NOT_FIND_USER_PAGE: &str = "Could not find user";
/// The page error line for any other failure, Pleroma's catch-all.
const SOMETHING_WENT_WRONG: &str = "Something went wrong.";
/// The title of the `main/ostatus` remote-subscribe page.
const REMOTE_SUBSCRIBE_TITLE: &str = "Remote subscribe";
/// The path of `GET /authorize_interaction`.
const AUTHORIZE_PATH: &str = "/authorize_interaction";
/// The path of the remote-subscribe form pair.
const REMOTE_SUBSCRIBE_PATH: &str = "/main/ostatus";
/// The media type of every page this module renders.
const PAGE_CONTENT_TYPE: &str = "text/html; charset=utf-8";
/// The `Content-Security-Policy` of every page: no scripts, inline CSS
/// only and a form submit to this same origin. These pages exist to
/// submit a form, and a policy without the `form-action 'self'` arm would
/// have the browser refuse it.
const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'";
/// The one inline stylesheet [`page_document`] embeds (the CSP allows no
/// other).
///
/// [`page_document`]: crate::api::remote_interaction::pages::page_document
const PAGE_STYLE: &str = "body{margin:0;min-height:100vh;display:flex;\
    align-items:center;justify-content:center;background:#f5f4ef;\
    font-family:system-ui,sans-serif;color:#28242d}\
    main{background:#fff;border-radius:8px;padding:2rem;max-width:30rem;\
    box-shadow:0 2px 10px rgba(40,36,45,0.12)}\
    h1{font-size:1.3rem;margin-top:0}\
    label{display:block}\
    input{margin:0.25rem 0 0.75rem 0;padding:0.4rem 0.6rem;width:100%}\
    input[type=submit]{padding:0.5rem 1rem;border-radius:4px;\
    border:1px solid #7c8a52;background:#7c8a52;color:#fff;cursor:pointer;width:auto}\
    p.error{color:#a1282c}";
/// The `X-Frame-Options` header every page puts up: these pages carry
/// login forms, and no other site may frame them.
static FRAME_OPTIONS_HEADER: HeaderName = HeaderName::from_static("x-frame-options");

/// The top-level remote-interaction pages router, merged at the root by
/// [`crate::app`].
///
/// Mounts `/authorize_interaction` and `/main/ostatus`. Do not call
/// [`Router::fallback`] on it: unmatched paths must reach the app-wide
/// fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route(AUTHORIZE_PATH, get(authorize_interaction))
        .route(
            REMOTE_SUBSCRIBE_PATH,
            get(show_remote_subscribe).post(do_remote_subscribe),
        )
}

/// The `/api/v1/pleroma` fragment of this feature, merged by
/// [`crate::api::v1`]: `POST /remote_interaction`.
pub fn pleroma_router() -> Router<AppState> {
    Router::new().route("/pleroma/remote_interaction", post(remote_interaction))
}

/// The parameters of `POST /api/v1/pleroma/remote_interaction`: both are
/// optional at the type level so a request missing one deserializes and
/// is refused with the endpoint's own `Missing parameter` 400 rather than
/// the extractor's generic 422.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InteractionParams {
    /// The local actor or object URL the interaction is about.
    ap_id: Option<String>,
    /// The visitor's remote `user@domain` handle.
    profile: Option<String>,
}

/// The 200 body of `POST /api/v1/pleroma/remote_interaction`: the
/// interaction URL the client should send the visitor's browser to.
#[derive(Debug, Serialize)]
struct InteractionReply {
    /// The remote instance's subscribe template with the `ap_id`
    /// substituted for its `{uri}` macro.
    url: String,
}

/// The query of `GET /authorize_interaction`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InteractionQuery {
    /// The actor URL, status URL or `user@domain` handle to interact with.
    uri: Option<String>,
}

/// The query of `GET /main/ostatus`: Pleroma reads the flat
/// `nickname`/`profile` pair when it renders the form.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RemoteSubscribeQuery {
    /// The local account to prefill `user[nickname]` with.
    nickname: Option<String>,
    /// The remote handle to prefill `user[profile]` with.
    profile: Option<String>,
}

/// The body of `POST /main/ostatus`: the fields ride under `user`, the
/// shape of the form [`remote_subscribe_body`] renders (`user[nickname]`,
/// `user[profile]`).
///
/// [`remote_subscribe_body`]: crate::api::remote_interaction::pages::remote_subscribe_body
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RemoteSubscribeBody {
    /// The submitted pair.
    user: Option<SubscribeFields>,
}

/// The nested `user` map of [`RemoteSubscribeBody`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SubscribeFields {
    /// The local account.
    nickname: Option<String>,
    /// The remote handle.
    profile: Option<String>,
}

#[cfg(test)]
mod tests {
    use axum::Router;

    use super::*;

    /// The routers mount every documented path.
    #[test]
    fn the_two_routers_carry_the_three_routes() {
        // Building them proves the paths are axum-legal; the handlers are
        // exercised over HTTP by the integration suite.
        let _pages: Router<AppState> = router();
        let _api: Router<AppState> = pleroma_router();
    }
}
