-- Bookmark folders: one account's named folders for filing bookmarks, the
-- rows behind `/api/v1/pleroma/bookmark_folders` and the `folder_id` of the
-- bookmark writes and listing.
-- Reference: Pleroma's `Pleroma.BookmarkFolder`
-- (`lib/pleroma/bookmark_folder.ex`) and its `BookmarkFolderController`
-- (`lib/pleroma/web/pleroma_api/controllers/bookmark_folder_controller.ex`);
-- a folder is a private per-account fact that federates nothing
-- (docs/ARCHITECTURE.md decision 3).
CREATE TABLE bookmark_folders (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The owning account. Deleting it deletes their folders.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The folder name, unique per account — the API's duplicate refusal
    -- is this constraint.
    name text NOT NULL,
    -- The folder's emoji: one unicode emoji or a `:shortcode:` naming a
    -- local custom emoji (the API renders the image URL from it).
    emoji text,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT bookmark_folders_user_id_name_key UNIQUE (user_id, name)
);

-- The owner's own folder list, in creation order (decision 4), is the one
-- walk this composite serves; the unique key above leads on `user_id` too,
-- but orders by name, and the list answers by id.
CREATE INDEX bookmark_folders_user_id_id_idx ON bookmark_folders (user_id, id);

-- Filing a bookmark into a folder is the foreign key the bookmarks
-- migration left in place without one. Deleting a folder KEEPS its
-- bookmarks: the column goes back to NULL (`ON DELETE SET NULL`), which is
-- Pleroma's `BookmarkFolder.delete` leaning on the same database rule.
ALTER TABLE bookmarks
    ADD FOREIGN KEY (folder_id) REFERENCES bookmark_folders(id) ON DELETE SET NULL;
