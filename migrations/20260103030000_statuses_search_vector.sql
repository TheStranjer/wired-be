-- Search over status text (WBE-128): one generated `tsvector` column and
-- its GIN index, the read behind `GET /api/v1/search` / `GET /api/v2/search`
-- with `type=statuses` (src/models/status_search.rs). Reference: Pleroma's
-- `Pleroma.Activity.Search` (migrations/structure.sql, the
-- `activities_object_data_fts_index` GIN index over the object content);
-- here the text lives in its own columns (decision 6), so the vector is
-- generated from them and stays in step with every write by itself.
--
-- The `simple` configuration: no stemming and no stop words, so a search
-- finds the words the author wrote, in any language (the ticket's
-- language-agnostic choice, against Pleroma's default text search config).
-- `spoiler_text` joins the vector because a content warning is text a
-- searcher can legitimately search for. Both columns are NOT NULL, so the
-- `coalesce` calls are belt-and-braces against a hand-set NULL, not load
-- bearing.
--
-- The vector is only meaningful for non-reblog rows: a reblog carries no
-- text of its own (decision 6), so its vector is empty and it can never
-- match; the search still excludes reblog rows explicitly, because what a
-- reblog "contains" is the original's text, which the original itself
-- answers with.
ALTER TABLE statuses ADD COLUMN search_vector tsvector
    GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(raw_text, '') || ' ' || coalesce(spoiler_text, ''))
    ) STORED;

-- The match itself: `search_vector @@ plainto_tsquery('simple', q)` reads
-- through this index (the same GIN choice Pleroma makes for its fts
-- column).
CREATE INDEX statuses_search_vector_idx ON statuses USING GIN (search_vector);
