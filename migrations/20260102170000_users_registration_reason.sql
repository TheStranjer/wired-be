-- Registration reason (`POST /api/v1/accounts`): the free-text `reason`
-- an applicant submits when registrations require manual approval, kept
-- for the later approval flow. NULL means the account stated no reason
-- (Pleroma's `users.registration_reason`, `lib/pleroma/user.ex`).
ALTER TABLE users ADD COLUMN registration_reason text;
