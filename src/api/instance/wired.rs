//! The `wired` extension block both instance documents carry
//! (`docs/ARCHITECTURE.md` decision 17).
//!
//! Wired's entity data sits under a `wired` key beside Mastodon's fields and
//! Pleroma's `pleroma` block, and only adds to them. Clients check for the
//! `wired:protocols` feature flag ([`instance_meta::features`]) before they
//! read it.
//!
//! [`instance_meta::features`]: crate::services::instance_meta::features

use serde::Serialize;

use crate::{
    config::Config,
    protocols::{ProtocolId, adapter::ProtocolAdapter, registry},
};

/// The `wired` block of the instance documents.
#[derive(Debug, Serialize)]
pub(super) struct WiredBlock {
    /// The ids ([`ProtocolId::as_str`](crate::protocols::ProtocolId::as_str))
    /// of the protocols this instance speaks, in registry order.
    protocols: Vec<&'static str>,
}

impl WiredBlock {
    /// The block for the protocols `config` enables.
    pub(super) fn for_config(config: &Config) -> Self {
        Self {
            protocols: registry::enabled(config)
                .map(ProtocolAdapter::id)
                .map(ProtocolId::as_str)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::{
        BASE_URL_VAR, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    };

    use super::*;

    /// The block serialised for a config whose `PROTOCOLS` is `protocols`.
    fn block_for(protocols: &str) -> serde_json::Value {
        let config = Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            PROTOCOLS_VAR => Some(protocols.to_owned()),
            _ => None,
        })
        .expect("the fixed protocols test config is valid");
        serde_json::to_value(WiredBlock::for_config(&config)).expect("the block serialises")
    }

    /// The default config lists `ActivityPub` alone.
    #[test]
    fn the_default_config_lists_activitypub() {
        let block = serde_json::to_value(WiredBlock::for_config(&Config::for_tests()))
            .expect("the block serialises");
        assert_eq!(block, json!({"protocols": ["activitypub"]}));
    }

    /// Every enabled protocol is listed, in registry order.
    #[test]
    fn every_enabled_protocol_is_listed_in_registry_order() {
        assert_eq!(
            block_for("fixture, activitypub"),
            json!({"protocols": ["activitypub", "fixture"]})
        );
    }
}
