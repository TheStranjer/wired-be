-- Mastodon v2 filters (`/api/v2/filters`): one filter row per titled rule, the
-- keywords it matches and the whole statuses it hides. Mastodon's
-- `custom_filters`, `custom_filter_keywords` and `custom_filter_statuses`
-- tables, with this project's ids (`UUIDv7` made in Rust, decision 4).
--
-- The v1 `filters` table is migrated into these tables and dropped: a v1
-- filter becomes a v2 filter holding one keyword, and the `/api/v1/filters`
-- routes are served from the same rows, one v1 filter per keyword, as
-- Mastodon does.
CREATE TABLE custom_filters (
    -- UUIDv7 made in Rust; no database-side default.
    id uuid PRIMARY KEY,
    -- The owner; an account's filters go with it.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The name the owner gave the filter. For a migrated v1 filter, its
    -- phrase.
    title text NOT NULL,
    -- Where the filter applies: a non-empty subset of `home`,
    -- `notifications`, `public`, `thread`, `account`. The API validates.
    context text[] NOT NULL,
    -- What a match does: `warn` (annotate the status), `hide` (drop it from
    -- the response) or `blur` (annotate, clients blur the media).
    filter_action text NOT NULL DEFAULT 'warn'
        CHECK (filter_action IN ('warn', 'hide', 'blur')),
    -- When the filter stops applying; NULL means it never does. Expiry is a
    -- query condition, so no cleanup job exists.
    expires_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX custom_filters_user_id_idx ON custom_filters (user_id);

CREATE TABLE custom_filter_keywords (
    -- UUIDv7 made in Rust. A migrated v1 filter keeps its id here, so the
    -- id a v1 client holds still names the same filter.
    id uuid PRIMARY KEY,
    -- The filter the keyword belongs to; the keyword goes with it.
    filter_id uuid NOT NULL REFERENCES custom_filters(id) ON DELETE CASCADE,
    -- The phrase to look for, verbatim as its owner wrote it.
    keyword text NOT NULL,
    -- Whether the phrase must appear as a whole word.
    whole_word boolean NOT NULL DEFAULT true,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX custom_filter_keywords_filter_id_idx ON custom_filter_keywords (filter_id);

CREATE TABLE custom_filter_statuses (
    -- UUIDv7 made in Rust.
    id uuid PRIMARY KEY,
    -- The filter the row belongs to; the row goes with it.
    filter_id uuid NOT NULL REFERENCES custom_filters(id) ON DELETE CASCADE,
    -- The status the filter matches whole; the row goes with the status.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- A status is added to a filter once.
    UNIQUE (filter_id, status_id)
);

CREATE INDEX custom_filter_statuses_status_id_idx ON custom_filter_statuses (status_id);

-- A v1 filter becomes a v2 filter with the same id, titled by its phrase;
-- `irreversible` becomes the `hide` action.
INSERT INTO custom_filters (id, user_id, title, context, filter_action, expires_at,
                            inserted_at, updated_at)
SELECT id, user_id, phrase, context,
       CASE WHEN irreversible THEN 'hide' ELSE 'warn' END,
       expires_at, inserted_at, updated_at
  FROM filters;

-- Its phrase is the filter's one keyword, under the same id again.
INSERT INTO custom_filter_keywords (id, filter_id, keyword, whole_word, inserted_at, updated_at)
SELECT id, id, phrase, whole_word, inserted_at, updated_at
  FROM filters;

DROP TABLE filters;
