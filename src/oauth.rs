//! OAuth 2.0: the bearer-token authorization behind the API
//! (`docs/ARCHITECTURE.md` decision 9).
//!
//! The database-free half of it lives here first: [`scopes`] parses, matches
//! and validates Mastodon-style scope strings, and [`authorize_page`] builds
//! the HTML of the `/oauth/authorize` consent and out-of-band flows.
//! Application and token models, the `/oauth` endpoints and the `AuthUser`
//! extractors build on top of them.

pub mod authorize_page;
pub mod scopes;
