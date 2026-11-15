-- Polls: the question a status asks and the votes cast on it (WBE-93) — the
-- shape behind a `POST /api/v1/statuses` that carries `poll[options]`, the
-- Poll entity attached to a status, `GET /api/v1/statuses/{id}/poll` and
-- `POST /api/v1/statuses/{id}/votes`.
-- Reference: Pleroma keeps a poll inside the status's object — a `Question`
-- whose `oneOf`/`anyOf` are its options, built by `make_poll_data` of
-- `lib/pleroma/web/common_api/utils.ex` and rendered by
-- `lib/pleroma/web/mastodon_api/views/poll_view.ex`, with a vote recorded as
-- an `Answer` activity in the object's collection. Here it is three
-- relational tables instead (docs/ARCHITECTURE.md decisions 3 and 6): one
-- poll per status, one row per option, one row per mark, so voting counts
-- per row rather than rewriting the post.
CREATE TABLE polls (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The status asking the question. Unique: a status has at most one poll,
    -- which is the key a create rides on and the column every read of a
    -- stored poll goes through. Deleting the status deletes its poll, and
    -- below it its options and votes.
    status_id uuid NOT NULL UNIQUE REFERENCES statuses(id) ON DELETE CASCADE,
    -- When voting closes; NULL for a poll that never expires. The API layer
    -- turns Mastodon's `poll[expires_in]` seconds into this instant.
    expires_at timestamptz,
    -- Whether a voter may mark more than one option (Pleroma's `anyOf`
    -- rather than `oneOf`).
    multiple boolean NOT NULL DEFAULT false,
    -- Whether per-option counts stay hidden while the poll runs.
    hide_totals boolean NOT NULL DEFAULT false,
    -- Distinct accounts that have voted. A column, not a read: the vote
    -- endpoint's transaction moves it.
    voters_count integer NOT NULL DEFAULT 0,
    -- Votes cast in total — above the voter count once `multiple` lets each
    -- voter tick several boxes.
    votes_count integer NOT NULL DEFAULT 0,
    -- When the poll was created; stamped by the insert that made the row.
    inserted_at timestamptz NOT NULL
);

-- One option of a poll. The option's `id` is what a vote names, and its
-- `position` is the index a client votes by (`choices=[0,2]`) and the order
-- the entity lists options in.
CREATE TABLE poll_options (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The poll this option belongs to; deleting the poll deletes its
    -- options. Nullable because the option rows are written with their poll
    -- in the same transaction by `crate::models::poll::create`, which always
    -- names the poll it made; nothing in this instance stores an orphan.
    poll_id uuid REFERENCES polls(id) ON DELETE CASCADE,
    -- 0-based order within the poll, and the index the vote endpoint and the
    -- entity's `own_votes` speak.
    position integer NOT NULL,
    -- The option's text as its author wrote it.
    title text NOT NULL,
    -- Votes this option has; a column the vote transaction moves.
    votes_count integer NOT NULL DEFAULT 0,
    -- One row per slot of one poll: what the batched insert cannot overflow,
    -- and the key the options of a poll read from in order (`poll_id` leads,
    -- so the constraint's index serves that walk).
    CONSTRAINT poll_options_poll_id_position_key UNIQUE (poll_id, position)
);

-- One mark cast by one account on one option. There is no id column: the
-- triple is the fact, it is what `POST …/votes` inserts (or skips) for each
-- choice it is given, and a voter taking a mark back deletes the row — so a
-- voter cannot tick the same option twice.
CREATE TABLE poll_votes (
    -- The poll this mark belongs to; deleting the poll removes its votes.
    poll_id uuid REFERENCES polls(id) ON DELETE CASCADE,
    -- The option ticked; deleting that option removes the marks on it.
    option_id uuid REFERENCES poll_options(id) ON DELETE CASCADE,
    -- The account that cast the mark; deleting the account withdraws them.
    user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    -- When the mark was cast; stamped by the insert.
    inserted_at timestamptz NOT NULL,
    -- The whole fact: one mark per option per account per poll. The primary
    -- key leads on `poll_id`, so "this voter's marks on this poll" — the
    -- `voted` flag and `own_votes` of the entity — is one index walk.
    PRIMARY KEY (poll_id, option_id, user_id)
);
