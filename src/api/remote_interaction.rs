//! Remote interaction: the endpoints a remote instance uses to follow a
//! local account or interact with a local object from its own frontend
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `RemoteInteractionController`
//! (`lib/pleroma/web/remote_interaction/remote_interaction_controller.ex`)
//! and the routes `lib/pleroma/web/router.ex` serves it on; the ticket
//! rules here are authoritative where the two differ. The two surfaces
//! (Pleroma's `GET/POST /ostatus_subscribe` password-login follow and its
//! server-rendered `GET/POST /main/ostatus` remote-subscribe form are not
//! served: the first took a password with neither a rate limit nor the
//! second factor, and the second is an HTML form the frontend replaces, so
//! remote follow lands on the frontend's `/authorize_interaction` page and
//! follows with the user's OAuth token):
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
//!
//! The subscribe lookup reads the full `WebFinger` answer through the
//! client's whole-document reader (see
//! [`crate::federation::webfinger_client`]), the same URL, host-meta
//! fallback and format sniffing the actor resolver uses. The mock-server
//! tests in the children drive it against loopback, through that
//! client's scheme-and-authority override.
//!
//! This module is a tree. `interaction` answers the JSON endpoint and
//! `GET /authorize_interaction`: the subscribe lookup, its template
//! reading and macro expansion, and the frontend resolution; `subscribe`
//! builds the frontend redirect; `pages` holds the parameter trimming the
//! routes read their requests through; and `test_support` holds the
//! mock-`WebFinger` fixtures the children's unit tests share.

use axum::{
    Router,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

use self::interaction::{authorize_interaction, remote_interaction};

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
/// The path of `GET /authorize_interaction`.
const AUTHORIZE_PATH: &str = "/authorize_interaction";

/// The top-level remote-interaction router, merged at the root by
/// [`crate::app`].
///
/// Mounts `/authorize_interaction`. Do not call [`Router::fallback`] on
/// it: unmatched paths must reach the app-wide fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route(AUTHORIZE_PATH, get(authorize_interaction))
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

#[cfg(test)]
mod tests {
    use axum::Router;

    use super::*;

    /// The routers mount every documented path.
    #[test]
    fn the_two_routers_carry_the_two_routes() {
        // Building them proves the paths are axum-legal; the handlers are
        // exercised over HTTP by the integration suite.
        let _pages: Router<AppState> = router();
        let _api: Router<AppState> = pleroma_router();
    }
}
