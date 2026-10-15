-- Thread mutes (WBE-150): one account opting out of one status thread, the
-- fact `docs/ARCHITECTURE.md` decision 1 names for the streaming socket's
-- per-connection filter ("skip when the thread is muted"). Reference:
-- Mastodon's `conversation_mutes` table (`db/schema.rb`, `account_id` plus
-- `conversation_id`) behind `PUT /api/v1/conversations/:id/mute`, and
-- Pleroma's muted `Conversation.Participation` flag as it is read by
-- `CommonAPI.thread_muted?` (`lib/pleroma/web/common_api.ex`) from
-- `Pleroma.Web.Streamer`'s user filter
-- (`lib/pleroma/web/streamer.ex`, `filtered_by_user?`).
--
-- The thread is named by `statuses.conversation_id` (decision 6's statuses
-- table: "the root status of a conversation is its own id, every reply
-- inherits it"), so this table keys on that uuid rather than Mastodon's
-- `conversations` row — this schema has no conversations table, and the uuid
-- is already the stable thread identity every status carries. No foreign key
-- follows: a thread is a group of statuses, not a row, and deleting its
-- statuses must not decide whether the account still mutes it.
CREATE TABLE conversation_mutes (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The muting account. Deleting it deletes its thread mutes, like every
    -- other user-keyed edge table does.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The muted thread, as `statuses.conversation_id`.
    conversation_id uuid NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One mute per (account, thread) pair: the key `mute`/`unmute` are
    -- idempotent on, and the pair the streaming filter reads one user's set
    -- through.
    CONSTRAINT conversation_mutes_user_id_conversation_id_key
        UNIQUE (user_id, conversation_id)
);

-- The streaming socket's per-connection load: one account's muted threads.
-- The unique key above already serves it (its leading column is `user_id`),
-- so no second index is needed.
