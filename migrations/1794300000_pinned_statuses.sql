-- Pinned statuses: one account pinning one of its own posts to its profile,
-- the fact behind `POST /api/v1/statuses/{id}/pin`, its `…/unpin` twin, the
-- `pinned` flag and the `pleroma.pinned_at` timestamp of the Status entity.
-- Reference: Pleroma's `pin`/`unpin` actions of
-- `lib/pleroma/web/mastodon_api/controllers/status_controller.ex` over
-- `Pleroma.Web.CommonAPI.pin`/`unpin` (`lib/pleroma/web/common_api.ex`), which
-- keep the fact as a `pinned_objects` map on the user; here it is this table,
-- one row per `(user_id, status_id)` (docs/ARCHITECTURE.md decision 3).
CREATE TABLE pinned_statuses (
    -- The pinning account. Deleting it deletes the pins it made.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The pinned status. Deleting it removes every pin on it; a soft delete
    -- clears the same rows in its transaction (see
    -- `crate::models::status::soft_delete`), because a tombstone is pinned
    -- by nobody.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- When the status was pinned; a repeat pin keeps its row and this
    -- timestamp. The profile listing orders by it, newest pin first.
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One pin per account and status: the key pin()/unpin() are idempotent
    -- on, and the pair the renderer's batched flag read asks with.
    PRIMARY KEY (user_id, status_id)
);

-- The account's own pins newest-first — the walk behind the limit check and
-- the profile listing — leads on the owner and orders on the pin time; the
-- batched flag read leads on `(user_id, status_id)` and rides the primary key.
CREATE INDEX pinned_statuses_user_id_inserted_at_idx ON pinned_statuses (user_id, inserted_at DESC);
