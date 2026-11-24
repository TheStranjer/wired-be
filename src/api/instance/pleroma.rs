//! The `pleroma` extension block both instance documents carry: the
//! capability metadata, the federation state, the profile-field limits and
//! the markup allowances. The federation, field-limit and markup blocks are
//! the ones `NodeInfo` publishes too, defined in
//! [`crate::services::instance_meta`].

use serde::Serialize;
use serde_json::Value;

use crate::{
    config::Config,
    services::instance_meta::{self, Federation, FieldsLimits, Markup, POST_FORMATS},
};

use super::MONTHLY_ACTIVE_USERS;

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
            restrict_unauthenticated: instance_meta::restrict_unauthenticated(config),
            markup: Markup::sanitizer_allowances(),
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
            instance_meta::restrict_unauthenticated(&Config::for_tests()),
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
                "allow_inline_images": true,
                "allow_headings": true,
                "allow_tables": false,
                "allow_fonts": false,
            }),
            "what the sanitizer lets through"
        );
    }
}
