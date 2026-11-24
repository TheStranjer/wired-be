//! The `metadata` block of the `NodeInfo` document: Pleroma's extensions
//! and the limit and markup blocks they carry. The federation, poll-limit,
//! field-limit and markup blocks are the ones the instance documents publish
//! too, defined in [`crate::services::instance_meta`].

use serde::Serialize;
use serde_json::Value;

use crate::{
    config::{Config, Toggle},
    services::instance_meta::{
        self, AVATAR_UPLOAD_LIMIT, BACKGROUND_UPLOAD_LIMIT, BANNER_UPLOAD_LIMIT, Federation,
        FieldsLimits, Markup, POST_FORMATS, PollLimits,
    },
};

/// The `metadata` block: Pleroma's extensions, in Pleroma's key order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Metadata {
    /// Instance display name (`INSTANCE_NAME`).
    node_name: String,
    /// Instance description (`INSTANCE_DESCRIPTION`).
    node_description: String,
    /// Whether registration alone does not get a user in; wired-be has no
    /// private-instance mode, so `false` (Pleroma's `public` default).
    private: Toggle,
    /// The account-suggestions feature; disabled, because it reports the
    /// `[:instance, :suggestions]` switch this server has no setting for
    /// (Pleroma answers its `false` default the same way, and the
    /// `is_suggested` pool is admin-curated, not an instance feature).
    suggestions: Suggestions,
    /// Actor ids of the admin and moderator accounts.
    staff_accounts: Vec<String>,
    /// The federation state block.
    federation: Federation,
    /// The poll limits this instance enforces.
    poll_limits: PollLimits,
    /// Source formats a status may be written in.
    post_formats: &'static [&'static str],
    /// Upload size limits, in bytes.
    upload_limits: UploadLimits,
    /// Profile-field limits: the shared [`FieldsLimits`] under `NodeInfo`'s
    /// camelCase key spelling ([`FieldsLimits::camel_case`]).
    fields_limits: Value,
    /// Whether a new account needs an email confirmation before it works
    /// (`ACCOUNT_ACTIVATION_REQUIRED`); Pleroma's
    /// `account_activation_required` mirror.
    account_activation_required: Toggle,
    /// Whether registration invites exist (`INVITES_ENABLED`); Pleroma's
    /// `invites_enabled` mirror.
    invites_enabled: Toggle,
    /// Whether the instance sends mail; no mailer exists, so `false`
    /// (Pleroma's `Pleroma.Emails.Mailer` `enabled` default).
    mailer_enabled: Toggle,
    /// Feature flags, shared with the instance endpoints'
    /// `pleroma.metadata.features` ([`instance_meta::features`]).
    features: &'static [&'static str],
    /// Which reads unauthenticated callers must not make, shared with the
    /// instance endpoints ([`instance_meta::restrict_unauthenticated`]).
    /// Snake-cased on purpose: Pleroma writes this config block out under
    /// its own atom names, not the `NodeInfo` schema's camelCase.
    #[serde(rename = "restrict_unauthenticated")]
    restrict_unauthenticated: Value,
    /// Whether threads from other instances keep only what addresses this
    /// one; Pleroma 2.10 ships `skip_thread_containment: true`.
    skip_thread_containment: Toggle,
    /// The markup allowances behind the scrubbing policy
    /// ([`crate::util::sanitize`]), the same block the instance documents
    /// publish.
    markup: Markup,
}

impl Metadata {
    /// The metadata block of the instance `config` serves, with
    /// `staff_accounts` as its staff actor ids and the given
    /// `federation` state.
    pub(super) fn build(
        config: &Config,
        staff_accounts: Vec<String>,
        federation: Federation,
    ) -> Self {
        Self {
            node_name: config.instance_name.clone(),
            node_description: config.instance_description.clone(),
            private: Toggle::new(false),
            suggestions: Suggestions {
                enabled: Toggle::new(false),
            },
            staff_accounts,
            federation,
            poll_limits: PollLimits::for_config(config),
            post_formats: &POST_FORMATS,
            upload_limits: UploadLimits::for_config(config),
            fields_limits: FieldsLimits::pleroma_defaults().camel_case(),
            account_activation_required: config.account_activation_required,
            invites_enabled: config.invites_enabled,
            mailer_enabled: Toggle::new(false),
            features: instance_meta::features(),
            restrict_unauthenticated: instance_meta::restrict_unauthenticated(config),
            skip_thread_containment: Toggle::new(true),
            markup: Markup::sanitizer_allowances(),
        }
    }
}

/// The `metadata.suggestions` block.
#[derive(Debug, Serialize)]
struct Suggestions {
    /// Whether the suggested-accounts endpoints serve anything.
    enabled: Toggle,
}

/// The `metadata.uploadLimits` block, in bytes.
#[derive(Debug, Serialize)]
struct UploadLimits {
    /// One attachment (`UPLOAD_LIMIT_BYTES`).
    general: usize,
    /// One avatar.
    avatar: usize,
    /// One banner.
    banner: usize,
    /// One profile background.
    background: usize,
}

impl UploadLimits {
    /// The configured general limit with Pleroma's shipped per-purpose
    /// defaults for the other three (wired-be has no separate settings for
    /// them).
    const fn for_config(config: &Config) -> Self {
        Self {
            general: config.upload_limits.max_file_bytes,
            avatar: AVATAR_UPLOAD_LIMIT,
            banner: BANNER_UPLOAD_LIMIT,
            background: BACKGROUND_UPLOAD_LIMIT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `invitesEnabled` is the `INVITES_ENABLED` setting, on both sides
    /// of its default.
    #[test]
    fn invites_enabled_mirrors_the_config() {
        let mut config = Config::for_tests();
        config.invites_enabled = Toggle::new(true);
        let on = serde_json::to_value(Metadata::build(
            &config,
            Vec::new(),
            Federation::federating(),
        ))
        .unwrap();
        assert_eq!(
            on["invitesEnabled"], true,
            "INVITES_ENABLED=true is reported"
        );
        config.invites_enabled = Toggle::new(false);
        let off = serde_json::to_value(Metadata::build(
            &config,
            Vec::new(),
            Federation::federating(),
        ))
        .unwrap();
        assert_eq!(off["invitesEnabled"], false, "and off stays off");
    }

    /// `accountActivationRequired` is the `ACCOUNT_ACTIVATION_REQUIRED`
    /// setting: Pleroma reads the same config key into this metadata field.
    #[test]
    fn account_activation_required_mirrors_the_config() {
        let mut config = Config::for_tests();
        config.account_activation_required = Toggle::new(true);
        let on = serde_json::to_value(Metadata::build(
            &config,
            Vec::new(),
            Federation::federating(),
        ))
        .unwrap();
        assert_eq!(
            on["accountActivationRequired"], true,
            "ACCOUNT_ACTIVATION_REQUIRED=true is reported"
        );
        config.account_activation_required = Toggle::new(false);
        let off = serde_json::to_value(Metadata::build(
            &config,
            Vec::new(),
            Federation::federating(),
        ))
        .unwrap();
        assert_eq!(off["accountActivationRequired"], false, "and off stays off");
    }

    /// Both documents publish the same markup and the same field limits.
    #[test]
    fn markup_and_field_limits_match_the_instance_documents() {
        let value = serde_json::to_value(Metadata::build(
            &Config::for_tests(),
            Vec::new(),
            Federation::federating(),
        ))
        .unwrap();
        assert_eq!(
            value["markup"],
            serde_json::to_value(Markup::sanitizer_allowances()).unwrap(),
            "the shared markup block"
        );
        assert_eq!(
            value["fieldsLimits"],
            FieldsLimits::pleroma_defaults().camel_case(),
            "the shared field limits"
        );
    }
}
