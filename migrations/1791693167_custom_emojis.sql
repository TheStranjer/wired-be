-- Custom emoji: shortcodes this instance serves as images, local and
-- mirrored from remote instances. Reference: Pleroma's custom emoji packs
-- (lib/pleroma/emoji/pack.ex) and Emoji module
-- (lib/pleroma/emoji.ex), flattened into one row per emoji
-- (docs/ARCHITECTURE.md decision 3), rendered by
-- lib/pleroma/web/mastodon_api/views/custom_emoji_view.ex.
CREATE TABLE custom_emojis (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- Shortcode without surrounding colons, e.g. `railroad_track`.
    shortcode text NOT NULL,
    -- Origin instance of a remote emoji; NULL for a local one.
    domain text,
    -- URL of the emoji image (animated included).
    image_url text NOT NULL,
    -- URL of the still (non-animated) variant; NULL means clients use
    -- `image_url` for both.
    static_url text,
    -- Pack name the emoji belongs to; NULL for an ungrouped one.
    category text,
    -- Picker tags, e.g. `{transport, trains}`. Empty array by default.
    tags text[] NOT NULL DEFAULT '{}',
    -- Whether clients show the emoji in their pickers.
    visible_in_picker boolean NOT NULL DEFAULT true,
    -- Moderation kill switch: disabled emojis are served to nobody.
    disabled boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- One row per shortcode and origin. `coalesce(domain, '')` folds the local
-- emoji (NULL domain) into the same key space, so `alice.example`'s `cat`
-- and the local `cat` coexist while a duplicate local `cat` cannot; this
-- is also the conflict target of the model's upsert.
CREATE UNIQUE INDEX custom_emojis_shortcode_domain_key
    ON custom_emojis (shortcode, coalesce(domain, ''));
