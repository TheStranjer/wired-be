//! Redirect containment shared by the actor and object fetchers.
//!
//! What a fetch may accept depends on the URL that actually served the body,
//! not only on the URL that was asked for (Mastodon's `follow: false` round
//! trip, Pleroma's `check_crossdomain_redirect`).

use url::Url;

use super::FetchError;

/// Checks that `served` — the final URL after redirects — is on the host
/// (and explicit port) of `requested`. A scheme upgrade on the same host
/// passes.
///
/// # Errors
///
/// [`FetchError::Invalid`] when `requested` is not a URL or `served` is on
/// another host or port.
pub fn ensure_same_host(requested: &str, served: &Url) -> Result<(), FetchError> {
    let asked = Url::parse(requested)
        .map_err(|err| FetchError::Invalid(format!("`{requested}` is not a URL: {err}")))?;
    if asked.host_str() == served.host_str() && asked.port() == served.port() {
        return Ok(());
    }
    Err(FetchError::Invalid(format!(
        "cross-domain redirect from `{requested}` to `{served}`"
    )))
}

/// Whether a document claiming `claimed` as its `id` names the URL that
/// served it: the final URL itself, or the requested URL spelled as asked
/// when no redirect happened.
#[must_use]
pub fn names_served_url(requested: &str, served: &Url, claimed: &str) -> bool {
    claimed == served.as_str() || (was_not_redirected(requested, served) && claimed == requested)
}

/// Checks that `requested` was answered from its own URL: the refetch of a
/// claimed id may not redirect.
///
/// # Errors
///
/// [`FetchError::Invalid`] when `served` is a different URL.
pub fn ensure_not_redirected(requested: &str, served: &Url) -> Result<(), FetchError> {
    if was_not_redirected(requested, served) {
        return Ok(());
    }
    Err(FetchError::Invalid(format!(
        "the claimed id `{requested}` redirects to `{served}`"
    )))
}

/// Whether `served` is exactly the URL `requested` parses to.
fn was_not_redirected(requested: &str, served: &Url) -> bool {
    Url::parse(requested).is_ok_and(|asked| asked == *served)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a literal URL.
    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn the_host_and_explicit_port_must_match() {
        let base = "https://a.example/x";
        ensure_same_host(base, &url("https://a.example/y")).unwrap();
        ensure_same_host(base, &url("http://a.example/y")).unwrap();
        assert!(ensure_same_host(base, &url("https://b.example/x")).is_err());
        assert!(ensure_same_host(base, &url("https://a.example:8443/x")).is_err());
        assert!(ensure_same_host("not a url", &url("https://a.example/x")).is_err());
    }

    #[test]
    fn a_document_names_the_served_url_or_the_unredirected_request() {
        let served = url("https://a.example/users/alice");
        assert!(names_served_url(
            "https://a.example/actor",
            &served,
            "https://a.example/users/alice"
        ));
        assert!(!names_served_url(
            "https://a.example/actor",
            &served,
            "https://a.example/actor"
        ));
        assert!(names_served_url(
            "https://a.example/users/alice",
            &served,
            "https://a.example/users/alice"
        ));
        assert!(!names_served_url(
            "https://a.example/users/alice",
            &served,
            "https://a.example/other"
        ));
    }

    #[test]
    fn a_refetch_must_be_served_from_the_url_asked_for() {
        let asked = "https://a.example/users/alice";
        ensure_not_redirected(asked, &url(asked)).unwrap();
        assert!(ensure_not_redirected(asked, &url("https://a.example/elsewhere")).is_err());
    }
}
