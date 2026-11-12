-- Status `ap_context`: the ActivityPub thread collection a mirrored status
-- names in its `context` (or `conversation` / `ostatus:conversation`)
-- field (WBE-167, ingest remote Notes). Reference: Pleroma stores the same
-- fact inside the object JSON (`Pleroma.Object.data["context"]`, used by
-- `Context.conversation_id_from_object` to group replies of a thread whose
-- parent is not stored yet); here it is a column so the ingest service can
-- find the conversation a remote context already started without a JSONB
-- scan. Local statuses leave it NULL — their conversation is derived from
-- the parent row or their own id, and they own no foreign context.
ALTER TABLE statuses ADD COLUMN ap_context text;

-- "Which statuses already speak this context": the read the ingest reply
-- arm runs to inherit a conversation id. Only rows that carry one are
-- interesting, hence the partial index.
CREATE INDEX statuses_ap_context_idx ON statuses (ap_context)
    WHERE ap_context IS NOT NULL;
