-- Partial indexes for the two periodic expiry sweeps (WBE-552, CORE-8).
--
-- Both sweeps match almost no rows, yet with no index on `expires_at` each
-- run was a sequential scan of a table that grows with federated history.
-- Each index holds only the rows its sweep can ever return, so it stays
-- tiny and the sweep reads it in `expires_at` order.

-- `EXPIRED_SQL` in `src/models/status/delete.rs`, run by the
-- `statuses.expire_sweep` job: live statuses with an expiry, ordered by
-- `expires_at, id`. The index carries that exact order and predicate.
CREATE INDEX statuses_expires_at_live_idx ON statuses (expires_at, id)
    WHERE deleted_at IS NULL AND expires_at IS NOT NULL;

-- `FETCH_ENDED_UNNOTIFIED_SQL` in `src/models/poll/ending.rs`, run by the
-- `polls.end_notify` job: polls not yet announced as ended whose expiry
-- falls in a recent window. Polls drop out of the index once notified.
CREATE INDEX polls_expires_at_unnotified_idx ON polls (expires_at)
    WHERE NOT ended_notified AND expires_at IS NOT NULL;
