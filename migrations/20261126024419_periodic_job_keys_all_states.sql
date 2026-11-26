-- One row per period for the scheduler's `periodic:<kind>:<bucket>` keys,
-- whatever state the row is in. The live-only index `jobs_unique_key_live_idx`
-- stops covering a key once its job completes, so the next 10 s tick enqueued
-- the same bucket again. This index covers every state; the scheduler's
-- `ON CONFLICT DO NOTHING` insert rides on it, so concurrent app instances
-- still produce exactly one row per period.

-- Collapse the duplicates the bug already wrote, keeping the live row when
-- there is one and otherwise the newest.
DELETE FROM jobs
WHERE id IN (
    SELECT id
    FROM (
        SELECT id,
               row_number() OVER (
                   PARTITION BY unique_key
                   ORDER BY (state IN ('available', 'executing', 'retryable')) DESC,
                            inserted_at DESC,
                            id DESC
               ) AS keep_rank
        FROM jobs
        WHERE unique_key LIKE 'periodic:%'
    ) ranked
    WHERE keep_rank > 1
);

CREATE UNIQUE INDEX jobs_periodic_unique_key_idx
    ON jobs (unique_key)
    WHERE unique_key LIKE 'periodic:%';
