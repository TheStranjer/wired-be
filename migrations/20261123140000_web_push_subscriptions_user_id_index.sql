-- Index the per-user Web Push lookup (WBE-512, CORE-9).
--
-- `FOR_USER_SQL` in `src/models/web_push_subscription.rs` runs once per
-- notification that may push: `WHERE user_id = $1 ORDER BY id`. Only the
-- unique `token_id` index existed, so each lookup was a sequential scan that
-- grew with users x devices. `(user_id, id)` serves the filter and the order.
CREATE INDEX web_push_subscriptions_user_id_idx ON web_push_subscriptions (user_id, id);
