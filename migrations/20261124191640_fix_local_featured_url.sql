-- Local actors advertise `{ap_id}/collections/featured`, and that is the only
-- featured collection the router serves. Rows written before the fix carry
-- `{ap_id}/featured` (a 404) or nothing, so a pin's `Add`/`Remove` named a
-- target no peer accepts. Remote rows are the peers' own words: left alone.
UPDATE users
SET featured_url = ap_id || '/collections/featured'
WHERE local
  AND (featured_url IS NULL OR featured_url = ap_id || '/featured');
