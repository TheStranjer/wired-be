//! Reads `PROTOCOLS`: which of the compiled-in protocols this instance runs,
//! kept as [`EnabledProtocols`].
//!
//! A protocol is named by [`ProtocolId`]; this module only decides which of
//! them an operator switched on, and refuses to start on a value it cannot
//! honour.

use std::{iter::Copied, slice::Iter};

use crate::protocols::ProtocolId;

use super::{
    defaults::DEFAULT_PROTOCOLS, error::ConfigError, parse::comma_list, vars::PROTOCOLS_VAR,
};

/// The protocols an instance runs, always in [`ProtocolId::ALL`] order.
///
/// The order is the registry order, so it never depends on how the operator
/// wrote `PROTOCOLS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnabledProtocols(Vec<ProtocolId>);

impl EnabledProtocols {
    /// Whether `protocol` is one of the enabled protocols.
    #[must_use]
    pub fn contains(&self, protocol: ProtocolId) -> bool {
        self.0.contains(&protocol)
    }

    /// The enabled protocols in [`ProtocolId::ALL`] order.
    pub fn iter(&self) -> Copied<Iter<'_, ProtocolId>> {
        self.0.iter().copied()
    }

    /// Only `ActivityPub`, what the default `PROTOCOLS` gives.
    #[cfg(any(test, feature = "integration-tests"))]
    #[must_use]
    pub fn activitypub_only() -> Self {
        Self(vec![ProtocolId::ActivityPub])
    }
}

impl<'a> IntoIterator for &'a EnabledProtocols {
    type Item = ProtocolId;
    type IntoIter = Copied<Iter<'a, ProtocolId>>;

    /// Iterates like [`EnabledProtocols::iter`].
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Reads `PROTOCOLS`, [`DEFAULT_PROTOCOLS`] when unset.
///
/// A set but empty value is an error, not the default: an operator who wrote
/// `PROTOCOLS=` asked for no protocol at all.
///
/// # Errors
///
/// Returns [`ConfigError::EmptyProtocols`] when the list names nothing,
/// [`ConfigError::UnknownProtocol`] for a name that is no [`ProtocolId`], and
/// [`ConfigError::ProtocolNotCompiled`] for a known protocol this binary was
/// built without.
pub(crate) fn enabled_protocols<F>(lookup: &F) -> Result<EnabledProtocols, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup(PROTOCOLS_VAR).unwrap_or_else(|| DEFAULT_PROTOCOLS.to_owned());
    parse_protocols(&raw, ProtocolId::is_compiled)
}

/// Parses a `PROTOCOLS` value, asking `is_compiled` whether a protocol's code
/// is in this binary.
///
/// The predicate is a parameter so a test can play a binary built without a
/// protocol; [`enabled_protocols`] passes [`ProtocolId::is_compiled`].
fn parse_protocols(
    raw: &str,
    is_compiled: fn(ProtocolId) -> bool,
) -> Result<EnabledProtocols, ConfigError> {
    let names = comma_list(raw);
    if names.is_empty() {
        return Err(ConfigError::EmptyProtocols);
    }
    let mut chosen = Vec::with_capacity(names.len());
    for name in &names {
        let protocol =
            ProtocolId::parse(name).ok_or_else(|| ConfigError::UnknownProtocol(name.clone()))?;
        if !is_compiled(protocol) {
            return Err(ConfigError::ProtocolNotCompiled(protocol));
        }
        chosen.push(protocol);
    }
    // Walking `ALL` rather than `chosen` is what fixes the order and drops
    // duplicates.
    Ok(EnabledProtocols(
        ProtocolId::ALL
            .iter()
            .copied()
            .filter(|protocol| chosen.contains(protocol))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{
        Config,
        test_support::{config_with, try_config_with},
    };

    /// The protocols `config` enabled, as a vector.
    fn enabled(config: &Config) -> Vec<ProtocolId> {
        config.protocols.iter().collect()
    }

    #[test]
    fn unset_gives_activitypub() {
        let config = config_with(&[]);
        assert_eq!(enabled(&config), vec![ProtocolId::ActivityPub]);
        assert!(config.protocols.contains(ProtocolId::ActivityPub));
        assert!(!config.protocols.contains(ProtocolId::Fixture));
    }

    #[test]
    fn explicit_activitypub_is_accepted() {
        let config = config_with(&[(PROTOCOLS_VAR, "activitypub")]);
        assert_eq!(enabled(&config), vec![ProtocolId::ActivityPub]);
    }

    #[test]
    fn mixed_case_and_spaces_are_accepted() {
        let config = config_with(&[(PROTOCOLS_VAR, "  ActivityPub  ")]);
        assert_eq!(enabled(&config), vec![ProtocolId::ActivityPub]);
    }

    #[test]
    fn duplicates_collapse() {
        let config = config_with(&[(PROTOCOLS_VAR, "activitypub, ACTIVITYPUB,activitypub")]);
        assert_eq!(enabled(&config), vec![ProtocolId::ActivityPub]);
    }

    #[test]
    fn empty_list_is_an_error() {
        for value in ["", " ", ",", " , ", ",,,"] {
            let err = try_config_with(&[(PROTOCOLS_VAR, value)]).unwrap_err();
            assert_eq!(err, ConfigError::EmptyProtocols, "value {value:?}");
        }
        let message = ConfigError::EmptyProtocols.to_string();
        assert!(message.starts_with("PROTOCOLS: "), "message {message}");
        assert!(message.contains("activitypub"), "message {message}");
    }

    #[test]
    fn unknown_name_is_an_error() {
        let err = try_config_with(&[(PROTOCOLS_VAR, "activitypub, foo")]).unwrap_err();
        assert_eq!(err, ConfigError::UnknownProtocol("foo".to_owned()));
        assert_eq!(
            err.to_string(),
            "PROTOCOLS: \"foo\" is not a protocol WiredBE knows"
        );
    }

    #[test]
    fn fixture_is_accepted_in_test_builds() {
        let config = config_with(&[(PROTOCOLS_VAR, "fixture")]);
        assert_eq!(enabled(&config), vec![ProtocolId::Fixture]);
        assert!(!config.protocols.contains(ProtocolId::ActivityPub));
    }

    #[test]
    fn result_is_in_all_order_not_written_order() {
        let reversed = config_with(&[(PROTOCOLS_VAR, "fixture,activitypub")]);
        let in_order = config_with(&[(PROTOCOLS_VAR, "activitypub,fixture")]);
        assert_eq!(
            enabled(&reversed),
            vec![ProtocolId::ActivityPub, ProtocolId::Fixture]
        );
        assert_eq!(
            enabled(&in_order),
            vec![ProtocolId::ActivityPub, ProtocolId::Fixture]
        );
    }

    #[test]
    fn a_protocol_missing_from_the_binary_is_an_error() {
        let err = parse_protocols("activitypub,fixture", |protocol| {
            protocol != ProtocolId::Fixture
        })
        .unwrap_err();
        assert_eq!(err, ConfigError::ProtocolNotCompiled(ProtocolId::Fixture));
        assert_eq!(
            err.to_string(),
            "PROTOCOLS: \"fixture\" is a known protocol, but this binary was built without the \
             \"fixture\" feature"
        );
    }

    #[test]
    fn entries_are_checked_in_the_order_written() {
        let err = parse_protocols("fixture,foo", |_| false).unwrap_err();
        assert_eq!(err, ConfigError::ProtocolNotCompiled(ProtocolId::Fixture));
    }

    #[test]
    fn for_tests_runs_activitypub_only() {
        let config = Config::for_tests();
        assert_eq!(enabled(&config), vec![ProtocolId::ActivityPub]);
        assert_eq!(config.protocols, EnabledProtocols::activitypub_only());
    }

    #[test]
    fn debug_shows_the_protocols() {
        let debug = format!("{:?}", config_with(&[]));
        assert!(debug.contains("protocols: EnabledProtocols([ActivityPub])"));
    }
}
