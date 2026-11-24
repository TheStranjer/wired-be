-- The `Idempotency-Key` of the request that parked a schedule (WBE-669,
-- SVC1-7).
--
-- A scheduled create used to claim no replay key, so a client retry after a
-- timeout parked a second schedule and both fired. Storing the key on the row
-- lets the create answer the existing schedule instead (Mastodon's
-- `PostStatusService` does the same with `@account.scheduled_statuses`).
-- NULL for rows parked without a key and for rows that predate the column.
ALTER TABLE scheduled_statuses ADD COLUMN idempotency_key TEXT;

-- One key names one schedule per account; the partial index also serves the
-- replay lookup.
CREATE UNIQUE INDEX scheduled_statuses_user_id_idempotency_key_key
    ON scheduled_statuses (user_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
