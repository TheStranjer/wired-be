//! Domain types and database access.
//!
//! Per `docs/ARCHITECTURE.md` decisions 2 and 3 each module owns one table
//! (or one closely-coupled group of them, like the OAuth token and
//! authorization-code tables, or the three that make up a chat): a
//! `FromRow` struct for the row plus the
//! runtime-checked queries that touch
//! it (`sqlx::query_as::<_, T>`; never the `query!` family). One module is
//! the exception: [`relationship`] owns no table; it is the batched loader
//! that reads `follows` to build the `Relationship` entity (and will read
//! each later flag table as its ticket lands). Nothing here
//! knows about HTTP, though every fallible function returns
//! [`AppError`](crate::error::AppError), the crate's single error type, so
//! handlers can use `?`.

pub mod account_note;
pub mod announcement;
pub mod block;
pub mod chat;
pub mod custom_emoji;
pub mod domain_block;
pub mod endorsement;
pub mod filter;
pub mod follow;
pub mod hashtag;
pub mod instance_policy_domain;
pub mod instance_rule;
pub mod instance_setting;
pub mod job;
pub mod list;
pub mod marker;
pub mod media_attachment;
pub mod mention;
pub mod oauth_app;
pub mod oauth_login_state;
pub mod oauth_token;
pub mod relationship;
pub mod scrobble;
pub mod status;
pub mod user;
pub mod user_identity;
