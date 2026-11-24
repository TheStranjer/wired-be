//! Integration tests for the emoji-pack endpoints of
//! `src/api/emoji_packs.rs`: the pack listing and its pagination, the
//! single-pack view, the zip archive download and its sharing gate, the
//! util emoji map over synced `custom_emojis` rows, the static
//! `GET /emoji/{pack}/{file}` route with its media safety headers, the
//! admin write routes of WBE-211: create, metadata patch and delete
//! under `admin:write`, their validation refusals, the `custom_emojis`
//! resync they trigger and the `emoji_pack_*` moderation entries they
//! leave behind, the emoji-file routes of WBE-215 under `/packs/files`:
//! add (uploaded part and fetched URL), rename with `force`, and delete
//! — each answering the pack's full `files` object, resyncing the custom
//! emojis, and refusing an unknown pack with the 400 `{"error":"Pack
//! <name> does not exist"}` — and the install routes of WBE-216: the
//! remote listing relay, the pack download from a mock remote instance
//! (shared, fallback and refused), the uploaded-zip install and the
//! filesystem import, all under `admin:write`. The reads answer without authentication and unknown
//! or unsafe pack names answer the reads' shared 404; every write
//! demands an admin bearer token.
//!
//! The tests live in submodules by route group; `support` holds the
//! helpers shared by all of them.

#![cfg(test)]

mod download_zip_import;
mod file_deletes;
mod file_mocks;
mod file_renames;
mod file_writes;
mod install_gates;
mod pack_deletes;
mod pack_writes;
mod reads;
mod remote_checksums;
mod remote_download;
mod remote_mocks;
mod support;
