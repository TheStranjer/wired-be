//! What a captured [`Delivery`] can tell a test about itself.

use super::*;

impl Delivery {
    /// The path this delivery arrived at — the shared inbox or one actor's
    /// personal inbox, which is how a test tells the two endpoints apart.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The delivered activity.
    pub fn activity(&self) -> &Value {
        &self.activity
    }

    /// The value of request header `name`, or `None` when it was not sent.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_key, value)| value.as_str())
    }

    /// The `type` of the delivered activity, when it carries one.
    pub fn activity_type(&self) -> Option<&str> {
        self.activity.get("type").and_then(Value::as_str)
    }

    /// The `keyId` the `Signature` header names, when that header parses.
    pub fn key_id(&self) -> Option<String> {
        let value = self.header("signature")?;
        parse_signature_header(value)
            .ok()
            .map(|parsed| parsed.key_id)
    }

    /// The exact bytes the `POST` arrived with, before parsing. A retry
    /// test compares two deliveries with these to prove the same signed
    /// body went out again.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Whether the `Digest` header describes the bytes that arrived.
    pub fn digest_matches(&self) -> bool {
        verify_digest(&self.header_map(), &self.body).is_ok()
    }

    /// Whether the `Signature` header authenticates this `POST` against
    /// `public_pem`.
    ///
    /// The signature covers the `(request-target)`, `host`, `date` and
    /// `digest` lines as they arrived, so a true answer means the holder of
    /// that key sent these bytes to this path.
    pub fn signature_verifies(&self, public_pem: &str) -> bool {
        let Some(value) = self.header("signature") else {
            return false;
        };
        let Ok(parsed) = parse_signature_header(value) else {
            return false;
        };
        let request = SignedRequestParts {
            method: "POST",
            path_and_query: &self.path,
            headers: self.header_map(),
        };
        verify(&parsed, &request, public_pem, Utc::now()).is_ok()
    }

    /// The captured headers as the [`HeaderMap`] the verifiers take.
    fn header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value))
            else {
                continue;
            };
            let _previous: Option<HeaderValue> = headers.insert(name, value);
        }
        headers
    }
}
