-- Which instance a custom reaction's emoji belongs to: the `domain` of its
-- `custom_emojis` row, NULL for a unicode reaction and for a local emoji.
-- Pleroma names a remote custom reaction `shortcode@domain`, and two
-- reactions of one shortcode from different instances are different emojis.
ALTER TABLE emoji_reactions ADD COLUMN emoji_domain text;

-- Reactions mirrored before this column existed carry the image of an emoji
-- of their reactor's instance, which is the only origin they ever recorded.
UPDATE emoji_reactions r
   SET emoji_domain = u.domain
  FROM users u
 WHERE u.id = r.user_id
   AND u.domain IS NOT NULL
   AND r.url IS NOT NULL;

-- One reaction per account, status and emoji now includes the emoji's origin,
-- so a local `:cat:` and a remote `:cat:` are two reactions. A NULL domain
-- compares as the empty string, which no domain is.
ALTER TABLE emoji_reactions
    DROP CONSTRAINT emoji_reactions_user_id_status_id_emoji_key;
CREATE UNIQUE INDEX emoji_reactions_user_status_emoji_key
    ON emoji_reactions (user_id, status_id, emoji, (COALESCE(emoji_domain, '')));
