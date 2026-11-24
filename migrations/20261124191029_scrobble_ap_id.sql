-- Scrobbles federate as Pleroma-compatible `Listen` activities wrapping an
-- `Audio` object (WBE-763). `ap_id` is the `Listen`'s id,
-- `{BASE_URL}/activities/{id}`, minted when a local scrobble is inserted
-- (decision 6); the `Audio` object's id is `{BASE_URL}/objects/{id}`, which
-- needs no column. NULL for rows that predate federation, which were never
-- delivered and so name no activity.
ALTER TABLE scrobbles ADD COLUMN ap_id text UNIQUE;
