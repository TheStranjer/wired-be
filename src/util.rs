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
//! as one reaction emoji in [`emoji`], the HMAC-SHA256 helpers in [`mod@hmac`],
//! the `LIKE` pattern escaper in [`like`] and the one boolean-parameter
//! reading in [`params`].
//! Time-expiring in-memory values go through [`cache`], the batched
//! group-by-key fold lives in [`collections`], and the `http(s)` URL
//! checks every origin, signing and delivery guard shares live in [`mod@url`],
//! the one `Content-Type` media-type parser in [`media_type`], and the
//! U+0000 strippers (the database cannot store NUL) in [`nul`], and the
//! saturating narrowing of counts to `i32` in [`numbers`].

pub mod cache;
pub mod collections;
pub mod emoji;
pub mod entities;
pub mod format;
pub mod headers;
pub mod hmac;
pub mod id;
pub mod keys;
pub mod like;
pub mod media_type;
pub mod nul;
pub mod numbers;
pub mod params;
pub mod password;
pub mod sanitize;
pub mod time;
pub mod token;
pub mod totp;
pub mod url;
