//! The instance's domain reject lists, as one question every signed
//! `ActivityPub` request asks of its signer: is this host refused?
//!
//! Both gates that authenticate a peer by HTTP Signature — the inbox
//! extractor ([`crate::federation::inbox_auth`]) and the read gate
//! ([`crate::federation::authorized_fetch`]) — turn a refused host away
//! through `host_refused`, so a rejected instance can neither deliver to
//! this instance nor read from it.

use url::Url;

use crate::{error::AppResult, federation::mrf, models::instance_policy_domain, state::AppState};

/// Whether `host` is on a reject list: the instance-policy table, or the
/// domain lists of the [`mrf`] configuration (its `reject` list, or an
/// `accept` allowlist that does not name it).
///
/// `host` is the canonical spelling [`signer_host`] returns: lowercase,
/// without port.
///
/// # Errors
///
/// Database failures arrive as the usual
/// [`AppError::Database`](crate::error::AppError).
pub(crate) async fn host_refused(state: &AppState, host: &str) -> AppResult<bool> {
    let listed = instance_policy_domain::is_rejected(&state.db, host).await?;
    Ok(listed || mrf::load_config(state).await.rejected(host))
}

/// The host of a signer's key URL, lowercase and without the port.
///
/// It is the authority of its `keyId` (or, equivalently, of the actor URL
/// it points at) — the canonical spelling `instance_policy_domains` rows
/// keep. `None` when the URL cannot be parsed.
#[must_use]
pub fn signer_host(key_id: &str) -> Option<String> {
    let parsed = Url::parse(key_id).ok()?;
    let host = parsed.host_str()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signer_host_reads_the_lowercase_authority_host() {
        assert_eq!(
            signer_host("https://Peer.Example/users/alice#main-key").as_deref(),
            Some("peer.example"),
            "host only, portless, case-folded"
        );
        assert_eq!(
            signer_host("http://127.0.0.1:4000/users/alice#main-key").as_deref(),
            Some("127.0.0.1"),
            "the port is not part of the host"
        );
        assert_eq!(
            signer_host("not a url"),
            None,
            "an unparsable keyId names no host"
        );
    }
}
