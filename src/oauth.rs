//! OAuth 2.0: the bearer-token authorization behind the API
//! (`docs/ARCHITECTURE.md` decision 9).
//!
//! The database-free half of it lives here first: [`scopes`] parses, matches
//! and validates Mastodon-style scope strings, and [`mfa_page`] builds the
//! HTML of the `/oauth/mfa` code-entry page, on the shared scaffolding of
//! [`page`]. The consent screen is not server-rendered: it lives at
//! `{FRONTEND_BASE_URL}/oauth/authorize`, and the external-login
//! registration and consent screens at
//! `{FRONTEND_BASE_URL}/oauth/external/register` and `/oauth/external/consent`.
//! Application and token models, the `/oauth` endpoints and the `AuthUser`
//! extractors build on top of them.

pub mod mfa_page;
pub mod page;
pub mod scopes;
