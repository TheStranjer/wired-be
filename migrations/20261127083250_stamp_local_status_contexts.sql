-- Stamp every local status with the `context` it was published with
-- (WBE-886).
--
-- Until now a local status stored no `ap_context`: its note recomputed the
-- context on every render as the thread's oldest stored context (when the
-- root is a mirrored status), else this instance's minted
-- `{base}/contexts/{conversation_id}`. A remote reply that arrived later
-- with a context of its own (Pleroma names the root note, Mastodon mints
-- `/contexts/...`) then changed the context of local notes peers had
-- already stored, and `GET /contexts/{id}` started answering 404 for a
-- minted IRI this instance had already sent out. From now on
-- `status::insert` stamps the context once and the note renders the stored
-- value.
--
-- The backfill gives each unstamped local status the value it rendered
-- when it was published. The rows of a conversation are ordered by id
-- (UUIDv7, i.e. by arrival here), and no local row stored a context
-- before this migration, so the context a local status rendered at its
-- creation is the oldest context stored by a remote status of its
-- conversation *older than the local status itself*, if the conversation's
-- root row is a mirrored status; anything newer arrived after it was sent.
-- Where there is none — a local root, a remote root with no stored
-- context, or only contexts that arrived after the local status — it
-- rendered the minted IRI, which is stamped instead. This is the ticket's
-- rule: minted when the earliest stored remote context is newer than the
-- local status, the remote one otherwise.
--
-- The migration has no configuration, so the minted IRI's base is read off
-- the status's own object id, `{base}/objects/{id}` for every local
-- status. A row whose `uri` does not have that shape is left NULL; the note
-- of such a row keeps the old render-time fallback. Reblog rows render no
-- note and stay NULL, as `status::insert` leaves them.
UPDATE statuses s
   SET ap_context = COALESCE(
           (SELECT r.ap_context
              FROM statuses r
             WHERE r.conversation_id = s.conversation_id
               AND r.ap_context IS NOT NULL
               AND NOT r.local
               AND r.id < s.id
               AND EXISTS (SELECT 1 FROM statuses root
                            WHERE root.id = s.conversation_id AND NOT root.local)
          ORDER BY r.id
             LIMIT 1),
           left(s.uri, length(s.uri) - length('/objects/' || s.id::text))
               || '/contexts/' || s.conversation_id::text
       )
 WHERE s.local
   AND s.ap_context IS NULL
   AND s.reblog_of_id IS NULL
   AND s.uri LIKE ('%/objects/' || s.id::text);
