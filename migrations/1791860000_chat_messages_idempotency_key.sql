-- Idempotent chat message posting (WBE-147, Pleroma's `Idempotency-Key`
-- handling on `ChatController.post_chat_message`): the key a client sent with
-- the request that created this message, stored beside its sender so a replay
-- of the same key by the same user within one hour answers that message
-- instead of writing a second one. Pleroma caches the key→id mapping in a
-- TTL cache; decision 6 keeps the fact on the row it belongs to, and the
-- window is measured against the message's own `inserted_at`.
ALTER TABLE chat_messages ADD COLUMN idempotency_key text;

-- The replay lookup: one sender's keys, newest first. Partial because only
-- keyed messages are ever looked up — the overwhelmingly common case is a
-- keyless message, which stores NULL and wants no index entry.
CREATE INDEX chat_messages_sender_id_idempotency_key_idx
    ON chat_messages (sender_id, idempotency_key, inserted_at DESC)
    WHERE idempotency_key IS NOT NULL;
