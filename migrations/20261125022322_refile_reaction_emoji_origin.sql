-- Re-files the custom reactions that were stored under their reactor's domain
-- instead of the emoji's own origin. A reaction that joins another instance's
-- emoji (or this instance's own) was filed as `shortcode@reactor-domain`, a
-- fake emoji of its own, so one emoji showed up as several reaction groups.
--
-- A row is mis-filed when its stored image URL sits on another host than its
-- `emoji_domain` and that host is a place the emoji demonstrably comes from:
-- either the URL is the image of a local, enabled emoji of the same shortcode
-- (the emoji is ours: `emoji_domain` becomes NULL), or the host is an
-- instance this server already knows through a user or an emoji row. An image
-- on an unknown host is left alone, since it may be a CDN of the reactor.
CREATE TEMP TABLE emoji_refile AS
SELECT r.id AS reaction_id,
       r.user_id,
       r.status_id,
       r.emoji,
       r.url,
       r.emoji_domain AS old_domain,
       r.inserted_at,
       CASE WHEN l.id IS NOT NULL THEN NULL ELSE h.host END AS new_domain
  FROM emoji_reactions r
 CROSS JOIN LATERAL (
       SELECT lower(substring(r.url FROM '^https?://(?:[^/?#@]*@)?([^/?#]+)')) AS host
 ) h
  LEFT JOIN custom_emojis l
    ON l.domain IS NULL
   AND NOT l.disabled
   AND ':' || l.shortcode || ':' = r.emoji
   AND l.image_url = r.url
 WHERE r.emoji_domain IS NOT NULL
   AND r.url IS NOT NULL
   AND h.host IS NOT NULL
   AND h.host <> lower(r.emoji_domain)
   AND (
        l.id IS NOT NULL
        OR EXISTS (SELECT 1 FROM users u WHERE lower(u.domain) = h.host)
        OR EXISTS (SELECT 1 FROM custom_emojis c WHERE lower(c.domain) = h.host)
   );

-- The emoji rows the moved reactions now belong to. An emoji row that
-- stands already keeps its image.
INSERT INTO custom_emojis (id, shortcode, domain, image_url)
SELECT DISTINCT ON (btrim(f.emoji, ':'), f.new_domain)
       gen_random_uuid(), btrim(f.emoji, ':'), f.new_domain, f.url
  FROM emoji_refile f
 WHERE f.new_domain IS NOT NULL
 ORDER BY btrim(f.emoji, ':'), f.new_domain, f.inserted_at, f.reaction_id
ON CONFLICT (shortcode, (coalesce(domain, ''))) DO NOTHING;

-- Two moved rows of one account may land on one key, and a moved row may land
-- on a reaction that stands there already; the earliest row survives and the
-- others go, so the unique key `(user_id, status_id, emoji, domain)` holds.
DELETE FROM emoji_reactions r
 USING emoji_refile f
 WHERE r.id = f.reaction_id
   AND (
        EXISTS (
            SELECT 1
              FROM emoji_reactions e
             WHERE e.user_id = f.user_id
               AND e.status_id = f.status_id
               AND e.emoji = f.emoji
               AND coalesce(e.emoji_domain, '') = coalesce(f.new_domain, '')
               AND e.id <> f.reaction_id
               AND e.id NOT IN (SELECT reaction_id FROM emoji_refile)
        )
        OR EXISTS (
            SELECT 1
              FROM emoji_refile g
             WHERE g.user_id = f.user_id
               AND g.status_id = f.status_id
               AND g.emoji = f.emoji
               AND coalesce(g.new_domain, '') = coalesce(f.new_domain, '')
               AND (g.inserted_at, g.reaction_id) < (f.inserted_at, f.reaction_id)
        )
   );

UPDATE emoji_reactions r
   SET emoji_domain = f.new_domain
  FROM emoji_refile f
 WHERE r.id = f.reaction_id;

-- The fake emoji rows those reactions created: the reactor's domain carrying
-- an image of somebody else's. They go once no reaction names them.
DELETE FROM custom_emojis c
 USING (SELECT DISTINCT btrim(emoji, ':') AS shortcode, old_domain, url
          FROM emoji_refile) f
 WHERE c.shortcode = f.shortcode
   AND c.domain = f.old_domain
   AND c.image_url = f.url
   AND NOT EXISTS (
       SELECT 1
         FROM emoji_reactions r
        WHERE r.emoji = ':' || c.shortcode || ':'
          AND r.emoji_domain = c.domain
   );

DROP TABLE emoji_refile;
