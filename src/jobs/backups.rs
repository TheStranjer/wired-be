//! The account backups: the export archive a backup request describes,
//! built straight from the database into a temp file, and the two queue
//! jobs (decision 11) that turn a `backups` row into stored bytes.
//!
//! Reference: Pleroma's `Pleroma.Export.export/1`
//! (`lib/pleroma/export/import.ex`), which writes an account's actor,
//! outbox, likes and bookmarks as JSON files and zips them into
//! `archive-<nickname>-<stamp>.zip` under a temp directory. [`build`] is
//! that pure archive step: it writes the zip, returns the name and the
//! byte count of the result, and touches nothing else — no `backups`
//! row, no uploader. Around it stand this module's two job handlers:
//!
//! - [`backups.run`](crate::api::backups::KIND_RUN), handled by
//!   [`handle_run`] on [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND), is
//!   the job `POST /api/v1/pleroma/backups` enqueues with the arguments
//!   `{"backup_id": "<uuid>"}`: it loads the row and its owner, builds
//!   their archive, stores it under the key `backups/{user_id}/{name}`,
//!   and marks the row processed. Unusable arguments, a vanished row or
//!   a vanished owner discard the job — no retry can fix those; IO and
//!   database failures retry it, the run being idempotent: re-putting
//!   the same key and re-marking the same row converge, so another pass
//!   costs nothing but the wait.
//! - [`backups.purge`](KIND_PURGE), handled by [`handle_purge`] on the
//!   same queue once a day from the
//!   [`periodic_jobs`](super::scheduler::periodic_jobs) registry, sweeps
//!   every request older than
//!   [`backup_validity_days`](crate::config::Config::backup_validity_days)
//!   out of storage and out of the table — processed rows losing their
//!   file with their row, abandoned requests losing just the row. With
//!   [`backup_purge`](crate::config::Config::backup_purge) off the pass
//!   logs and deletes nothing.
//!
//! The archive itself holds exactly four entries, in this order:
//!
//! - `actor.json` — [`crate::federation::actor::actor_json`] for the
//!   account, verbatim;
//! - `outbox.json` — one `OrderedCollection` whose items are the `Create`
//!   activities of every live status the account authored across all four
//!   federated visibilities (`local` posts have no document to export);
//!   reblogs are ordinary status rows (decision 6) and ride along as
//!   `Create`, exactly as their rows stand — no `Announce` is synthesised;
//! - `likes.json` and `bookmarks.json` — the same collection shape whose
//!   items are the `statuses.uri` strings the account favourited and
//!   bookmarked, in row-id order.
//!
//! Memory is bounded by construction: each collection is one open zip
//! entry that a keyset walk streams into — a header fragment, then one
//! serialised item at a time (`PAGE_SIZE` rows per query at most), then
//! the `],"totalItems": {n}}` tail — so no page of items is ever a held
//! `serde_json::Value` and the whole archive is never a held `Vec<u8>`.
//!
//! The zip lives in a [`tempfile::NamedTempFile`] the returned
//! [`BackupArchive`] owns: the path stays private, the value's drop deletes
//! the file, and [`BackupArchive::size`] is the length measured on the
//! written file before its handle closes.
//!
//! This module is a tree: `archive` holds the zip — its entry names and
//! page step, [`BackupArchive`] and [`build`]; `run` the
//! [`handle_run`] handler with its argument parsing and storage key, and
//! [`KIND_PURGE`], the kind `purge` answers to; `purge` the
//! [`handle_purge`] sweep; `test_support` holds the fixtures the
//! children's unit tests share, and each child's `tests` its own. Every
//! kind and handler is re-exported here, so an outside path stays
//! `crate::jobs::backups::…`.

mod archive;
mod purge;
mod run;

#[cfg(test)]
mod test_support;

pub(in crate::jobs) use self::purge::sweep_archive_file;
pub use self::{
    archive::{BackupArchive, build},
    purge::handle_purge,
    run::{KIND_PURGE, handle_run},
};
