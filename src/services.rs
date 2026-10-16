//! Services: filesystem- and process-backed subsystems that are neither one
//! table's DB access ([`crate::models`]) nor HTTP handling
//! ([`crate::api`]) (`docs/ARCHITECTURE.md` decision 3).
//!
//! A service module owns its data's on-disk shape and the operations that
//! reconcile disk and database; handlers call it, never the other way round.

pub mod emoji_packs;
pub mod instance_actor;
pub mod instance_meta;
