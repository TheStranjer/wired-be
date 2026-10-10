//! Small helpers shared by the rest of the crate.
//!
//! Nothing in here knows about HTTP or the database: these are the primitives
//! the layers above are built on, following `docs/ARCHITECTURE.md` — ids from
//! [`id`] (decision 4), timestamps from [`time`] (decision 5), password
//! hashes from [`password`], the RSA key helpers in [`keys`] and the HTML
//! sanitizer in [`sanitize`].

pub mod id;
pub mod keys;
pub mod password;
pub mod sanitize;
pub mod time;
