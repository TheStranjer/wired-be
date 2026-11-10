-- Conversations: one row per direct-message thread, the per-account
-- participation in it, and the recipients each participation addresses
-- (WBE-123). Reference: Pleroma's `Pleroma.Conversation`
-- (lib/pleroma/conversation.ex), `Pleroma.Conversation.Participation`
-- (lib/pleroma/conversation/participation.ex) and
-- `Pleroma.Conversation.Participation.RecipientShip`
-- (lib/pleroma/conversation/participation/recipient_ship.ex).
--
-- Pleroma names a conversation by the AP `context` of the activities in it
-- and gives every recipient — local or remote — a participation row. Here a
-- participation row exists for the LOCAL side of a DM only (src/models/
-- conversation.rs), and the remote recipients ride `recipients` instead, so
-- the thread's addressees are stored without mirroring remote accounts into
-- the participation listing. `statuses.direct_conversation_id` is what ties
-- a direct status to its thread: a reply inherits its parent's, a new root
-- gets one, and anything else is NULL — only `direct` statuses belong.
CREATE TABLE conversations (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The thread's identity: Pleroma's `conversations.ap_id`, the AP
    -- context of the statuses it groups. Unique, so finding-or-creating a
    -- thread is one upsert on this column.
    ap_context text NOT NULL UNIQUE,
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- One account's side of one conversation: its unread flag, the newest
-- direct status of the thread it saw last, and the bump time that orders
-- the conversation list.
CREATE TABLE conversation_participations (
    -- UUIDv7 made in Rust (decision 4); no database-side default. It is
    -- what `pleroma.direct_conversation_id` renders and what
    -- POST /api/v1/statuses` reads back as `in_reply_to_conversation_id`.
    id uuid PRIMARY KEY,
    -- Whose side this is. Deleting the account takes it out of every
    -- conversation, like every other user-keyed edge table.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The thread. Deleting it (every account's side goes with it through
    -- the participations' own cascade) takes this row too.
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    -- Whether the account has seen `last_status_id`; the author's own post
    -- lands read, every other participant's lands unread.
    read boolean NOT NULL DEFAULT true,
    -- The conversation's newest direct status as this account's listing
    -- saw it. A hard status delete (a purged account) leaves the pointer
    -- gone; the soft-delete repoint lives in src/models/conversation.rs,
    -- because a tombstone never fires this key.
    last_status_id uuid REFERENCES statuses(id) ON DELETE SET NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- One side per (account, thread) pair: the conflict target of the
    -- model's upsert, so a participant has exactly one row per thread.
    CONSTRAINT conversation_participations_user_id_conversation_id_key
        UNIQUE (user_id, conversation_id)
);

-- The account's conversation list, newest participation's id first (the
-- keyset src/models/conversation.rs's `page_for_user` reads); the unique
-- key above already serves the per-account walk only by its leading
-- column, so this ordered index carries the id keyset.
CREATE INDEX conversation_participations_user_id_idx
    ON conversation_participations (user_id, id DESC);
-- The deletion repoint: which participations name the status just deleted.
CREATE INDEX conversation_participations_last_status_id_idx
    ON conversation_participations (last_status_id);

-- Who one participation addresses besides its owner — the thread's other
-- participants, remote accounts included. These are the mentions a post
-- made with `in_reply_to_conversation_id` carries, and the recipient list
-- `src/models/conversation.rs::participate` refreshes with every new
-- direct status.
CREATE TABLE conversation_participation_recipients (
    -- The participation this addressee belongs to. Withdrawing it (the
    -- account delete, or the repoint that ends a thread) takes the
    -- addressees with it.
    participation_id uuid NOT NULL REFERENCES conversation_participations(id) ON DELETE CASCADE,
    -- The addressed account. Deleting it simply drops it from the lists.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- One row per (participation, addressee) pair: the pair key the model
    -- rewrites wholesale, and the lookup the recipient read rides.
    PRIMARY KEY (participation_id, user_id)
);

-- `statuses.direct_conversation_id`: the DM thread a `direct` status
-- belongs to, written by src/models/conversation.rs. A conversation whose
-- last row is purged leaves the pointer gone (the participations cascade
-- with it); a soft delete keeps the column and the repoint moves the
-- participations off the tombstone instead.
ALTER TABLE statuses
    ADD COLUMN direct_conversation_id uuid REFERENCES conversations(id) ON DELETE SET NULL;

-- The thread's remaining direct statuses, which is what the deletion
-- repoint walks to find the newest one left.
CREATE INDEX statuses_direct_conversation_id_idx
    ON statuses (direct_conversation_id);
