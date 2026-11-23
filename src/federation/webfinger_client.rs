//! Client-side `WebFinger`: resolving an `acct:nick@domain` handle to the
//! `ActivityPub` actor URL of a remote account.
//!
//! [`resolve_acct`] is the lookup this instance starts every remote-follow
//! and remote-fetch with. The reference behaviour is Pleroma's
//! `Pleroma.Web.WebFinger.finger/1`
//! (`lib/pleroma/web/web_finger.ex`) and its controller
//! (`lib/pleroma/web/web_finger/web_finger_controller.ex`); the ticket rules
//! here are authoritative where the two differ.
//!
//! The protocol shape, per RFC 7033:
//!
//! 1. `GET https://{domain}/.well-known/webfinger?resource=acct:{nick}@{domain}`
//!    asking for `application/jrd+json`, with the body capped at 256 KiB.
//! 2. When that answers 404 or 410, or the network fails, `GET`
//!    `https://{domain}/.well-known/host-meta` (XRD), take the
//!    `<Link rel="lrdd" template="...">` template, expand its `{uri}` macro
//!    with the `acct:` resource and fetch that URL instead. A successful
//!    first attempt never consults host-meta.
//! 3. The actor URL is the `href` of the first `rel=self` link whose `type`
//!    is `application/activity+json` or `application/ld+json` (parameters
//!    and whitespace in the declared type are ignored). A
//!    `rel=http://webfinger.net/rel/profile-page` link only confirms that the
//!    host knows the account; it points at an HTML page, never at an actor
//!    document, so with no usable self link the lookup fails as
//!    [`WebfingerError::NoActorLink`] rather than falling back to it.
//! 4. Both representations are read: JRD JSON, and XRD XML through the
//!    `quick-xml` event reader. The format is decided by the response's
//!    content type, falling back to the document's first character when the
//!    type is absent or unknown.
//!
//! Two reads ride on that lookup: [`resolve_acct`] extracts the actor URL,
//! and `finger_with_base` answers the whole document as a JRD
//! `serde_json::Value` (an XRD answer is normalized into the same shape).
//! The latter is how the remote-interaction endpoints find the
//! `http://ostatus.org/schema/1.0/subscribe` template (see
//! [`crate::api::remote_interaction`]).
//!
//! Every resolved document is also checked for domain containment (see
//! [`resolve_acct`]): a host must not be able to point this instance at an
//! actor on someone else's host.
//!
//! The module is a tree: the lookup and its host-meta fallback live in
//! `resolve` and `lrdd`, the handle rules in `acct`, the document shapes in
//! `document` and the JRD/XRD readers in `parse`. This root keeps the shared
//! constants and the error type, and re-exports the outside-facing surface
//! so every import path into it stays as it was.

use std::{error::Error, fmt};

use quick_xml::XmlVersion;
use tracing::debug;

use crate::http_client::HttpError;

pub mod acct;
mod document;
mod lrdd;
mod parse;
pub mod resolve;
#[cfg(test)]
mod test_support;

pub use self::{acct::split_acct, resolve::resolve_acct};

pub(crate) use self::{
    acct::strip_finger_prefixes,
    resolve::{Resolved, finger_with_base, resolve_acct_with_base},
};

/// Response body cap for every `WebFinger` exchange (256 KiB): JRD and XRD
/// documents are small, and a hostile host must not be able to stream an
/// endless one into this process.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Media type asked for on every `WebFinger` GET (RFC 7033's JRD). Crate
/// visibility because the remote-interaction tests label their mock
/// answers with it (see [`crate::api::remote_interaction`]).
pub(crate) const JRD_JSON: &str = "application/jrd+json";
/// Media type sent as `Accept` on the host-meta GET (RFC 7033's XRD).
/// Crate-visible for the same reason.
pub(crate) const XRD_XML: &str = "application/xrd+xml";
/// The `rel` naming the actor document.
const SELF_REL: &str = "self";
/// The `rel` of the LRDD template link inside host-meta.
const LRDD_REL: &str = "lrdd";
/// First accepted media type of the self link: `ActivityPub` JSON.
const ACTIVITY_JSON: &str = "application/activity+json";
/// Second accepted media type of the self link. Its `profile` parameter is
/// never compared — the ticket fixes "ignore parameters" — so any
/// `application/ld+json` (with or without parameters) names the actor.
const LD_JSON: &str = "application/ld+json";
/// The resource macro of an LRDD template (RFC 7033 Section 5). Crate
/// visibility because the remote-interaction endpoints expand the same
/// macro in a subscribe template (see
/// [`crate::api::remote_interaction`]).
pub(crate) const URI_MACRO: &str = "{uri}";
/// The percent-encoded spelling of [`URI_MACRO`], as Mastodon emits it.
pub(crate) const URI_MACRO_ENCODED: &str = "%7Buri%7D";
/// The `acct:` URI scheme prefix `WebFinger` resources carry.
const ACCT_PREFIX: &str = "acct:";
/// XRD is an XML 1.0 vocabulary; attribute values are normalized and
/// unescaped under those rules.
const XRD_XML_VERSION: XmlVersion = XmlVersion::Explicit1_0;

/// How an `acct` to actor URL resolution can fail.
#[derive(Debug)]
pub enum WebfingerError {
    /// The handle is not a `nick@domain` this client may look up: empty or
    /// at-sign-less, or with characters outside the nickname and hostname
    /// alphabets (see [`split_acct`]).
    InvalidAcct,
    /// A request failed or answered 4xx/5xx: the transport and status
    /// failures of [`crate::http_client`].
    Http(HttpError),
    /// The document carried no `rel=self` link with an `ActivityPub` media
    /// type: the account has no actor URL to return. A profile-page link
    /// does not count.
    NoActorLink,
    /// The document does not belong to the requested domain: neither its
    /// subject nor its actor URL names that domain (see [`resolve_acct`]).
    DomainMismatch,
    /// The body is neither a parseable JRD document nor a parseable XRD
    /// one (or it is not valid UTF-8).
    Malformed,
}

impl fmt::Display for WebfingerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAcct => f.write_str("not a valid acct:nick@domain handle"),
            Self::Http(err) => write!(f, "webfinger request failed: {err}"),
            Self::NoActorLink => f.write_str("webfinger document has no ActivityPub self link"),
            Self::DomainMismatch => {
                f.write_str("webfinger result does not belong to the requested domain")
            }
            Self::Malformed => f.write_str("webfinger document is malformed"),
        }
    }
}

impl Error for WebfingerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::InvalidAcct | Self::NoActorLink | Self::DomainMismatch | Self::Malformed => None,
        }
    }
}

/// Any HTTP failure is a [`WebfingerError::Http`], so `?` works throughout
/// the lookup.
impl From<HttpError> for WebfingerError {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

/// Turns any parse-level cause into [`WebfingerError::Malformed`], after
/// logging what the document actually failed on.
pub(super) fn note_malformed<Cause: fmt::Display>(cause: Cause) -> WebfingerError {
    debug!(%cause, "could not parse a webfinger document");
    WebfingerError::Malformed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webfinger_errors_render_their_causes() {
        assert_eq!(
            WebfingerError::InvalidAcct.to_string(),
            "not a valid acct:nick@domain handle",
            "InvalidAcct"
        );
        assert_eq!(
            WebfingerError::Http(HttpError::Status(404)).to_string(),
            "webfinger request failed: HTTP peer answered 404",
            "Http keeps the transport message"
        );
        assert_eq!(
            WebfingerError::NoActorLink.to_string(),
            "webfinger document has no ActivityPub self link",
            "NoActorLink"
        );
        assert_eq!(
            WebfingerError::DomainMismatch.to_string(),
            "webfinger result does not belong to the requested domain",
            "DomainMismatch"
        );
        assert_eq!(
            WebfingerError::Malformed.to_string(),
            "webfinger document is malformed",
            "Malformed"
        );
        assert!(
            Error::source(&WebfingerError::Http(HttpError::Timeout)).is_some(),
            "Http keeps the transport error as its source"
        );
        assert!(
            Error::source(&WebfingerError::Malformed).is_none(),
            "a plain Malformed has no source"
        );
    }
}
