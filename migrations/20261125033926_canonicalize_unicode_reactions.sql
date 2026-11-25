-- Files every unicode reaction under its grouping key: the emoji with each
-- U+FE0F (variation selector-16) removed, unless it holds a zero-width joiner
-- (U+200D), in which case it is kept exactly. This is Misskey's own
-- normalization (`ReactionService.normalize`), and it makes `❤` and `❤️` one
-- reaction instead of two groups.
--
-- One account may hold both spellings on one status; the earliest row
-- survives and the other goes, so the unique index
-- `emoji_reactions_user_status_emoji_key` holds once the text is rewritten.
-- A custom `:shortcode:` never holds either code point, so its key is itself.
DELETE FROM emoji_reactions r
 USING (
       SELECT id,
              row_number() OVER (
                  PARTITION BY user_id, status_id, COALESCE(emoji_domain, ''),
                               CASE WHEN strpos(emoji, chr(8205)) = 0
                                    THEN replace(emoji, chr(65039), '')
                                    ELSE emoji
                               END
                  ORDER BY inserted_at, id
              ) AS rank
         FROM emoji_reactions
       ) d
 WHERE r.id = d.id
   AND d.rank > 1;

UPDATE emoji_reactions
   SET emoji = replace(emoji, chr(65039), '')
 WHERE strpos(emoji, chr(65039)) > 0
   AND strpos(emoji, chr(8205)) = 0;
