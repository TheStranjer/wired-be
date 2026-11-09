-- users.deleted_at (WBE-136): the account-deletion marker set by
-- `POST /api/pleroma/delete_account`. The row itself is kept (referential
-- integrity, decision 7: one table for local and remote accounts); the
-- `users.delete` job (src/jobs/deletion.rs) then purges its personal data.
-- NULL until the account starts its deletion, like `statuses.deleted_at`
-- (migrations/1791712088_statuses.sql) for a live post.
ALTER TABLE users ADD COLUMN deleted_at timestamptz;
