-- Notifications: one account being told something happened to them — a
-- favourite, a reblog, an emoji reaction, a follow, a mention, ... — the
-- rows behind the grouped `GET /api/v2/notifications` API
-- (`src/api/notifications_v2.rs`). Reference: Pleroma's `Notification`
-- schema (`priv/repo/migrations/20200602125217_add_group_key_to_notifications.exs`
-- and friends, `lib/pleroma/notification.ex`), flattened onto relational
-- columns like everything else that federates (docs/ARCHITECTURE.md
-- decision 6): the actor and the status are foreign keys, not an embedded
-- activity.
--
-- This is the table the earlier tickets defer to: `reports` promises
-- `notifications.report_id REFERENCES reports(id) ON DELETE CASCADE`
-- (migrations/1792300000_reports.sql) and `markers` names this table as the
-- source of its `notifications`-timeline unread count
-- (`src/models/marker.rs`); both land here.
CREATE TABLE notifications (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- order is the timeline order the cursors and the marker compare on.
    id uuid PRIMARY KEY,
    -- The account the notification is *for*. Deleting it takes its
    -- notifications with it.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The account that did it (Mastodon's `from_account_id`). Deleting the
    -- actor takes the notifications it caused: an entry whose account
    -- cannot be rendered is no entry at all.
    from_account_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Mastodon's notification type name: `mention`, `status`,
    -- `follow_request`, `reblog`, `favourite`, `follow`, `move`, `update`,
    -- `poll`, `admin.sign_up`, `admin.report`, `pleroma:emoji_reaction`,
    -- `pleroma:chat_mention`, `pleroma:report`, ... Kept as free text (no
    -- check constraint): the set grows with every ticket that *causes* a
    -- notification, and an unknown type must read as an ungroupable
    -- notification, not as a broken row.
    type text NOT NULL,
    -- The status the notification is about, `NULL` for the types that name
    -- none (a follow, a follow request, a moved account). Deleting the
    -- status takes the notifications about it with it, so no group ever
    -- outlives its subject.
    status_id uuid REFERENCES statuses(id) ON DELETE CASCADE,
    -- The emoji of a `pleroma:emoji_reaction` notification (a unicode
    -- grapheme or a `:shortcode:`, the same spelling `emoji_reactions`
    -- stores); `NULL` for every other type. It is part of the group key, so
    -- the same status reacted to with two emojis is two groups.
    emoji text,
    -- The report of a `pleroma:report` notification — the key
    -- `reports` deferred to this table (migrations/1792300000_reports.sql);
    -- deleting the report takes its notification, and `NULL` for every
    -- other type.
    report_id uuid REFERENCES reports(id) ON DELETE CASCADE,
    -- The group this notification belongs to, computed at insert time
    -- (`src/models/notification.rs`): `<type>-<status_id>` — `…-none` for a
    -- status-less row — and for a reaction the emoji too,
    -- `<type>-<status_id>-<emoji>`. `NULL` for every type the grouped API
    -- does not group: those read as the unique key `ungrouped-<id>`, which
    -- is derived from the id rather than stored so one rule cannot drift
    -- from the row's own identity.
    group_key text,
    -- Whether the account has read past this notification. The marker
    -- endpoints own the read position; this flag is Pleroma's per-row
    -- `seen`, and the unread counts ignore the rows it marks.
    seen boolean NOT NULL DEFAULT false,
    -- Whether the account dismissed this notification (and with it its
    -- group). Dismissal hides rather than deletes: the rows stay for the
    -- later tickets that count on them, and every read filters them out.
    dismissed boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- The recipient's newest-first walk — the grouped listing reads a page of
-- the account's notifications and groups them in place (decision 4: the id
-- orders the page).
CREATE INDEX notifications_user_id_id_desc_idx ON notifications (user_id, id DESC);

-- One group within one account: `GET`/`…/accounts`/`…/dismiss` for a stored
-- `group_key` all look the group up by exactly this pair.
CREATE INDEX notifications_user_id_group_key_idx ON notifications (user_id, group_key);
