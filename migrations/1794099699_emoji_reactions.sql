-- Emoji reactions: one account reacting to one status with one emoji, the
-- fact behind `PUT`/`DELETE`/`GET
-- /api/v1/pleroma/statuses/{id}/reactions[/{emoji}]` and the
-- `pleroma.emoji_reactions` key of the Status entity. Reference: Pleroma's
-- `EmojiReact` activities (`lib/pleroma/web/common_api.ex`,
-- `react_with_emoji`/`unreact_with_emoji`, served by
-- `lib/pleroma/web/pleroma_api/controllers/emoji_reaction_controller.ex`),
-- flattened into one row per (account, status, emoji) instead of an activity
-- per reaction plus its `Undo` (docs/ARCHITECTURE.md decision 3).
CREATE TABLE emoji_reactions (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The reacting account. Deleting it deletes the reactions it made.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The status reacted to. Deleting it removes every reaction on it.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The emoji: one unicode emoji grapheme, or a custom emoji written
    -- `:shortcode:`. Validated by src/util/emoji.rs before it gets here.
    emoji text NOT NULL,
    -- Image URL of the custom emoji, recorded from `custom_emojis` at the
    -- moment of the reaction; NULL for a unicode emoji.
    url text,
    -- ActivityPub id of the EmojiReact activity behind this row (decision 6):
    -- a local reactor mints {BASE_URL}/activities/{uuid}, a mirrored one's id
    -- arrives with the federation ticket that processes the activity; NULL
    -- until an activity exists to record.
    ap_id text UNIQUE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One reaction per account, status and emoji: the key react()/unreact()
    -- are idempotent on, and the reason a repeat reaction keeps the original
    -- row and its insertion time.
    CONSTRAINT emoji_reactions_user_id_status_id_emoji_key
        UNIQUE (user_id, status_id, emoji)
);

-- Reactions are read per status — the batched roll-up that fills
-- `pleroma.emoji_reactions` and both `GET …/reactions` routes — which no
-- other key of this table serves: the unique constraint leads on `user_id`.
CREATE INDEX emoji_reactions_status_id_idx ON emoji_reactions (status_id);
