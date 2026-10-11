//! OAuth 2.0: the bearer-token authorization behind the API
//! (`docs/ARCHITECTURE.md` decision 9).
//!
//! The database-free half of it lives here first: [`scopes`] parses, matches
//! and validates Mastodon-style scope strings. Application and token models,
//! the `/oauth` endpoints and the user-requiring extractors build on top of
//! it in later tickets.

pub mod scopes;
