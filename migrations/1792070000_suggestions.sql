-- Suggestions (WBE-124): the staff-picked pool the suggestion endpoints
-- answer from, and the per-viewer dismissal behind
-- `DELETE /api/v1/suggestions/{account_id}`. Reference: Pleroma's
-- `is_suggested` field of `User.Query` and its `suggestion_dismiss`
-- relationship type
-- (`lib/pleroma/web/mastodon_api/controllers/suggestion_controller.ex`,
-- `lib/pleroma/user_relationship.ex`).
ALTER TABLE users
    -- Whether an admin put this account in the suggestion pool. The admin
    -- API (a later ticket) is the only writer; reads only filter on it.
    ADD COLUMN is_suggested boolean NOT NULL DEFAULT false;

-- One viewer having dismissed one suggested account. Pleroma stores this as
-- a `:suggestion_dismiss` edge of its shared `user_relationships` table;
-- here it is a table of its own, keyed by exactly the pair the endpoint
-- works on: one dismissal per viewer and account, and deleting either
-- account takes the dismissal with it. The `is_suggested` flag is *not*
-- cleared — a dismissal is private to the viewer who made it.
--
-- The `(user_id, target_id)` primary key is also the only index the reads
-- need: every dismissal check is one viewer's own set (the suggestion
-- listing's `NOT EXISTS` and the model's read-back), and the pool the
-- listing walks is small by construction (admins add to it by hand), so it
-- gets no index of its own.
CREATE TABLE dismissed_suggestions (
    -- The viewer who dismissed the suggestion.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The suggested account they dismissed.
    target_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- One dismissal per viewer and account; the key `dismiss` is
    -- idempotent on.
    PRIMARY KEY (user_id, target_id)
);
