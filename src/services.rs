//! Services: subsystems that are neither one table's DB access
//! ([`crate::models`]) nor HTTP handling ([`crate::api`])
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! A service module owns its data's on-disk shape and the operations that
//! reconcile disk and database; handlers call it, never the other way
//! round. The cross-table read side of the same rule lives here too:
//! [`render_status`] is the batched loader-and-renderer that composes
//! several models into one entity page, which is more than any single
//! table module owns and less than an endpoint. [`media_proxy`] is the
//! cross-cutting resident of the same home: it rewrites remote media URLs
//! into signed proxy links (used by the [`entities`](crate::entities)
//! builders) and serves those links over HTTP (its router is merged at
//! the top level by [`crate::app`]). [`create_status`] is the write side of
//! the same idea: it decides what a `POST /api/v1/statuses` body means — its
//! limits, its defaults, its refusals — from the request parameters alone, so
//! the endpoint that stores the result never re-decides any of it.

pub mod create_status;
pub mod emoji_packs;
pub mod instance_actor;
pub mod instance_meta;
pub mod media_processing;
pub mod media_proxy;
pub mod mentions;
pub mod render_status;
