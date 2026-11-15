-- Media proxy cache URL ledger (WBE-208): every remote URL the `/proxy`
-- route has served, newest write winning. Reference: Pleroma's media-proxy
-- cache table (`Pleroma.Web.MediaProxy`'s `:banned_urls_cache`, read and
-- cleared by `lib/pleroma/web/admin_api/controllers/media_proxy_cache_controller.ex`);
-- here it is a durable table instead of a Cachex one.
-- `src/models/media_proxy_cache.rs` owns the queries and
-- `src/api/admin/ops.rs` the admin list/delete/purge routes.
CREATE TABLE media_proxy_cache_urls (
    -- The served URL itself is the key: recording re-serves as an upsert,
    -- and the admin delete/purge remove by it.
    url text PRIMARY KEY,
    -- When this URL was last served: the prune to
    -- `MEDIA_PROXY_CACHE_LIMIT` rows removes the oldest of these first.
    inserted_at timestamptz NOT NULL DEFAULT now()
);
