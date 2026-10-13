//! Small helpers shared by the rest of the crate.
//!
//! Nothing in here knows about HTTP or the database: these are the primitives
//! the layers above are built on, following `docs/ARCHITECTURE.md` — ids from
//! [`id`] (decision 4), timestamps from [`time`] (decision 5), password
//! hashes from [`password`], random OAuth tokens from [`token`], the RSA key
//! helpers in [`keys`], the HTML sanitizer in [`sanitize`], the content
//! formatter in [`mod@format`] and the mention/hashtag extraction and link
//! builders in [`entities`].

pub mod entities;
pub mod format;
pub mod id;
pub mod keys;
pub mod password;
pub mod sanitize;
pub mod time;
pub mod token;
