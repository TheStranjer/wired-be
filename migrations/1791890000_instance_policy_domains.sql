-- Instance policy domains (WBE-126): the domains this instance rejects or
-- silences by instance policy, the data behind
-- `GET /api/v1/instance/domain_blocks`. Reference: Pleroma keeps these in
-- its `:mrf_simple` runtime config (`lib/pleroma/web/mastodon_api/views/instance_view.ex`
-- `render("domain_blocks.json")`); here each blocked domain is its own row,
-- which is what lets the (later) admin tickets edit the list without
-- rewriting a config blob. `src/models/instance_policy_domain.rs` owns the
-- queries.
CREATE TABLE instance_policy_domains (
    -- The blocked host; the primary key keeps one policy per domain.
    domain text PRIMARY KEY,
    -- What the policy does: reject the domain's traffic, keep it visible
    -- but unfederated (silence), or strip its media (media_removal). The
    -- endpoint maps these to Mastodon severities (suspend, silence).
    action text NOT NULL CHECK (action IN ('reject', 'silence', 'media_removal')),
    -- Why the domain is blocked, shown as the entry's comment; empty when
    -- the instance gave no reason.
    reason text NOT NULL DEFAULT '',
    inserted_at timestamptz NOT NULL DEFAULT now()
);

-- The only read is the whole listing, ordered by domain; the primary key's
-- index already serves it.
