//! The redacting [`Debug`] impl of [`Config`]: the
//! whole field list in declaration order, with `database_url`,
//! `smtp_password` and the `secret_key_base` behind `hide_secret`
//! standing in as `<redacted>`.

use std::fmt;

use super::Config;

/// Redacts `database_url`, which usually contains a password.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The whole field list does not fit one function inside Clippy's
        // line budget, so two segment helpers write it in declaration
        // order and `fmt` only opens, hands over and closes the document.
        let mut debug = f.debug_struct("Config");
        let debug = debug_identity_and_exposure(&mut debug, self);
        debug_delivery_and_federation(debug, self).finish()
    }
}

/// The `secret_key_base` rendered for `Debug`: the marker string in place
/// of the secret, `None` staying `None`. A function rather than the inline
/// closure the impl used to bind: the closure's text pushed the chain's
/// call past rustfmt's call width, splitting the field over lines the
/// [`Debug`] impl's line budget could not spend.
fn hide_secret(config: &Config) -> Option<&'static str> {
    config.secret_key_base.as_ref().map(|_| "<redacted>")
}

/// Writes the first segment of the [`Config`] `Debug` document: the
/// instance's identity, its content limits, storage locations, auth
/// requirements and everything it exposes to clients — every field from
/// `database_url` (redacted, it carries the password) to
/// `available_frontends`. The builder arrives borrowed and leaves borrowed,
/// so `fmt` can keep the chain going.
fn debug_identity_and_exposure<'a, 'b, 'd>(
    debug: &'d mut fmt::DebugStruct<'a, 'b>,
    config: &Config,
) -> &'d mut fmt::DebugStruct<'a, 'b> {
    debug
        .field("database_url", &"<redacted>")
        .field("bind_addr", &config.bind_addr)
        .field("base_url", &config.base_url)
        .field("frontend_base_url", &config.frontend_base_url)
        .field("instance_name", &config.instance_name)
        .field("instance_description", &config.instance_description)
        .field("contact_email", &config.contact_email)
        .field("instance_languages", &config.instance_languages)
        .field("max_status_chars", &config.max_status_chars)
        .field("max_media_attachments", &config.max_media_attachments)
        .field("max_poll_options", &config.max_poll_options)
        .field("max_poll_option_chars", &config.max_poll_option_chars)
        .field("min_poll_expiration_secs", &config.min_poll_expiration_secs)
        .field("max_poll_expiration_secs", &config.max_poll_expiration_secs)
        .field("max_chat_message_chars", &config.max_chat_message_chars)
        .field("max_media_description", &config.max_media_description)
        .field("max_display_name_chars", &config.max_display_name_chars)
        .field("max_bio_chars", &config.max_bio_chars)
        .field("max_profile_fields", &config.max_profile_fields)
        .field("max_report_comment", &config.max_report_comment)
        .field("max_image_pixels", &config.max_image_pixels)
        .field("max_pinned_statuses", &config.max_pinned_statuses)
        .field("min_expiration_secs", &config.min_expiration_secs)
        .field("min_scheduled_secs", &config.min_scheduled_secs)
        .field("max_scheduled_per_user", &config.max_scheduled_per_user)
        .field(
            "max_scheduled_per_user_per_day",
            &config.max_scheduled_per_user_per_day,
        )
        .field("upload_limits", &config.upload_limits)
        .field("avatar_limit_bytes", &config.avatar_limit_bytes)
        .field("banner_limit_bytes", &config.banner_limit_bytes)
        .field("inbox_max_bytes", &config.inbox_max_bytes)
        .field(
            "max_remote_mention_lookups",
            &config.max_remote_mention_lookups,
        )
        .field("uploads_dir", &config.uploads_dir)
        .field("emoji_dir", &config.emoji_dir)
        .field("statics", &config.statics)
        .field("media_base_url", &config.media_base_url)
        .field("accounts_email_optional", &config.accounts_email_optional)
        .field("min_password_length", &config.min_password_length)
        .field("oauth_token_ttl_secs", &config.oauth_token_ttl_secs)
        .field("password_reset_ttl_secs", &config.password_reset_ttl_secs)
        .field(
            "account_activation_required",
            &config.account_activation_required,
        )
        .field("allow_account_deletion", &config.allow_account_deletion)
        .field("mongooseim_enabled", &config.mongooseim_enabled)
        .field("captcha_enabled", &config.captcha_enabled)
        .field("healthcheck_enabled", &config.healthcheck_enabled)
        .field("registrations_open", &config.registrations_open)
        .field("invites_enabled", &config.invites_enabled)
        .field(
            "registrations_approval_required",
            &config.registrations_approval_required,
        )
        .field(
            "search_statuses_unauthenticated",
            &config.search_statuses_unauthenticated,
        )
        .field("restrict_unauthenticated", &config.restrict_unauthenticated)
        .field("oauth_providers", &config.oauth_providers)
        .field("available_frontends", &config.available_frontends)
}

/// Writes the second segment of the [`Config`] `Debug` document: the job
/// worker and its sweeps — the deleted-status purge age and the backup
/// trio — the streaming heartbeat, the mailer (whose `smtp_password` and
/// [`hide_secret`]'s `secret_key_base` are the impl's redactions beside
/// `database_url`), the media, translation and fetch services, and the
/// federation settings with the unreachable-host grace period, the MRF
/// chain and the rate limiter. The builder arrives borrowed and leaves
/// borrowed, so `fmt` can close the document it opened.
fn debug_delivery_and_federation<'a, 'b, 'd>(
    debug: &'d mut fmt::DebugStruct<'a, 'b>,
    config: &Config,
) -> &'d mut fmt::DebugStruct<'a, 'b> {
    debug
        .field("job_concurrency", &config.job_concurrency)
        .field("job_queues", &config.job_queues)
        .field(
            "purge_deleted_statuses_days",
            &config.purge_deleted_statuses_days,
        )
        .field("backup_validity_days", &config.backup_validity_days)
        .field("backup_limit_days", &config.backup_limit_days)
        .field("backup_purge", &config.backup_purge)
        .field(
            "streaming_ping_interval_secs",
            &config.streaming_ping_interval_secs,
        )
        .field("smtp_host", &config.smtp_host)
        .field("smtp_port", &config.smtp_port)
        .field("smtp_username", &config.smtp_username)
        .field("smtp_password", &"<redacted>")
        .field("smtp_tls", &config.smtp_tls)
        .field("mail_from", &config.mail_from)
        .field("secret_key_base", &hide_secret(config))
        .field("media_proxy", &config.media_proxy)
        .field("rich_media_ignore_hosts", &config.rich_media_ignore_hosts)
        .field("rich_media_remote", &config.rich_media_remote)
        .field("vapid", &config.vapid)
        .field("translation", &config.translation)
        .field("fetch", &config.fetch)
        .field("federation_enabled", &config.federation_enabled)
        .field(
            "accept_unsolicited_public",
            &config.accept_unsolicited_public,
        )
        .field("unreachable_grace_secs", &config.unreachable_grace_secs)
        .field("mrf", &config.mrf)
        .field("rate_limit", &config.rate_limit)
}
