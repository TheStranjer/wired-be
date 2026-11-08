-- Web Push subscriptions (WBE-144): one row per access token holding the
-- browser PushSubscription the later delivery ticket sends notifications to.
-- Reference: Pleroma's `Pleroma.Web.Push.Subscription`
-- (`lib/pleroma/web/push/subscription.ex`) and its `push_subscriptions`
-- table; the Pleroma schema's `data` jsonb (whose only key ever written is
-- `alerts`) is this table's `alerts` and `policy` columns, and Pleroma's
-- `(user_id, token_id)` lookup is served here by the unique `token_id`:
-- every access token has at most one subscription, and creating a second
-- one replaces the first (the API's upsert). Deleting the account or the
-- token that owns a subscription takes it down with them.
CREATE TABLE web_push_subscriptions (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The owning account, kept beside the token's for cascade cleanup.
    user_id uuid REFERENCES users ON DELETE CASCADE,
    -- The access token this subscription belongs to; unique, so one token
    -- carries one subscription and the API's create can replace in place.
    token_id uuid NOT NULL UNIQUE REFERENCES oauth_tokens ON DELETE CASCADE,
    -- The browser callback URL notifications are pushed to.
    endpoint text NOT NULL,
    -- Client ECDH public key (P-256), base64url, as the PushSubscription
    -- hands it over.
    key_p256dh text NOT NULL,
    -- Client auth secret, base64url.
    key_auth text NOT NULL,
    -- Which notification types wake this subscription, as a jsonb object of
    -- `{"<alert type>": <bool>}`; the API fills in every known type, the
    -- column default keeps a hand-written row readable.
    alerts jsonb NOT NULL DEFAULT '{}',
    -- Who may wake this subscription: `all`, `followed`, `follower` or
    -- `none` (Mastodon's push policy); `all` unless the client narrows it.
    policy text NOT NULL DEFAULT 'all',
    -- Row timestamps; the model restamps `updated_at` on every write.
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
