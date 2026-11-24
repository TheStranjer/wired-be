-- Quote requests (WBE-781, FEP-044f).
--
-- One row is one `QuoteRequest` this instance sent: the local status
-- `quoting_status_id` quotes the remote status `quoted_status_id`, and its
-- author `quoted_user_id` has to consent. The author answers with an
-- `Accept` whose `result` is the `QuoteAuthorization` address (state
-- `accepted`, the address kept in `authorization_uri`), with a `Reject`
-- (`rejected`), or later withdraws the authorization with a `Delete`
-- (`revoked`). Until one of those arrives the request is `pending`.
CREATE TABLE quote_requests (
    -- UUIDv7 made in Rust (decision 4); also names the request activity,
    -- `{BASE_URL}/activities/{id}`.
    id uuid PRIMARY KEY,
    -- The local status that quotes. A deleted status takes its request
    -- with it.
    quoting_status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The remote status being quoted.
    quoted_status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    -- The quoted status' author: the only account whose answer counts.
    quoted_user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The `QuoteRequest` activity id, as sent and as the answer embeds it.
    request_ap_id text NOT NULL,
    state text NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'accepted', 'rejected', 'revoked')),
    -- The `QuoteAuthorization` address the author granted; NULL until then.
    authorization_uri text,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- One request per quoting status: a retried hook restates it.
CREATE UNIQUE INDEX quote_requests_quoting_status_id_key
    ON quote_requests (quoting_status_id);

-- An answer finds its request by the activity id it embeds.
CREATE UNIQUE INDEX quote_requests_request_ap_id_key
    ON quote_requests (request_ap_id);

-- A `Delete` finds its request by the authorization it withdraws.
CREATE INDEX quote_requests_authorization_uri_idx
    ON quote_requests (authorization_uri)
    WHERE authorization_uri IS NOT NULL;

CREATE INDEX quote_requests_quoted_status_id_idx
    ON quote_requests (quoted_status_id);

CREATE INDEX quote_requests_quoted_user_id_idx
    ON quote_requests (quoted_user_id);
