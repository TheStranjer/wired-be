//! Small helpers shared by the rest of the crate.
//!
//! Little in here knows about HTTP (only the shared response header values
//! in [`headers`]) or the database: these are the primitives
//! the layers above are built on, following `docs/ARCHITECTURE.md` — ids from
//! [`id`] (decision 4), timestamps from [`time`] (decision 5), password
//! hashes from [`password`], random OAuth tokens from [`token`], the RSA key
//! helpers in [`keys`], the TOTP and backup-code helpers in [`totp`], the HTML
//! sanitizer in [`sanitize`], the content formatter in [`mod@format`], the
//! mention/hashtag extraction and link builders in [`entities`], what counts
//! as one reaction emoji in [`emoji`] and the XML escaper in [`xml`].
//! Time-expiring in-memory values go through [`cache`], the batched
//! group-by-key fold lives in [`collections`], and the `http(s)` URL
//! checks every origin, signing and delivery guard shares live in [`mod@url`].

pub mod cache;
pub mod collections;
pub mod emoji;
pub mod entities;
pub mod format;
pub mod headers;
pub mod id;
pub mod keys;
pub mod password;
pub mod sanitize;
pub mod time;
pub mod token;
pub mod totp;
pub mod url;
pub mod xml;
