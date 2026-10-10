-- Background job queue (docs/ARCHITECTURE.md decision 11): a plain table
-- claimed with FOR UPDATE SKIP LOCKED, no Redis, no external queue.
CREATE TABLE jobs (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- Queue name workers subscribe to, e.g. 'default', 'mailers'.
    queue text NOT NULL,
    -- Handler key inside src/jobs/, e.g. 'federate'.
    kind text NOT NULL,
    -- Job arguments as JSON.
    args jsonb NOT NULL DEFAULT '{}',
    -- Lifecycle state (see src/models/job.rs).
    state text NOT NULL DEFAULT 'available'
        CONSTRAINT jobs_state_check
        CHECK (state IN ('available', 'executing', 'completed', 'retryable', 'discarded')),
    -- Claims increment this; fail() compares it with max_attempts.
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5,
    -- Earliest time the job may be claimed.
    run_at timestamptz NOT NULL DEFAULT now(),
    -- Set while a claim holds the job; NULL in every other state.
    locked_at timestamptz,
    last_error text,
    -- Jobs with a key that already has a live job (state below) are
    -- rejected by the partial unique index; enqueue turns that into None.
    unique_key text,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Claim scan: queue + state filter, ordered by run_at.
CREATE INDEX jobs_queue_state_run_at_idx ON jobs (queue, state, run_at);

-- Deduplication: one live job per unique_key; NULL keys never collide.
CREATE UNIQUE INDEX jobs_unique_key_live_idx
    ON jobs (unique_key)
    WHERE state IN ('available', 'executing', 'retryable');
