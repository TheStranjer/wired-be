//! Small helpers shared by the rest of the crate.
//!
//! Nothing in here knows about HTTP or the database: these are the primitives
//! the layers above are built on, following `docs/ARCHITECTURE.md` — ids from
//! [`id`] (decision 4) and the random OAuth credentials from [`token`].

pub mod id;
pub mod token;
