//! The `metadata` block of the `NodeInfo` document: Pleroma's extensions
//! and the limit and markup blocks they carry.

use serde::Serialize;
use serde_json::{Map, Value};

use super::{
    ACCOUNT_FIELD_NAME_LENGTH, ACCOUNT_FIELD_VALUE_LENGTH, AVATAR_UPLOAD_LIMIT,
    BACKGROUND_UPLOAD_LIMIT, BANNER_UPLOAD_LIMIT, MAX_ACCOUNT_FIELDS, MAX_POLL_EXPIRATION_SECS,
    MAX_POLL_OPTION_CHARS, MAX_REMOTE_ACCOUNT_FIELDS, MIN_POLL_EXPIRATION_SECS, POST_FORMATS,
};
use crate::{
    config::{Config, Toggle},
    federation::mrf,
    services::instance_meta,
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
    /// Profile-field limits.
    fields_limits: FieldsLimits,
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
    /// The markup allowances behind the scrubbing policy ([`crate::util::sanitize`]).
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
            fields_limits: FieldsLimits::pleroma_defaults(),
            account_activation_required: config.account_activation_required,
            invites_enabled: config.invites_enabled,
            mailer_enabled: Toggle::new(false),
            features: instance_meta::features(),
            restrict_unauthenticated: instance_meta::restrict_unauthenticated(config),
            skip_thread_containment: Toggle::new(true),
            markup: Markup::pleroma_defaults(),
        }
    }
}

/// The `metadata.suggestions` block.
#[derive(Debug, Serialize)]
struct Suggestions {
    /// Whether the suggested-accounts endpoints serve anything.
    enabled: Toggle,
}

/// The `metadata.federation` block.
#[derive(Debug, Serialize)]
pub(super) struct Federation {
    /// Whether this instance federates.
    enabled: bool,
    /// The moderation filters in force, Pleroma-style names in chain
    /// order (Pleroma's `mrf, transparency` answer lists its policy
    /// modules; this server reports the [MRF
    /// chain](crate::federation::mrf)'s own names) — empty when
    /// transparency is off, which publishes no policy at all.
    mrf_policies: Vec<String>,
    /// Instances whose posts are held back; none is configured. Absent
    /// from the transparency-off answer, which keeps only the three keys
    /// the switch leaves standing.
    #[serde(skip_serializing_if = "Option::is_none")]
    quarantined_instances: Option<Vec<&'static str>>,
    /// Whether the published lists hold back any host
    /// (`MRF_TRANSPARENCY_EXCLUSIONS` in use, Pleroma deriving the same
    /// flag from its `[:mrf, :transparency_exclusions]`); with
    /// transparency off, false.
    exclusions: bool,
    /// Each policy's transparency blocks under the keys they name
    /// themselves — `mrf_simple`, `mrf_simple_info`, `mrf_<name>` —
    /// flattened into this object beside the fixed keys.
    #[serde(flatten)]
    mrf_descriptions: Map<String, Value>,
}

impl Federation {
    /// A plain federating instance with no moderation filters and
    /// transparency on: what an empty chain publishes, which is the answer
    /// [`federation_block`] collapses to when nothing is configured. Test
    /// support for the document builders; production always reads the live
    /// chain and the live settings.
    #[cfg(test)]
    pub(super) fn federating() -> Self {
        let published = mrf::ChainTransparency {
            published: true,
            ..mrf::ChainTransparency::default()
        };
        Self::with_mrf(&published)
    }

    /// The block of an instance whose chain published `described` (see
    /// [`federation_block`]).
    pub(super) fn with_mrf(described: &mrf::ChainTransparency) -> Self {
        Self {
            enabled: true,
            mrf_policies: described.policies.clone(),
            quarantined_instances: described.quarantined_instances(),
            exclusions: described.exclusions,
            mrf_descriptions: described.descriptions.clone(),
        }
    }
}

/// The `metadata.pollLimits` block; its keys keep Pleroma's `snake_case`
/// config names, which is what a `NodeInfo` consumer of this block reads.
#[derive(Debug, Serialize)]
struct PollLimits {
    /// Options one poll may offer (`MAX_POLL_OPTIONS`).
    max_options: usize,
    /// Characters one option may hold.
    max_option_chars: usize,
    /// Shortest poll duration, in seconds.
    min_expiration: usize,
    /// Longest poll duration, in seconds.
    max_expiration: usize,
}

impl PollLimits {
    /// The limits this instance enforces: its configured option count with
    /// Pleroma's shipped values around it.
    const fn for_config(config: &Config) -> Self {
        Self {
            max_options: config.max_poll_options,
            max_option_chars: MAX_POLL_OPTION_CHARS,
            min_expiration: MIN_POLL_EXPIRATION_SECS,
            max_expiration: MAX_POLL_EXPIRATION_SECS,
        }
    }
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

/// The `metadata.fieldsLimits` block (camelCase keys, as Pleroma renders
/// them from its `snake_case` config names).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldsLimits {
    /// Fields one local account may set.
    max_fields: usize,
    /// Fields a remote account may carry before they are dropped.
    max_remote_fields: usize,
    /// Characters a field name may hold.
    name_length: usize,
    /// Characters a field value may hold.
    value_length: usize,
}

impl FieldsLimits {
    /// Pleroma 2.10.2's shipped values; wired-be enforces no stricter
    /// profile-field settings.
    const fn pleroma_defaults() -> Self {
        Self {
            max_fields: MAX_ACCOUNT_FIELDS,
            max_remote_fields: MAX_REMOTE_ACCOUNT_FIELDS,
            name_length: ACCOUNT_FIELD_NAME_LENGTH,
            value_length: ACCOUNT_FIELD_VALUE_LENGTH,
        }
    }
}

/// The `metadata.markup` block: the formatting Pleroma's
/// `[:markup]` config allows by default, which [`crate::util::sanitize`]
/// implements to the same effect. The field names drop Pleroma's `allow_`
/// config prefix for Clippy's `struct_field_names`; `serde` restores it on
/// the wire, so consumers still read `allow_inline_images` and friends.
#[derive(Debug, Serialize)]
struct Markup {
    /// Whether a status may show remote images inline (Pleroma's default
    /// yes, because custom emoji are inline images).
    #[serde(rename = "allow_inline_images")]
    inline_images: Toggle,
    /// Whether heading markup survives; it does not.
    #[serde(rename = "allow_headings")]
    headings: Toggle,
    /// Whether tables survive; they do not.
    #[serde(rename = "allow_tables")]
    tables: Toggle,
}

impl Markup {
    /// Pleroma 2.10's shipped `[:markup]` defaults.
    const fn pleroma_defaults() -> Self {
        Self {
            inline_images: Toggle::new(true),
            headings: Toggle::new(false),
            tables: Toggle::new(false),
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
}
