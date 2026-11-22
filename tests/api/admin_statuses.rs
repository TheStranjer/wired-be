//! The admin statuses surface over HTTP (`/api/v1/pleroma/admin/statuses…`,
//! WBE-201): Pleroma's `AdminAPI.StatusController` beside
//! `AdminAPIController.list_user_statuses` and
//! `AdminAPI.InstanceController.list_statuses`.
//!
//! What the tests pin, per the ticket:
//!
//! - `GET …/statuses` is the network ledger, newest post first, off the
//!   `public`/`unlisted` visibilities alone until `godmode` admits the
//!   private ones, narrowed by `local_only` and opened to boost rows by
//!   `with_reblogs`, windowed by `page`/`page_size` with the admin API's
//!   lenient defaulting and the shared 200-row cap — answered as a bare
//!   array of the STANDARD [`Status`] entities, the author's ordinary
//!   [`Account`] entity beside each and no admin extension;
//! - `GET …/statuses/{id}` shows any live post whatever its visibility, and
//!   a malformed, unknown or tombstoned id is the one 404
//!   `{"error":"Not found"}`;
//! - `PUT …/statuses/{id}` moves the two scope fields and nothing else — the
//!   entity answers, the row agrees, `edited_at` holds, the `status_update`
//!   moderation sentence names the parts, a `visibility` off the five is the
//!   422 `{"error":"Invalid visibility"}` with the row left as it was, and a
//!   local post's correction is the `Update` the queue holds while a
//!   mirrored or never-federated one queues nothing;
//! - `DELETE …/statuses/{id}` is the shared deletion service: `200 {}`, the
//!   tombstone, the `status_delete` entry, the federated `Delete`, and the
//!   404 for the second strike;
//! - `GET …/users/{nickname}/statuses` and `GET …/instances/{instance}/statuses`
//!   answer Pleroma's `{total, activities}` envelope — the one account's and
//!   the one domain's (`local` being this instance's own authors) under the
//!   same filters and window, the total the whole set the filters select,
//!   and the unknown nickname the same 404;
//! - every route sits behind [`StaffUser`] — a moderator passes where a
//!   regular account is refused — plus its scope: `admin:read:statuses` on
//!   the four reads, `admin:write:statuses` on the two writes, another
//!   admin area's grant, a bare `read` and an app-only token all refused.
//!
//! Statuses are seeded through [`wired_be::models::status`] directly (the
//! posting endpoint is `statuses.rs`'s business), and the newest-first
//! expectations read the stored `ORDER BY s.id DESC` order back, so nothing
//! depends on which millisecond a fixture's `UUIDv7` landed in.
//!
//! [`StaffUser`]: wired_be::auth::admin::StaffUser
//! [`Status`]: wired_be::entities::status::Status
//! [`Account`]: wired_be::entities::account::Account

#![cfg(test)]

mod support;
