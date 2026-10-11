-- Chats: one-to-one conversations and the per-participant view of them.
-- Reference: Pleroma's `Pleroma.Chat` (lib/pleroma/chat.ex), whose `recipient`
-- is the other actor's `ActivityPub` id string, and
-- `Pleroma.Chat.MessageReference` (lib/pleroma/chat/message_reference.ex),
-- which joins a chat to a message object and remembers whether that one
-- participant has read it. Pleroma stores the message itself as a
-- `ChatMessage` activity/object (JSONB); decision 6 keeps such facts
-- relational, so the message becomes its own row here and the reference points
-- at it by id.
--
-- One chat row per *direction*: each participant keeps their own row, with
-- their own `pinned` flag and their own `updated_at` bump, because chat state
-- is per viewer (Pleroma's `chats_user_id_recipient_index` is the same
-- shape). `chat_message_references` is likewise one row per participant's
-- chat: the sender's is created read, the recipient's unread.
CREATE TABLE chats (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- Whose chat this row is. Deleting the account takes their chats, and
    -- every reference hanging off them, with it.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The other participant. Deleting them removes this direction of the
    -- chat (the other participant keeps their own row until they go too).
    recipient_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Whether the owner pinned the chat to the top of their list
    -- (`Chat.pin`/`unpin`).
    pinned boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- One row per ordered pair: the conflict target of the model's upsert,
    -- so a participant has exactly one chat row against an account.
    CONSTRAINT chats_user_id_recipient_id_key UNIQUE (user_id, recipient_id)
);

-- A user's chat list, newest activity first (the order
-- `src/models/chat.rs`'s `page_for_user` reads, keyed on `id`).
CREATE INDEX chats_user_id_updated_at_idx ON chats (user_id, updated_at DESC);
-- The opposite direction of a pair, and cleanup when an account is deleted.
CREATE INDEX chats_recipient_id_idx ON chats (recipient_id);

-- The messages themselves, shared by both participants' references.
CREATE TABLE chat_messages (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- Who wrote the message. Deleting the account takes its messages with it
    -- (and their references with them); a withdrawal is the soft `deleted_at`
    -- path instead, which keeps the row and hides it from both listings.
    sender_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The message as sanitized HTML (`render_chat_content`): escaped plain
    -- text with linkified URLs, never markdown. Emptied by a soft delete.
    content text NOT NULL DEFAULT '',
    -- The message as its source text, before formatting. Emptied too.
    raw_text text NOT NULL DEFAULT '',
    -- The one attachment this message carries, if any; a deleted attachment
    -- leaves the message standing without it.
    attachment_media_id uuid REFERENCES media_attachments(id) ON DELETE SET NULL,
    -- `ActivityPub` object id (decision 6): `{BASE_URL}/objects/{uuid}` for a
    -- message this instance owns, whatever a remote instance sent for a
    -- mirrored one. Unique, and kept after a soft delete, so a deleted remote
    -- chat message is never re-imported.
    ap_id text UNIQUE,
    -- Soft delete: when the sender withdrew the message it stays as a
    -- tombstone for the other participant, whose reference still exists.
    -- NULL means live.
    deleted_at timestamptz,
    -- Creation time: for a remote message the `published` timestamp it
    -- arrived with.
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- A remote message's tombstone, and the sender's own history.
CREATE INDEX chat_messages_sender_id_idx ON chat_messages (sender_id);

-- Which chat sees which message, and whether that side has read it.
CREATE TABLE chat_message_references (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- sorts in creation order, which is what makes it the message cursor of
    -- one chat: both sides' references for a message are written together.
    id uuid PRIMARY KEY,
    -- One participant's chat. Deleting the chat takes its references with it.
    chat_id uuid NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    -- The message the chat displays. Deleting the message removes it from
    -- both sides' listings.
    message_id uuid NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
    -- Whether this side still has to read this message; the sender's own
    -- reference is created `false`.
    unread boolean NOT NULL DEFAULT true,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One reference per chat and message: both sides see a message once, and
    -- this is the conflict target of the model's insert.
    CONSTRAINT chat_message_references_chat_id_message_id_key
        UNIQUE (chat_id, message_id)
);

-- A chat's messages, newest first (`messages_page`), keyset-paged on `id`.
CREATE INDEX chat_message_references_chat_id_idx
    ON chat_message_references (chat_id, id DESC);
-- Unread counts per chat; `false` rows (the read majority) stay out of it.
CREATE INDEX chat_message_references_chat_id_unread_idx
    ON chat_message_references (chat_id) WHERE unread;
-- The references of one message: what a message's delete cascades through, and
-- how a mirrored message is recognised on both sides.
CREATE INDEX chat_message_references_message_id_idx
    ON chat_message_references (message_id);
