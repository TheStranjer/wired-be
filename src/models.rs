//! Domain types and database access.
//!
//! Per `docs/ARCHITECTURE.md` decisions 2 and 3 each module owns one table
//! (or one closely-coupled group of them, like the OAuth token and
//! authorization-code tables, or the three that make up a chat): a
//! `FromRow` struct for the row plus the
//! runtime-checked queries that touch
//! it (`sqlx::query_as::<_, T>`; never the `query!` family). Nothing here
//! knows about HTTP, though every fallible function returns
//! [`AppError`](crate::error::AppError), the crate's single error type, so
//! handlers can use `?`.

pub mod chat;
pub mod custom_emoji;
pub mod follow;
pub mod job;
pub mod media_attachment;
pub mod oauth_app;
pub mod oauth_token;
pub mod status;
pub mod user;
