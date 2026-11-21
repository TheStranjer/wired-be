//! The unit-test fixtures the media children share.
//!
//! The magic-byte files the detector and the upload tests read, the file
//! part built from them, and the prefix-only uploader a row is built
//! with. The fixtures name a type for the detector and nothing more: none
//! of them is a complete file, because detection reads magic bytes and no
//! test opens the container.

use axum::body::Bytes;

use crate::{config::Config, extract::multipart::UploadedFile, uploads::UploaderBackend};

use super::FILE_FIELD;

/// A `PNG` signature and the start of an `IHDR` chunk.
pub(super) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// A `JPEG`: `SOI` followed by the `APP0` marker.
pub(super) const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01";
/// An `MP3`: an `ID3v2` tag header with a zero-length tag.
pub(super) const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
/// An `MP4`: an `ftyp` box with the `mp42` brand.
pub(super) const MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
/// A `PDF`: bytes with a type, just not one of the three accepted ones.
pub(super) const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Byte soup: nothing `infer` can name.
pub(super) const NOTHING: &[u8] = b"just some plain text, definitely not media\n";
/// A truncated `PNG` signature: the start of a file every matcher needs
/// more of.
pub(super) const TOO_SHORT: &[u8] = b"\x89P";
/// No bytes at all, which is what a zero-length part holds.
pub(super) const EMPTY: &[u8] = b"";

/// A file part named `file` holding `bytes`, sent as `client_type`.
pub(super) fn file_part(filename: &str, client_type: &str, bytes: &[u8]) -> UploadedFile {
    UploadedFile {
        field: FILE_FIELD.to_owned(),
        filename: Some(filename.to_owned()),
        content_type: Some(client_type.to_owned()),
        bytes: Bytes::copy_from_slice(bytes),
    }
}

/// The uploader an attachment is built with: a prefix only, because
/// building a row reads and writes nothing.
pub(super) fn uploader() -> UploaderBackend {
    UploaderBackend::from(&Config::for_tests())
}
