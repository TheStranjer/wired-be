-- Pleroma extension columns on `users`: everything the `pleroma` block of
-- the `Account` entity renders, plus the account state the entity's
-- privacy rules need (docs/ARCHITECTURE.md decision 1: the HTTP shapes are
-- what must match Pleroma). Reference: Pleroma's user schema
-- (lib/pleroma/user.ex) and the migrations adding these columns to it.
ALTER TABLE users
    -- Follow-list privacy: the collections themselves, and the counters
    -- the entity renders as zero for everyone but the owner when the
    -- count is hidden.
    ADD COLUMN hide_followers boolean NOT NULL DEFAULT false,
    ADD COLUMN hide_follows boolean NOT NULL DEFAULT false,
    ADD COLUMN hide_followers_count boolean NOT NULL DEFAULT false,
    ADD COLUMN hide_follows_count boolean NOT NULL DEFAULT false,
    -- Favourites are private to the account when hidden (the Pleroma
    -- default, unlike the follow flags).
    ADD COLUMN hide_favorites boolean NOT NULL DEFAULT true,
    -- `move` behaviour, thread containment and chat: Pleroma's own
    -- defaults, which federation mirrors verbatim.
    ADD COLUMN allow_following_move boolean NOT NULL DEFAULT true,
    ADD COLUMN skip_thread_containment boolean NOT NULL DEFAULT false,
    ADD COLUMN accepts_chat_messages boolean NOT NULL DEFAULT true,
    -- Role display: whether `pleroma.is_admin`/`is_moderator` may be
    -- shown, and whether the client renders rich text for this account.
    ADD COLUMN show_role boolean NOT NULL DEFAULT true,
    ADD COLUMN no_rich_text boolean NOT NULL DEFAULT false,
    -- Visibility new statuses default to (`public`, `unlisted`,
    -- `private`, `direct`); interpreted by the statuses tickets.
    ADD COLUMN default_scope text NOT NULL DEFAULT 'public',
    -- Profile tags (`pleroma.tags`) and the stored client settings blob
    -- (`pleroma.settings_store`), which the server only round-trips.
    ADD COLUMN tags text[] NOT NULL DEFAULT '{}',
    ADD COLUMN settings_store jsonb NOT NULL DEFAULT '{}',
    -- Profile background image (`pleroma.background_image`); NULL when
    -- unset, unlike the avatar and banner which have instance defaults.
    ADD COLUMN background_image_url text,
    -- Birthday and whether strangers may see it
    -- (`pleroma.birthday` / `pleroma.show_birthday`).
    ADD COLUMN birthday date,
    ADD COLUMN show_birthday boolean NOT NULL DEFAULT false,
    -- Aliases this account is also known as (`pleroma.also_known_as`),
    -- and the actor id it moved to (the entity's top-level `moved`).
    ADD COLUMN also_known_as text[] NOT NULL DEFAULT '{}',
    ADD COLUMN moved_to_ap_id text,
    -- Account state: activation and approval as Pleroma tracks them, and
    -- the last time the account was seen (`last_active_at` is NULL for an
    -- account that never logged in).
    ADD COLUMN email_confirmed boolean NOT NULL DEFAULT true,
    ADD COLUMN approval_pending boolean NOT NULL DEFAULT false,
    ADD COLUMN last_active_at timestamptz;
