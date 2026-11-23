//! Response header values several routes share.

use axum::http::HeaderValue;

/// The `x-content-type-options` value every media-serving route sends: the
/// type served is the type to use, and a browser must not sniff another
/// one (as in Pleroma's `HttpSecurityPlug`).
///
/// A `static`, not a `const`: [`HeaderValue`] holds a `Bytes` with interior
/// mutability, which Clippy rejects in a `const`. Clone it per response.
pub static NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nosniff_is_the_header_keyword() {
        assert_eq!(NOSNIFF, "nosniff", "the only value the header defines");
    }
}
