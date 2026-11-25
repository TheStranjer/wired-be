//! The protocol layer: the map of how `WiredBE` speaks more than one wire protocol.
//!
//! A *protocol* is one federation or messaging protocol this codebase
//! implements (today only `ActivityPub`). Each protocol is named by [`ProtocolId`]
//! and compiled in by one Cargo feature.
//!
//! # Naming rule
//!
//! Each protocol has exactly one id string, used everywhere: the `PROTOCOLS`
//! env value, the Cargo feature, the module `src/protocols/<id>.rs`, and the
//! prefix of its tables (`<id>_...`). `ActivityPub`'s id is `activitypub`; the
//! test-only fixture protocol's id is `fixture`.
//!
//! # Adding a protocol
//!
//! 1. Add a variant to [`ProtocolId`]. The compiler then points at every
//!    `match` on it (they have no wildcard arm) and at [`ProtocolId::ALL`].
//! 2. Add a Cargo feature named by the id to `Cargo.toml`, and add it to the
//!    `any(...)` list of the `compile_error!` below.
//! 3. Put the protocol's code in `src/protocols/<id>.rs`.
//!
//! # Where later pieces live
//!
//! Under `src/protocols/`:
//!
//! - [`adapter`] holds [`ProtocolAdapter`](adapter::ProtocolAdapter), the one
//!   trait every protocol implements; required methods are decisions every
//!   protocol makes, methods with a default are optional hooks.
//! - [`registry`] maps each [`ProtocolId`] to its adapter
//!   ([`registry::adapter`]) and lists the ones an instance runs
//!   ([`registry::enabled`]); [`crate::app`] mounts exactly those.
//! - [`activitypub`] is `ActivityPub`'s adapter, a thin façade over the code in
//!   `src/federation/` and `src/api/activitypub*`.
//! - `fixture` (test builds only) is the minimal adapter, the template to copy
//!   for a new protocol.
//!
//! Owned `/.well-known` paths come from
//! [`ProtocolAdapter::well_known_routes`](adapter::ProtocolAdapter::well_known_routes)
//! and are mounted by [`crate::discovery`].
//!
//! [`identity`] holds the identifier schemes: how an account gets its ID on
//! each protocol (see its docs). [`events`] holds the account lifecycle
//! events every enabled protocol reacts to. This file only names the protocols
//! and declares the submodules.

pub mod activitypub;
pub mod adapter;
pub mod events;
#[cfg(any(test, feature = "integration-tests"))]
pub mod fixture;
pub mod identity;
pub mod registry;

#[cfg(not(any(feature = "activitypub")))]
compile_error!("WiredBE needs at least one protocol feature: activitypub");
// Whoever adds a protocol adds its feature to the `any(...)` list above and to
// the message. The fixture protocol never counts: it exists only in test
// builds, so it must not satisfy this check.

/// Names one protocol this codebase implements.
///
/// Every `match` on this enum must list each variant and have no wildcard arm,
/// so that adding a variant fails to compile until every site handles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProtocolId {
    /// `ActivityPub`, the fediverse protocol; id `activitypub`.
    ActivityPub,
    /// The test-only fixture protocol; id `fixture`. Never in a release build.
    #[cfg(any(test, feature = "integration-tests"))]
    Fixture,
}

impl ProtocolId {
    /// Every protocol in this build, in registry order, `ActivityPub` first.
    pub const ALL: &'static [Self] = &[
        Self::ActivityPub,
        #[cfg(any(test, feature = "integration-tests"))]
        Self::Fixture,
    ];

    /// The protocol's one id string.
    ///
    /// It is the `PROTOCOLS` value, the Cargo feature, the module name and the
    /// table prefix (see the module docs).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActivityPub => "activitypub",
            #[cfg(any(test, feature = "integration-tests"))]
            Self::Fixture => "fixture",
        }
    }

    /// Parses an id string, ignoring ASCII case and surrounding whitespace.
    ///
    /// Returns `None` for a name that is not a protocol of this build.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .iter()
            .copied()
            .find(|id| id.as_str().eq_ignore_ascii_case(name))
    }

    /// Whether this build includes the protocol's code.
    ///
    /// `ActivityPub` follows its Cargo feature; the fixture is compiled wherever
    /// it exists.
    #[must_use]
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::ActivityPub => cfg!(feature = "activitypub"),
            #[cfg(any(test, feature = "integration-tests"))]
            Self::Fixture => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Every id in `ALL` parses back from its own string.
    #[test]
    fn parse_round_trips_every_id() {
        for id in ProtocolId::ALL {
            assert_eq!(ProtocolId::parse(id.as_str()), Some(*id));
        }
    }

    /// `parse` ignores ASCII case.
    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(
            ProtocolId::parse("ActivityPub"),
            Some(ProtocolId::ActivityPub)
        );
        assert_eq!(
            ProtocolId::parse("ACTIVITYPUB"),
            Some(ProtocolId::ActivityPub)
        );
        assert_eq!(ProtocolId::parse("FIXTURE"), Some(ProtocolId::Fixture));
    }

    /// `parse` trims surrounding whitespace.
    #[test]
    fn parse_trims_whitespace() {
        assert_eq!(
            ProtocolId::parse("  activitypub\t\n"),
            Some(ProtocolId::ActivityPub)
        );
    }

    /// `parse` rejects unknown, empty and padded-inside names.
    #[test]
    fn parse_rejects_unknown_names() {
        assert_eq!(ProtocolId::parse("matrix"), None);
        assert_eq!(ProtocolId::parse(""), None);
        assert_eq!(ProtocolId::parse("   "), None);
        assert_eq!(ProtocolId::parse("activity pub"), None);
        assert_eq!(ProtocolId::parse("activitypub,fixture"), None);
    }

    /// `ActivityPub` is first in registry order.
    #[test]
    fn activitypub_is_first() {
        assert_eq!(ProtocolId::ALL.first(), Some(&ProtocolId::ActivityPub));
    }

    /// No two protocols share an id string, and no variant is listed twice.
    #[test]
    fn id_strings_are_unique() {
        let names: HashSet<&str> = ProtocolId::ALL
            .iter()
            .copied()
            .map(ProtocolId::as_str)
            .collect();
        let variants: HashSet<ProtocolId> = ProtocolId::ALL.iter().copied().collect();
        assert_eq!(names.len(), ProtocolId::ALL.len());
        assert_eq!(variants.len(), ProtocolId::ALL.len());
    }

    /// The id strings are the documented ones.
    #[test]
    fn as_str_gives_the_documented_ids() {
        assert_eq!(ProtocolId::ActivityPub.as_str(), "activitypub");
        assert_eq!(ProtocolId::Fixture.as_str(), "fixture");
    }

    /// Every protocol of a default test build is compiled in.
    #[test]
    fn every_listed_protocol_is_compiled_in_default_builds() {
        assert!(ProtocolId::ActivityPub.is_compiled());
        assert!(ProtocolId::Fixture.is_compiled());
    }

    /// The derived order follows `ALL`.
    #[test]
    fn ordering_follows_registry_order() {
        let mut sorted = ProtocolId::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, ProtocolId::ALL);
    }
}
