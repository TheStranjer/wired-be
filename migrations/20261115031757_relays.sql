-- Relays (WBE-186): the relay actors THIS instance follows, one row per
-- relay. Reference: Pleroma's `Pleroma.Web.ActivityPub.Relay`
-- (`lib/pleroma/web/activity_pub/relay.ex`), whose `follow/1` sends a
-- `Follow` of the relay actor from the instance actor and whose announce
-- handling consults the stored subscriptions. `src/models/relay.rs` owns
-- the queries; `src/services/relay.rs` runs the handshake, and
-- `src/federation/publisher.rs` fans public activities out to the
-- `inbox` of every `accepted` relay (`relay_inboxes`).
CREATE TABLE relays (
    -- The relay actor's ActivityPub id: the `object` of the `Follow` we
    -- sent and the `actor` of the `Accept` and the `Announce`es we fold.
    -- It is the primary key, so re-following a relay restates its row
    -- instead of clashing.
    actor_ap_id text PRIMARY KEY,
    -- The relay's inbox URL: where the `Follow` (and its `Undo`) went,
    -- and what `relay_inboxes` hands the publisher.
    inbox text NOT NULL,
    -- Lifecycle state (see src/models/relay.rs): the row starts
    -- 'pending' and the accepted arm opens only when the relay's own
    -- `Accept` folds.
    state text NOT NULL DEFAULT 'pending'
        CONSTRAINT relays_state_check
        CHECK (state IN ('pending', 'accepted')),
    -- When the follow was sent.
    inserted_at timestamptz NOT NULL DEFAULT now()
);
