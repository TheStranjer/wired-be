//! OAuth 2.0: the bearer-token authorization behind the API
//! (`docs/ARCHITECTURE.md` decision 9).
//!
//! The database-free half of it lives here first: [`scopes`] parses, matches
//! and validates Mastodon-style scope strings, and [`pkce`] validates and
//! verifies the `S256` PKCE challenges codes are bound to. No OAuth screen is
//! server-rendered: the consent screen lives at
//! `{FRONTEND_BASE_URL}/oauth/authorize`, the external-login registration and
//! consent screens at `{FRONTEND_BASE_URL}/oauth/external/register` and
//! `/oauth/external/consent`, and the frontend answers an `mfa_required`
//! grant in-app through `POST /oauth/mfa/challenge`.
//! Application and token models, the `/oauth` endpoints and the `AuthUser`
//! extractors build on top of them.

pub mod pkce;
pub mod scopes;
