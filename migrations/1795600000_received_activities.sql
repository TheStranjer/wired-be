-- Received-activity dedupe: the ledger behind the `federator.incoming`
-- dispatcher (src/federation/dispatch.rs), Pleroma's
-- `Pleroma.Web.ActivityPub.Utils.create_activity` id-uniqueness
-- (lib/pleroma/web/activity_pub/utils.ex) reduced to exactly the fact the
-- dispatcher asks: have we already acted on this `ActivityPub` id. One row
-- per processed activity; the dispatcher's `INSERT … ON CONFLICT DO
-- NOTHING` is the claim, and the daily
-- `federation.cleanup_received` job prunes rows past their redelivery
-- horizon.
CREATE TABLE received_activities (
    -- The activity's `ActivityPub` id, verbatim (decision 6): the primary
    -- key is the uniqueness the dispatcher deduplicates on.
    ap_id text PRIMARY KEY,
    -- When this instance first saw the activity; the prune job's clock.
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- The daily prune walks the age; without this it would scan the whole
-- ledger for the handful of rows past two weeks.
CREATE INDEX received_activities_inserted_at_idx ON received_activities (inserted_at);
