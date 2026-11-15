-- Preferred frontend (WBE-122): the frontend this account picked through
-- `PUT /api/v1/pleroma/preferred_frontend` (Pleroma's
-- `FrontendSettingsController.update_preferred_frontend`, which only sets a
-- cookie; here the choice is also stored so it survives the browser). NULL
-- means the account never chose one (docs/ARCHITECTURE.md decision 1: the
-- HTTP behaviour must match, the storage is ours).
ALTER TABLE users ADD COLUMN preferred_frontend text;
