-- The `uri` of the object a remote status quotes, kept while the quote is
-- unlinked (WBE-777).
--
-- Ingest used to link a quote only when the quoted status was already stored
-- and forgot the reference otherwise, so a quote of an unseen post stayed a
-- plain post for good. With the reference kept, a fetch job or the arrival of
-- the quoted status itself can set `quote_of_id` later. NULL for local
-- statuses, for statuses that quote nothing and for rows that predate the
-- column.
ALTER TABLE statuses ADD COLUMN quote_uri TEXT;

-- Only unlinked quotes are looked up by the uri they wait for, so the index
-- holds just those rows and drops a quote out once it is linked.
CREATE INDEX statuses_quote_uri_pending_idx
    ON statuses (quote_uri)
    WHERE quote_of_id IS NULL AND quote_uri IS NOT NULL;
