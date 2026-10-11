//! Domain types and database access.
//!
//! Per `docs/ARCHITECTURE.md` decisions 2 and 3 each module owns one
//! table (or one closely-coupled pair, like the OAuth token and
//! authorization-code tables): a `FromRow` struct for the row plus the
//! runtime-checked queries that touch
//! it (`sqlx::query_as::<_, T>`; never the `query!` family). Nothing here
//! knows about HTTP, though every fallible function returns
//! [`AppError`](crate::error::AppError), the crate's single error type, so
//! handlers can use `?`.

pub mod follow;
pub mod job;
pub mod media_attachment;
pub mod oauth_app;
pub mod oauth_token;
pub mod user;
