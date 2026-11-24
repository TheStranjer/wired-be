-- Quote authorizations (WBE-780, FEP-044f).
--
-- One row is one grant: this instance's author let the remote account
-- `quoter_user_id` quote the local status `quoted_status_id` with the post at
-- `quoting_uri`. The row's id names the `QuoteAuthorization` document served
-- at `{BASE_URL}/quote_authorizations/{id}`, the address the `Accept` of the
-- `QuoteRequest` carries as its `result` and Mastodon fetches before it
-- shows the quote as accepted.
CREATE TABLE quote_authorizations (
    -- UUIDv7 made in Rust (decision 4).
    id uuid PRIMARY KEY,
    -- The local status being quoted. A hard-deleted status takes its grants
    -- with it; a soft-deleted one keeps them, and the route answers 410.
    quoted_status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The object id of the quoting post, as the quoter's instance names it.
    quoting_uri text NOT NULL,
    -- The remote account that asked, and whose post `quoting_uri` is.
    quoter_user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The `QuoteRequest` activity id that asked for the grant (the latest
    -- one, when the request is redelivered).
    request_ap_id text NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- Set when the grant is withdrawn; the document then answers 410.
    revoked_at timestamptz
);

-- One grant per quoted status and quoting post: a redelivered request
-- restates the grant instead of minting a second one.
CREATE UNIQUE INDEX quote_authorizations_quoted_status_id_quoting_uri_key
    ON quote_authorizations (quoted_status_id, quoting_uri);

CREATE INDEX quote_authorizations_quoter_user_id_idx
    ON quote_authorizations (quoter_user_id);
