-- Favourites: one account favouriting one status, the fact behind
-- `POST /api/v1/statuses/{id}/favourite`, its `…/unfavourite` twin,
-- `GET /api/v1/statuses/{id}/favourited_by` and `GET /api/v1/favourites`,
-- and the `favourited` flag and `favourites_count` of the Status entity.
-- Reference: Pleroma's `Like` activities (`lib/pleroma/web/common_api.ex`,
-- `favorite`/`unfavorite`, served by the `favourite`, `unfavourite`,
-- `favourited_by` and `favourites` actions of
-- `lib/pleroma/web/mastodon_api/controllers/status_controller.ex`),
-- flattened into one row per (account, status) instead of an activity per
-- favourite plus its `Undo` (docs/ARCHITECTURE.md decision 3).
CREATE TABLE favourites (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The favouriting account. Deleting it deletes the favourites it made.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The status favourited. Deleting it removes every favourite on it.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- ActivityPub id of the Like activity behind this row (decision 6): a
    -- local account mints {BASE_URL}/activities/{uuid}, a mirrored one's id
    -- arrives with the federation ticket that processes the activity; NULL
    -- until an activity exists to record.
    ap_id text UNIQUE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One favourite per account and status: the key favourite()/unfavourite()
    -- are idempotent on.
    CONSTRAINT favourites_user_id_status_id_key UNIQUE (user_id, status_id)
);

-- Who favourited a given status (`GET …/favourited_by`) and the batched
-- `favourited` flag read of a whole page of statuses are the directions the
-- unique key above, which leads on the account, cannot serve.
CREATE INDEX favourites_status_id_idx ON favourites (status_id);

-- The owner's own list, newest favourite first and keyed on the favourite
-- row id (decision 4, `GET /api/v1/favourites`), is the one walk this
-- composite serves: the account picks the rows, the id orders the page.
CREATE INDEX favourites_user_id_id_desc_idx ON favourites (user_id, id DESC);
