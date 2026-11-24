//! Remote interaction: the endpoint a remote-follow client uses to learn
//! where a visitor's own instance wants an interaction carried
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `RemoteInteractionController`
//! (`lib/pleroma/web/remote_interaction/remote_interaction_controller.ex`);
//! the ticket rules here are authoritative where the two differ. Pleroma's
//! browser surfaces are not served: `GET/POST /ostatus_subscribe` took a
//! password with neither a rate limit nor the second factor, the
//! server-rendered `GET/POST /main/ostatus` form is HTML the frontend
//! replaces, and `GET /authorize_interaction` let an anonymous caller make
//! this server fetch and store any remote actor or status. The frontend
//! serves `/authorize_interaction` (the `WebFinger` subscribe template
//! points at it) and follows with the user's OAuth token.
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
//!
//! The subscribe lookup reads the full `WebFinger` answer through the
//! client's whole-document reader (see
//! [`crate::federation::webfinger_client`]), the same URL, host-meta
//! fallback and format sniffing the actor resolver uses. The mock-server
//! tests in the child drive it against loopback, through that client's
//! scheme-and-authority override.
//!
//! This module is a tree. `interaction` answers the JSON endpoint: the
//! subscribe lookup, its template reading and macro expansion, and the
//! parameter trimming; `test_support` holds the mock-`WebFinger` fixtures
//! its unit tests share.

use axum::{Router, routing::post};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

use self::interaction::remote_interaction;

mod interaction;

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

#[cfg(test)]
mod tests {
    use axum::Router;

    use super::*;

    /// The router mounts the documented path.
    #[test]
    fn the_router_carries_the_route() {
        // Building it proves the path is axum-legal; the handler is
        // exercised over HTTP by the integration suite.
        let _api: Router<AppState> = pleroma_router();
    }
}
