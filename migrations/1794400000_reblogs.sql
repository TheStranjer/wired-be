-- Reblogs: a user can reblog a given status once (decision 6 — a reblog is
-- a status row with `reblog_of_id` set). The live pair is unique; a
-- soft-deleted reblog leaves the pair free again, which is why the index is
-- partial on `deleted_at IS NULL`. `create_reblog` (src/models/status.rs)
-- rides this index with `ON CONFLICT ... DO NOTHING` to make a repeat
-- reblog idempotent, and `reblogged_status_ids` reads the flag through it.
-- Other rows carry NULL `reblog_of_id`, and a unique index never collides
-- on NULLs, so ordinary posts are untouched.
CREATE UNIQUE INDEX statuses_user_id_reblog_of_id_key ON statuses (user_id, reblog_of_id)
    WHERE deleted_at IS NULL;
