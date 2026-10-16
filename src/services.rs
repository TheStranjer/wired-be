//! Services: subsystems that are neither one table's DB access
//! ([`crate::models`]) nor HTTP handling ([`crate::api`])
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! A service module owns its data's on-disk shape and the operations that
//! reconcile disk and database; handlers call it, never the other way
//! round. The cross-table read side of the same rule lives here too:
//! [`render_status`] is the batched loader-and-renderer that composes
//! several models into one entity page, which is more than any single
//! table module owns and less than an endpoint.

pub mod emoji_packs;
pub mod instance_meta;
pub mod media_processing;
pub mod mentions;
pub mod render_status;
