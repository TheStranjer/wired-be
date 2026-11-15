-- Reports (WBE-120): an account telling the moderators about another
-- account, with optional status context and rule references. Reference:
-- Pleroma's `CommonAPI.report` (`lib/pleroma/web/common_api.ex`) and its
-- `Flag` activities (`Pleroma.Web.ActivityPub.ActivityPub.flag`), plus
-- `Pleroma.ReportNote` (`lib/pleroma/report_note.ex`). Pleroma stores a
-- report as an `Activity` whose `data` map holds the actor, the reported
-- account and the reported statuses; per `docs/ARCHITECTURE.md` decision 6
-- that becomes one relational row here plus the two join tables the ticket
-- names, and the `ap_id` column is where the federated `Flag` id will live
-- (federation of the activity is a later ticket, see
-- `src/api/reports.rs`'s `after_report_created` hook).
CREATE TABLE reports (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- sorts in creation order, which is what the listing pages on.
    id uuid PRIMARY KEY,
    -- The reporting account. Deleting it deletes the reports it filed.
    reporter_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The reported account. Deleting it takes the reports filed against it.
    target_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- What the reporter wrote; empty when they said nothing.
    comment text NOT NULL DEFAULT '',
    -- Which rule the reporter picked (`src/models/report.rs`, `Category`).
    -- The API validates the value; the check makes the set the column's too.
    category text NOT NULL DEFAULT 'other'
        CONSTRAINT reports_category_check
        CHECK (category IN ('spam', 'legal', 'violation', 'other')),
    -- Whether the reporter asked for the report to be forwarded to the
    -- target's own instance (Mastodon's `forward`).
    forwarded boolean NOT NULL DEFAULT false,
    -- Moderation lifecycle (`src/models/report.rs`, `ReportState`): a fresh
    -- report is `open`; the admin tickets move it to `closed` or `resolved`.
    state text NOT NULL DEFAULT 'open'
        CONSTRAINT reports_state_check
        CHECK (state IN ('open', 'closed', 'resolved')),
    -- The moderator this report is assigned to; `NULL` is unassigned.
    -- Deliberately no `ON DELETE` rule (the ticket's column): an account
    -- that still carries an assignment cannot be deleted out from under it,
    -- which is the admin ticket that removes accounts' job to untangle.
    assigned_to uuid REFERENCES users(id),
    -- When a moderator acted on the report; `NULL` while none has. The
    -- entity's `action_taken` is this column read as a boolean, so there is
    -- no second flag to keep in step with it.
    action_taken_at timestamptz,
    -- The instance rules the reporter cited, in the order they cited them.
    rule_ids uuid[] NOT NULL DEFAULT '{}',
    -- ActivityPub id of the Flag activity behind this report (decision 6):
    -- a local reporter mints {BASE_URL}/activities/{uuid}, a remote one
    -- keeps the id its own instance sent; NULL until a Flag exists.
    ap_id text UNIQUE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- The reports one account filed, and the queue of reports against one
-- account (the target's own admin view, and the later "already reported?"
-- questions).
CREATE INDEX reports_reporter_id_idx ON reports (reporter_id);
CREATE INDEX reports_target_id_idx ON reports (target_id);
-- The moderation queue, which is always read per state.
CREATE INDEX reports_state_idx ON reports (state);

-- The statuses a report cites, for context. The pair is the key, so
-- citing one status twice is one row, and a deleted status leaves the
-- report — whose entity then simply does not name it any more.
CREATE TABLE report_statuses (
    -- The report. Deleting it takes its status context with it.
    report_id uuid NOT NULL REFERENCES reports(id) ON DELETE CASCADE,
    -- The cited status. Deleting the post drops it from every report.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    PRIMARY KEY (report_id, status_id)
);

-- Which reports cite one status (the reverse lookup a moderation view of a
-- post needs; the primary key only leads with the report).
CREATE INDEX report_statuses_status_id_idx ON report_statuses (status_id);

-- Moderators' notes on a report, never the reporter's (`comment` is that).
-- Nothing writes this table yet: the admin ticket that adds the notes
-- endpoint owns `src/models/report.rs`'s counterpart.
CREATE TABLE report_notes (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The report the note belongs to; deleting it deletes its notes.
    report_id uuid REFERENCES reports(id) ON DELETE CASCADE,
    -- The moderator who wrote it; `NULL` is no author. No `ON DELETE` rule
    -- (the ticket's column), so a note keeps naming an account that cannot
    -- be deleted while it exists.
    user_id uuid REFERENCES users(id),
    -- The note's text.
    content text NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- The notes of one report, oldest first.
CREATE INDEX report_notes_report_id_idx ON report_notes (report_id);

-- The ticket also asks for the foreign key `notifications.report_id ->
-- reports(id) ON DELETE CASCADE`. There is no `notifications` table in this
-- schema yet — it is a later ticket — so that key cannot be added here
-- without inventing the table; the migration that creates `notifications`
-- declares `report_id uuid REFERENCES reports(id) ON DELETE CASCADE` as part
-- of its own columns, which is the same constraint on the same two tables.
