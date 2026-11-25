-- The `interactionPolicy.canQuote` a remote Note advertised: a JSON object
-- `{"automatic": [...], "manual": [...]}` of the approver addresses, as the
-- document named them. NULL means the remote advertised no policy at all
-- (an empty `automatic` and `manual` means it advertised that nobody may
-- quote). Local statuses leave it NULL: their policy follows from their
-- visibility.
ALTER TABLE statuses ADD COLUMN quote_policy jsonb;
