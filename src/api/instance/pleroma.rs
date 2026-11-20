//! The `pleroma` extension block both instance documents carry: the
//! capability metadata, the federation state, the profile-field limits and
//! the markup allowances.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    config::{Config, Toggle},
    federation::mrf,
    services::instance_meta,
};

use super::{
    ACCOUNT_FIELD_NAME_LENGTH, ACCOUNT_FIELD_VALUE_LENGTH, MAX_ACCOUNT_FIELDS,
    MAX_REMOTE_ACCOUNT_FIELDS, MONTHLY_ACTIVE_USERS, POST_FORMATS,
};

/// The `pleroma` extension block: Pleroma's own view of the instance
/// (its `pleroma_configuration`, `instance_view.ex`, restricted to the
/// keys the ticket documents).
#[derive(Debug, Serialize)]
pub(super) struct PleromaBlock {
    /// The capability metadata block.
    metadata: PleromaMetadata,
    /// The extra counters Pleroma reports here.
    stats: PleromaStats,
    /// The instance VAPID public key Web Push clients need, the key
    /// Pleroma answers from its own `:web_push_encryption` config.
    vapid_public_key: String,
}

impl PleromaBlock {
    /// The block of a plain public instance: the shared capability lists
    /// and `config`'s registration modes with the live `federation`
    /// state, the instance's `vapid_public_key`, and the zeroed extra
    /// counters.
    pub(super) fn build(config: &Config, vapid_public_key: &str, federation: Federation) -> Self {
        Self {
            metadata: PleromaMetadata::build(config, federation),
            stats: PleromaStats {
                mau: MONTHLY_ACTIVE_USERS,
            },
            vapid_public_key: vapid_public_key.to_owned(),
        }
    }
}

/// The `pleroma.stats` block.
#[derive(Debug, Serialize)]
struct PleromaStats {
    /// Monthly active users; wired-be keeps no activity counter yet, so
    /// a constant zero (the ticket's answer while the counters ticket is
    /// open).
    mau: usize,
}

/// The `pleroma.metadata` block: what this instance can do, in Pleroma's
/// key order.
#[derive(Debug, Serialize)]
struct PleromaMetadata {
    /// Whether a new account must confirm its email before it works
    /// (`ACCOUNT_ACTIVATION_REQUIRED`).
    account_activation_required: bool,
    /// Whether registration must state a birthday; it need not.
    birthday_required: bool,
    /// The age a birthday must show to register; 0 means no minimum.
    birthday_min_age: usize,
    /// Feature flags, shared with `NodeInfo` ([`instance_meta::features`]).
    features: &'static [&'static str],
    /// The federation state block.
    federation: Federation,
    /// Profile-field limits.
    fields_limits: FieldsLimits,
    /// Source formats a status may be written in.
    post_formats: &'static [&'static str],
    /// Which reads unauthenticated callers must not make, shared with
    /// `NodeInfo` ([`instance_meta::restrict_unauthenticated`]).
    restrict_unauthenticated: Value,
    /// The markup allowances behind the scrubbing policy
    /// ([`crate::util::sanitize`]).
    markup: Markup,
}

impl PleromaMetadata {
    /// The metadata of a plain public instance: the shared feature and
    /// access lists, no moderation filters, Pleroma's shipped limits,
    /// `config`'s activation mode, and the given `federation` state.
    fn build(config: &Config, federation: Federation) -> Self {
        Self {
            account_activation_required: config.account_activation_required.get(),
            birthday_required: false,
            birthday_min_age: 0,
            features: instance_meta::features(),
            federation,
            fields_limits: FieldsLimits::pleroma_defaults(),
            post_formats: &POST_FORMATS,
            restrict_unauthenticated: instance_meta::restrict_unauthenticated(),
            markup: Markup::denying(),
        }
    }
}

/// The `pleroma.metadata.federation` block — the same shape `NodeInfo`
/// reports: a federating instance beside whatever the live MRF chain
/// publishes under the operator's transparency settings.
#[derive(Debug, Serialize)]
pub(super) struct Federation {
    /// Whether this instance federates.
    enabled: bool,
    /// The moderation filters in force, Pleroma-style names in chain
    /// order (the [MRF](crate::federation::mrf) chain's
    /// [`describe`](crate::federation::mrf::describe)); empty when
    /// transparency is off, which publishes no policy at all.
    mrf_policies: Vec<String>,
    /// Instances whose posts are held back; none is configured. Absent
    /// from the transparency-off answer, which keeps only the three keys
    /// the switch leaves standing.
    #[serde(skip_serializing_if = "Option::is_none")]
    quarantined_instances: Option<Vec<&'static str>>,
    /// Whether the published lists hold back any host
    /// (`MRF_TRANSPARENCY_EXCLUSIONS` in use); with transparency off,
    /// false.
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
    /// `federation_block` collapses to when nothing is configured. Test
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
    /// `federation_block`).
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

/// The `pleroma.metadata.fields_limits` block; unlike `NodeInfo`'s
/// camelCase `fieldsLimits`, Pleroma writes this one under its own
/// `snake_case` config names.
#[derive(Debug, Serialize)]
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

/// The `pleroma.metadata.markup` block: the formatting this instance
/// allows, as the ticket documents it. The field names drop Pleroma's
/// `allow_` config prefix for Clippy's `struct_field_names`; `serde`
/// restores it on the wire, so consumers still read `allow_inline_images`
/// and friends.
#[derive(Debug, Serialize)]
struct Markup {
    /// Whether a status may show remote images inline; it may not.
    #[serde(rename = "allow_inline_images")]
    inline_images: Toggle,
    /// Whether heading markup survives; it does not.
    #[serde(rename = "allow_headings")]
    headings: Toggle,
    /// Whether tables survive; they do not.
    #[serde(rename = "allow_tables")]
    tables: Toggle,
    /// Whether font styling survives; it does not.
    #[serde(rename = "allow_fonts")]
    fonts: Toggle,
}

impl Markup {
    /// The markup this instance's sanitizer lets through per the ticket:
    /// none of the four optional allowances.
    const fn denying() -> Self {
        Self {
            inline_images: Toggle::new(false),
            headings: Toggle::new(false),
            tables: Toggle::new(false),
            fonts: Toggle::new(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::instance::test_support::{TEST_VAPID_KEY, empty_document};

    #[test]
    fn the_pleroma_block_holds_the_instance_metadata() {
        let document = empty_document();
        assert_eq!(document["pleroma"]["stats"], json!({"mau": 0}));
        assert_eq!(
            document["pleroma"]["vapid_public_key"],
            json!(TEST_VAPID_KEY),
            "the pleroma block advertises the instance VAPID key"
        );
        let metadata = &document["pleroma"]["metadata"];
        assert_eq!(metadata["account_activation_required"], false);
        assert_eq!(metadata["birthday_required"], false);
        assert_eq!(metadata["birthday_min_age"], 0);
        assert_eq!(
            metadata["features"],
            json!(instance_meta::features()),
            "the shared feature list"
        );
        assert_eq!(
            metadata["restrict_unauthenticated"],
            instance_meta::restrict_unauthenticated(),
            "the shared access restrictions"
        );
        assert_eq!(
            metadata["federation"],
            json!({"enabled": true, "mrf_policies": [], "quarantined_instances": [], "exclusions": false}),
            "plain federation, no filters"
        );
        assert_eq!(
            metadata["fields_limits"],
            json!({"max_fields": 10, "max_remote_fields": 20, "name_length": 512, "value_length": 2048}),
            "Pleroma's shipped field limits, snake_cased"
        );
        assert_eq!(
            metadata["post_formats"],
            json!(["text/plain", "text/html", "text/markdown"]),
            "the three formats this server renders"
        );
        assert_eq!(
            metadata["markup"],
            json!({
                "allow_inline_images": false,
                "allow_headings": false,
                "allow_tables": false,
                "allow_fonts": false,
            }),
            "the ticket's markup answer"
        );
    }
}
