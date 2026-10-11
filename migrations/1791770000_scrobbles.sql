-- Scrobbles: what an account listened to, Pleroma's `Listen` activities
-- (WBE-121). Reference: `Pleroma.Web.PleromaAPI.ScrobbleController`
-- (lib/pleroma/web/pleroma_api/controllers/scrobble_controller.ex), which
-- stores a listen as a `Listen` activity wrapping an `Audio` object whose
-- `title`/`artist`/`album`/`length`/`externalLink` keys are the media
-- metadata (`lib/pleroma/web/common_api/activity_draft.ex`,
-- `listen_object/1`). Per docs/ARCHITECTURE.md decision 6 those activities
-- are relational here, like statuses: one row per listen, its actor becomes
-- `user_id` and its `published` time becomes `inserted_at`.
CREATE TABLE scrobbles (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- sorts in creation order, so "newest first" is a plain `ORDER BY id
    -- DESC` and the id is the pagination cursor (decision 4).
    id uuid PRIMARY KEY,
    -- The account that scrobbled. Deleting it takes its scrobbles with it,
    -- as Pleroma's activities of a deleted user are removed.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The title of the media playing. NOT NULL and required by the API: an
    -- empty or absent title is a 422 there, never a row here (Pleroma's
    -- `create_request()` lists `title` as the one required property); the
    -- 255-character limit is the endpoint's, so nothing truncates here.
    title text NOT NULL,
    -- The artist and the album, both optional (nullable in Pleroma's
    -- object, which simply has no key when the client sent none).
    artist text,
    album text,
    -- Length of the media in milliseconds, as the client reported it; NULL
    -- when it reported none. `integer` is what Pleroma's `length` is.
    length integer,
    -- URL referencing the media (Pleroma's object `externalLink`, which its
    -- controller folds out of the `externalLink` parameter; decision 3
    -- spells columns in snake_case, so `external_link` here).
    external_link text,
    -- Who may see the scrobble (`src/models/status.rs`, `Visibility`). The
    -- endpoint documents `public` as the default and accepts `unlisted`,
    -- `private` and `direct`; `local` is a status visibility only, so the
    -- check keeps it out of this column, and `GET
    -- /api/v1/pleroma/accounts/{id}/scrobbles` hides the two private ones
    -- from everybody but their owner.
    visibility text NOT NULL DEFAULT 'public'
        CONSTRAINT scrobbles_visibility_check
        CHECK (visibility IN ('public', 'unlisted', 'private', 'direct')),
    -- When the listen happened, which for a scrobble is when it was posted:
    -- the entity's `created_at`. Scrobbles are never edited, so this table
    -- has no `updated_at`.
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- One account's recent scrobbles, newest first: what
-- `GET /api/v1/pleroma/accounts/{id}/scrobbles` reads, cursor included, so
-- the listing walks the index backwards instead of sorting the account's
-- whole history per page.
CREATE INDEX scrobbles_user_id_idx ON scrobbles (user_id, id DESC);
