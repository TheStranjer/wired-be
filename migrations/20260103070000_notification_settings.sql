-- Per-account notification settings (WBE-111): the flags Pleroma's
-- `PUT /api/pleroma/notification_settings` writes and `maybe_notify`
-- consults. Reference: Pleroma's `Pleroma.User.NotificationSetting`
-- embedded schema (`lib/pleroma/user/notification_setting.ex`), whose two
-- `false` defaults this column default mirrors; stored as jsonb so the
-- shape can grow without another migration.
ALTER TABLE users
    ADD COLUMN notification_settings jsonb NOT NULL DEFAULT '{"block_from_strangers": false, "hide_notification_contents": false}';
