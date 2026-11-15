-- Rich media link cards: the cached preview card for one URL, and which
-- status renders which card — the storage behind the rich-media cards the
-- API attaches to statuses (see `crate::models::link_card`).
-- Reference: Pleroma's `Pleroma.Web.RichMediaCard`
-- (`lib/pleroma/web/rich_media/card.ex`, with the parsers under
-- `lib/pleroma/web/rich_media/`); its `rich_media_cards` table keyed by URL
-- is what `link_cards` is here, and the per-activity card association is
-- what `status_cards` is. docs/ARCHITECTURE.md decision 11 keeps this
-- instance's shared state — this cache included — in Postgres.
CREATE TABLE link_cards (
    -- The URL the card was fetched from, verbatim — the cache key: one row
    -- per URL, refreshed in place by `crate::models::link_card::store`.
    url text PRIMARY KEY,
    -- Whether the fetch produced a card ('ok') or failed ('failed'), so a
    -- dead URL is remembered instead of re-fetched on every render; the
    -- check constraint is the two states the cache knows.
    status text NOT NULL CHECK (status IN ('ok', 'failed')),
    -- The parsed card as JSON (title, image, favicon and the like), NULL
    -- for a failed fetch or a page that yielded no card at all.
    card jsonb,
    -- When the card was last fetched; every store restamps it, which is
    -- how a later expiry policy finds the stale rows.
    fetched_at timestamptz NOT NULL DEFAULT now()
);

-- The status → card links: one card under one status, pointing into the
-- `link_cards` cache. Deleting the status deletes its link, and a URL
-- leaving the cache un-links every status that pointed at it — both with
-- the cascade below.
CREATE TABLE status_cards (
    -- The status this card renders under. One card per status: the primary
    -- key the renderer's link write is idempotent on.
    status_id uuid PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    -- The cached card's URL. The foreign key rides `link_cards`' text
    -- primary key, so a link can never name a URL the cache does not hold.
    url text NOT NULL REFERENCES link_cards(url) ON DELETE CASCADE
);
