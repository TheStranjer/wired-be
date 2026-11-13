-- Instances (WBE-164): one row per remote host this instance has heard of,
-- and the federation facts it accumulates about it. Reference: Pleroma's
-- `Pleroma.Instance` (`lib/pleroma/instance.ex`), whose `unreachable_since`
-- timestamp `Publisher.publish` consults through `Instances.filter_reachable/1`
-- before addressing a host, and whose `favicon`/`metadata` columns the
-- node-metadata fetcher fills. `src/models/instance.rs` owns the queries;
-- the reachability gate is `UNREACHABLE_GRACE_SECS` (see
-- `Config::unreachable_grace_secs`).
CREATE TABLE instances (
    -- The remote host: lowercase, no scheme and no port — exactly what a URL's
    -- host part gives, which is how the publisher derives it. The primary key
    -- is what makes a first-sight `INSERT` an upsert instead of a clash.
    host text PRIMARY KEY,
    -- When this instance first failed to reach the host, and the reason a
    -- delivery to it is skipped; NULL while the host is reachable or once it
    -- has recovered. `is_host_unreachable` compares it against now minus the
    -- grace period, so recovering a host means clearing this column.
    unreachable_since timestamptz,
    -- The host's favicon URL and when it was last fetched. Schema-only in
    -- this ticket series: no favicon fetching is specified yet, so nothing
    -- writes them (ARCHITECTURE.md decision 15).
    favicon text,
    favicon_updated_at timestamptz,
    -- When the host's node metadata was last fetched. Schema-only for the
    -- same reason.
    metadata_updated_at timestamptz,
    -- First sight of the host; `updated_at` is what every write here bumps.
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
