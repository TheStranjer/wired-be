//! The registry: which adapter belongs to which [`ProtocolId`], and which
//! adapters an instance runs.

use crate::{
    config::Config,
    protocols::{ProtocolId, activitypub::ActivityPub, adapter::ProtocolAdapter},
};

#[cfg(any(test, feature = "integration-tests"))]
use crate::protocols::fixture::Fixture;

/// The adapter for `id`.
///
/// The `match` lists every [`ProtocolId`] and has no wildcard arm, so adding a
/// protocol fails to compile here until it names its adapter.
#[must_use]
pub fn adapter(id: ProtocolId) -> &'static dyn ProtocolAdapter {
    match id {
        ProtocolId::ActivityPub => &ActivityPub,
        #[cfg(any(test, feature = "integration-tests"))]
        ProtocolId::Fixture => &Fixture,
    }
}

/// The adapters of the protocols `config` enables, in [`ProtocolId::ALL`] order.
pub fn enabled(config: &Config) -> impl Iterator<Item = &'static dyn ProtocolAdapter> {
    ProtocolId::ALL
        .iter()
        .copied()
        .filter(move |id| config.protocols.contains(*id))
        .map(adapter)
}

#[cfg(test)]
mod tests {
    use crate::config::{
        BASE_URL_VAR, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    };

    use super::*;

    /// A test config whose `PROTOCOLS` is `protocols`, or unset for `None`.
    fn config_with(protocols: Option<&str>) -> Config {
        Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            PROTOCOLS_VAR => protocols.map(str::to_owned),
            _ => None,
        })
        .expect("the fixed registry test config is valid")
    }

    /// The ids of the adapters `config` enables, in iteration order.
    fn enabled_ids(config: &Config) -> Vec<ProtocolId> {
        enabled(config).map(ProtocolAdapter::id).collect()
    }

    /// Every adapter reports the id it was looked up by.
    #[test]
    fn adapter_reports_its_own_id() {
        for id in ProtocolId::ALL {
            assert_eq!(adapter(*id).id(), *id);
        }
    }

    /// The default `PROTOCOLS` enables `ActivityPub` alone.
    #[test]
    fn enabled_defaults_to_activitypub() {
        assert_eq!(
            enabled_ids(&config_with(None)),
            vec![ProtocolId::ActivityPub]
        );
    }

    /// Only the protocols `PROTOCOLS` names are enabled.
    #[test]
    fn enabled_filters_by_config() {
        assert_eq!(
            enabled_ids(&config_with(Some("fixture"))),
            vec![ProtocolId::Fixture]
        );
    }

    /// The order is [`ProtocolId::ALL`], however the operator wrote it.
    #[test]
    fn enabled_follows_registry_order() {
        let config = config_with(Some("fixture, activitypub"));
        assert_eq!(enabled_ids(&config), ProtocolId::ALL.to_vec());
    }
}
