//! OAuth 2.0: the bearer-token authorization behind the API
//! (`docs/ARCHITECTURE.md` decision 9).
//!
//! The database-free half of it lives here first: [`scopes`] parses, matches
//! and validates Mastodon-style scope strings, and [`authorize_page`] and
//! [`mfa_page`] build the HTML of the `/oauth/authorize` consent,
//! out-of-band and `/oauth/mfa` code-entry flows.
//! Application and token models, the `/oauth` endpoints and the `AuthUser`
//! extractors build on top of them.

pub mod authorize_page;
pub mod mfa_page;
pub mod scopes;
