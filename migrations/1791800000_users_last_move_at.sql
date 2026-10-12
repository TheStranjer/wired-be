-- Account-move rate limit (`POST /api/pleroma/move_account`): the last
-- time this account moved, so the endpoint can enforce Pleroma's
-- once-per-30-days cooldown. NULL means the account has never moved
-- (docs/ARCHITECTURE.md decision 1: the HTTP behaviour must match).
ALTER TABLE users ADD COLUMN last_move_at timestamptz;
