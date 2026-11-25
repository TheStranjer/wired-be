-- Who may quote a local status, as its author chose with
-- `quote_approval_policy` (`public`, `followers` or `nobody`). NULL means
-- the default: the policy the status's visibility allows. Remote statuses
-- leave it NULL; what they advertise lives in `quote_policy`.
ALTER TABLE statuses
    ADD COLUMN quote_approval_policy text
    CHECK (quote_approval_policy IN ('public', 'followers', 'nobody'));
