-- The accounts whose ballots a poll edit wiped.
--
-- An edit that changes a local poll's options or choice mode deletes every
-- vote in the same transaction that writes the edit, so by the time the
-- federated `Update` is fanned out the voters are no longer readable from
-- `poll_votes`. The edit copies them here first; the `Update`'s fan-out reads
-- them (with the live voters) to reach the remote servers that hold the old
-- poll, then clears the status' rows.
CREATE TABLE poll_departed_voters (
    -- The status whose poll was edited. Deleting it removes the rows.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The account whose ballot was wiped.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (status_id, user_id)
);
