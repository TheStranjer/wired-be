-- External OAuth login states (docs/ARCHITECTURE.md decision 9): what
-- `GET /oauth/prepare_request` promises the provider round trip with, the
-- original `/oauth/authorize` parameters read back by the callback.
-- Reference: Pleroma hands the same fact to Ueberauth as a JSON-encoded
-- `state` parameter (prepare_request in
-- lib/pleroma/web/o_auth/o_auth_controller.ex); here it is stored server
-- side and the state is only an opaque key.
CREATE TABLE oauth_login_states (
    -- The opaque state this instance issues, a 43-character URL-safe random
    -- token made in src/util/token.rs. It is the primary key because it is
    -- the only thing that comes back.
    state text PRIMARY KEY,
    -- Provider name the round trip was prepared for; the callback refuses a
    -- state presented to a different provider.
    provider text NOT NULL,
    -- The original /oauth/authorize parameters (client_id, redirect_uri,
    -- scope and the client's own state), extended with the provider's
    -- account info under `pending` once an unlinked callback arrives.
    params jsonb NOT NULL,
    -- Expiry: ten minutes between prepare_request and the callback, the
    -- window src/models/oauth_login_state.rs pins.
    valid_until timestamptz NOT NULL
);
