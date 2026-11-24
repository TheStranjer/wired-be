//! OAuth 2.0: the bearer-token authorization behind the API
//! (`docs/ARCHITECTURE.md` decision 9).
//!
//! The database-free half of it lives here first: [`scopes`] parses, matches
//! and validates Mastodon-style scope strings, and [`mfa_page`] and
//! [`registration_page`] build the HTML of the `/oauth/mfa` code-entry
//! page and the external-login registration form, on the shared
//! scaffolding of [`page`]. The consent screen is not server-rendered: it
//! lives at `{FRONTEND_BASE_URL}/oauth/authorize`.
//! Application and token models, the `/oauth` endpoints and the `AuthUser`
//! extractors build on top of them.

pub mod mfa_page;
pub mod page;
pub mod registration_page;
pub mod scopes;
