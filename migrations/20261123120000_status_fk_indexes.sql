-- Indexes for the `status_id` foreign keys that status deletes scan
-- (WBE-529, CORE-7).
--
-- Each of these columns is filtered on, or cascaded into, whenever a status
-- (or chat message) goes away, and none had an index led by that column, so
-- every remote `Delete` and every purged row was a sequential scan of a
-- table that grows with federated history.

-- `DELETE FROM notifications WHERE status_id = $1` in
-- `src/models/notification/delete.rs`, run on every status tombstone, and
-- the `ON DELETE CASCADE` from `statuses`. Partial: most rows have no status.
CREATE INDEX notifications_status_id_idx ON notifications (status_id)
    WHERE status_id IS NOT NULL;

-- `DELETE FROM notifications WHERE chat_message_id = $1` in
-- `src/models/notification/delete.rs`, run when a chat message is deleted.
-- Partial: only chat-message notifications carry the column.
CREATE INDEX notifications_chat_message_id_idx ON notifications (chat_message_id)
    WHERE chat_message_id IS NOT NULL;

-- The `from_account_id = $1` arm of `DELETE_ALL_FOR_USER_SQL` in
-- `src/models/notification/delete.rs` (the `users.delete` job), and the
-- `ON DELETE CASCADE` from `users`.
CREATE INDEX notifications_from_account_id_idx ON notifications (from_account_id);

-- The `ON DELETE CASCADE` from `statuses`, hit by the hard-delete purge in
-- `src/jobs/maintenance/deleted_statuses.rs`. The unique key is led by
-- `user_id`, so it cannot serve a lookup by status.
CREATE INDEX bookmarks_status_id_idx ON bookmarks (status_id);

-- The same cascade for pins; the primary key is `(user_id, status_id)`.
CREATE INDEX pinned_statuses_status_id_idx ON pinned_statuses (status_id);

-- The same cascade for idempotency keys; the primary key is `(user_id, key)`.
CREATE INDEX idempotency_keys_status_id_idx ON idempotency_keys (status_id);
