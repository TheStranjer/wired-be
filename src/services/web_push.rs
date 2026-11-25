//! Web Push sender: the RFC 8291 `aes128gcm` payload encryption, the VAPID
//! (RFC 8292) request signing and the one POST that delivers a payload to a
//! browser's push service (WBE-149).
//!
//! The reference is Pleroma's `Pleroma.Web.Push.Impl`
//! (`lib/pleroma/web/push/impl.ex`), which delegates the wire work to the
//! `web_push_encryption` Hex package; this module *is* that package's
//! sender, split the way the two RFCs are:
//!
//! - [`encrypt_payload`] implements the RFC 8291 content coding: a random
//!   16-octet salt, a fresh ephemeral P-256 key pair, ECDH against the
//!   subscription's `p256dh` key, the two HKDF-SHA-256 steps the RFC spells
//!   out (the `WebPush: info\0` + client key + server key combine with the
//!   auth secret, then the `Content-Encoding:` derives), one record of
//!   `plaintext || 0x02` sealed with AES-128-GCM, and the
//!   `salt || rs || idlen || server public key` header in front.
//! - [`vapid_headers`] implements the RFC 8292 authorization: an ES256 JWT
//!   whose `aud` is the endpoint's origin, `exp` twelve hours out and `sub`
//!   the instance's [`vapid_subject`], sent as `Authorization: vapid t=…, k=…`
//!   beside the instance public key ([`crate::services::vapid`]).
//! - [`send`] puts the two together and POSTs one subscription's payload
//!   with the headers a push service expects, classifying the answer into
//!   [`PushError::Gone`] (the subscription is dead; the caller deletes it)
//!   or [`PushError::Retryable`] (5xx, 429, transport: the queue tries
//!   again).
//!
//! The delivery decisions that surround the send — which alert wakes a
//! subscription ([`alert_enabled`], whose `policy` mate the
//! [`push.deliver` job](crate::jobs::web_push) resolves against
//! [`crate::models::follow`]), and what the JSON payload says
//! ([`payload_json`]) — are pure here, so the job is thin glue. Like
//! Pleroma the payload is the Mastodon-shaped JSON document
//! (`notification_id`, `title`, `body`, …); the ticket
//! fixes its content: the title is `<actor display name> <verb>` and the
//! body the status's plain text, empty when the recipient set
//! `hide_notification_contents`. Unlike Pleroma it carries no
//! `access_token`: the token table keeps only digests, so the raw bearer
//! credential is not recoverable once issued.
//!
//! The sender is a tree: `crypto` holds the content coding and its header
//! constants, `decrypt` the receiver's half that exists for tests and
//! verification, `vapid` the authorization and the `TTL`/`Urgency` header
//! values, `payload` the delivery decisions and the document, `send` the
//! classified POST and the job queueing, and `test_support` the fixtures
//! the children's unit tests share. Everything outside this module touches
//! is re-exported here, so `crate::services::web_push::*` stays their
//! path.

use std::{error::Error, fmt};

mod crypto;
#[cfg(any(test, feature = "integration-tests"))]
mod decrypt;
mod payload;
mod send;
mod vapid;

#[cfg(test)]
mod test_support;

pub use crypto::encrypt_payload;
#[cfg(any(test, feature = "integration-tests"))]
pub use decrypt::decrypt_payload;
pub use payload::{PushPayload, alert_enabled, payload_json, verb_for};
pub use send::{enqueue_deliveries, send};
pub use vapid::{TTL_HEADER, URGENCY_HEADER, vapid_headers, vapid_subject};

/// Why a push delivery did not happen, as [`send`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The subscription is dead: the push service answered 404 or 410,
    /// the stored keys can encrypt nothing, or the SSRF guard blocks the
    /// endpoint (every retry would be blocked too). The caller deletes the
    /// subscription — sending to it will never work again.
    Gone,
    /// The delivery may succeed later: a 5xx, a 429, a transport failure,
    /// or any other refusal the ticket leaves on the retryable side. The
    /// string states what happened, for the job's `last_error`.
    Retryable(String),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("the push subscription is gone"),
            Self::Retryable(detail) => write!(f, "retryable push failure: {detail}"),
        }
    }
}

impl Error for PushError {}
