-- Lists: per-user named collections of accounts, with the Mastodon v1
-- semantics (`docs/ARCHITECTURE.md` WBE-95). Reference: Pleroma's
-- `Pleroma.List` (lib/pleroma/list.ex); its `following` array of follower
-- addresses is normalised here into the `list_members` join table, its
-- `ap_id` and `emoji` fields have no place in this ticket's entity, and the
-- membership is keyed by the member's `users` row (decision 7) instead of
-- an address string. `replies_policy` is Mastodon's column of the same
-- name; Pleroma 2.10 has none.
CREATE TABLE lists (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- sorts in creation order, so the list the API shows oldest-first is a
    -- plain `ORDER BY id`.
    id uuid PRIMARY KEY,
    -- The list belongs to exactly one account, and an account's lists go
    -- with it: Pleroma's user-scoped `get`/`for_user` are the ownership
    -- boundary, enforced here by the cascade.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The user-defined title. NOT NULL; emptiness and length are API
    -- validation errors, not missing columns (Pleroma's
    -- `validate_required([:title])`).
    title text NOT NULL,
    -- Which replies the list shows (Mastodon's `List#replies_policy`):
    -- nobody's, members' only, or everyone the owner follows. The API
    -- validates the value; the check makes the set the column's rule too.
    replies_policy text NOT NULL DEFAULT 'list'
        CONSTRAINT lists_replies_policy_check
        CHECK (replies_policy IN ('none', 'list', 'followed')),
    -- Whether members of the list are removed from the owner's Home feed
    -- (Mastodon's and Pleroma's `exclusive`).
    exclusive boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Everything one account has, and the per-user ownership lookups: both the
-- list query and `WHERE id = … AND user_id = …` guards start here.
CREATE INDEX lists_user_id_idx ON lists (user_id);

-- Who is on which list: one row per membership. The pair is the key
-- `List.follow` is idempotent on (Pleroma's `Enum.uniq` on the address
-- array, expressed as the primary key here).
CREATE TABLE list_members (
    -- The list. Deleting it takes its memberships with it.
    list_id uuid NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
    -- The member. Deleting the account removes it from every list.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (list_id, user_id)
);

-- The reverse lookup `GET /api/v1/accounts/:id/lists` runs on: which lists
-- of an owner hold this member (the primary key only leads with the list).
CREATE INDEX list_members_user_id_idx ON list_members (user_id);
