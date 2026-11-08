-- Bookmarks: one account bookmarking one status, the fact behind
-- `POST /api/v1/statuses/{id}/bookmark`, its `…/unbookmark` twin,
-- `GET /api/v1/bookmarks`, and the `bookmarked` flag of the Status entity.
-- Reference: Pleroma's `bookmark`, `unbookmark` and `bookmarks` actions of
-- `lib/pleroma/web/mastodon_api/controllers/status_controller.ex` over
-- `Pleroma.Bookmark` (`lib/pleroma/bookmark.ex`), a private fact that
-- federates nothing (docs/ARCHITECTURE.md decision 3).
CREATE TABLE bookmarks (
    -- UUIDv7 made in Rust (decision 4); no database-side default. The
    -- `GET /api/v1/bookmarks` page keys its cursors and `Link` header on
    -- this id, not the status id.
    id uuid PRIMARY KEY,
    -- The bookmarking account. Deleting it deletes the bookmarks it made.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The bookmarked status. Deleting it removes every bookmark on it.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The bookmark's folder. The folders ticket adds the `bookmark_folders`
    -- table and this column's foreign key; until then every bookmark is
    -- unstored (`NULL`) and `folder_id` on the API is accepted and ignored.
    folder_id uuid,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One bookmark per account and status: the key add()/remove() are
    -- idempotent on.
    CONSTRAINT bookmarks_user_id_status_id_key UNIQUE (user_id, status_id)
);

-- The owner's own list, newest bookmark first and keyed on the bookmark
-- row id (decision 4, `GET /api/v1/bookmarks`), is the one walk this
-- composite serves: the account picks the rows, the id orders the page.
-- The batched `bookmarked` flag read leads on `(user_id, status_id)` and
-- rides the unique key above.
CREATE INDEX bookmarks_user_id_id_desc_idx ON bookmarks (user_id, id DESC);
