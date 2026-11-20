//! How the pipeline publishes itself: the operator's transparency
//! settings, what the instance documents say about the live chain, and
//! the [`describe`] answer the federation metadata and `NodeInfo` embed.

use serde_json::{Map, Value};

use super::{
    chain::{PolicyChain, active_chain},
    simple,
};
use crate::{config::Config, state::AppState};

/// How the instance publishes the moderation lists its policies hold.
///
/// Pleroma's `[:mrf, :transparency]` switch and
/// `[:mrf, :transparency_exclusions]` hosts, spelled here as
/// `MRF_TRANSPARENCY` and `MRF_TRANSPARENCY_EXCLUSIONS`. The switch gates
/// the whole transparency answer — with it off the instance documents carry
/// no policy names and no lists — and the exclusions are compiled with the
/// same [`simple::DomainMatcher`] the policy lists match with, so an
/// exclusion of `*.example.com` hides that entry and every entry below it,
/// verbatim patterns included.
#[derive(Clone, Debug, Default)]
pub struct Transparency {
    /// Whether the lists are published at all (`MRF_TRANSPARENCY`).
    published: bool,
    /// The host patterns kept out of them
    /// (`MRF_TRANSPARENCY_EXCLUSIONS`).
    hidden: simple::DomainMatcher,
}

impl Transparency {
    /// The settings as the instance's `config` carries them.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            published: config.mrf.transparency.get(),
            hidden: simple::DomainMatcher::from_patterns(&config.mrf.transparency_exclusions),
        }
    }

    /// The settings that publish every list whole: what a policy's own
    /// transparency test reads, and what an instance with the flag on and
    /// no exclusions configured behaves exactly like.
    #[must_use]
    pub fn open() -> Self {
        Self {
            published: true,
            hidden: simple::DomainMatcher::default(),
        }
    }

    /// The settings that publish everything but `exclusions`, for callers
    /// (and tests) that hold the pattern list rather than a [`Config`].
    #[must_use]
    pub fn hiding(exclusions: &[String]) -> Self {
        Self {
            published: true,
            hidden: simple::DomainMatcher::from_patterns(exclusions),
        }
    }

    /// Whether the documents carry the lists at all.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.published
    }

    /// Whether the operator configured any exclusion. Pleroma discloses the
    /// *use* of the feature — the flag is true on the strength of the list,
    /// whether or not it happened to match a published entry.
    #[must_use]
    pub const fn excludes_anything(&self) -> bool {
        !self.hidden.is_empty()
    }

    /// Whether one list entry's `pattern` is kept out of public output.
    #[must_use]
    pub fn hides(&self, pattern: &str) -> bool {
        self.hidden.contains(pattern)
    }
}

/// What the instance documents publish about one chain.
///
/// The three fixed keys of the `federation` metadata block beside the
/// flattened policy objects, computed once by [`describe`] so
/// `GET /api/v1/instance`, `GET /api/v2/instance` and `NodeInfo` cannot
/// drift apart.
#[derive(Clone, Debug, Default)]
pub struct ChainTransparency {
    /// Whether anything beyond the fixed markers is published; it is the
    /// `quarantined_instances` key's presence that rides on it.
    pub published: bool,
    /// `mrf_policies`: the Pleroma-style names in chain order, empty when
    /// nothing is published.
    pub policies: Vec<String>,
    /// `mrf_simple`, `mrf_simple_info`, `mrf_keyword` and the rest: every
    /// policy's transparency blocks under the keys they name themselves.
    pub descriptions: Map<String, Value>,
    /// `exclusions`: whether the published lists hold back any host.
    pub exclusions: bool,
}

impl ChainTransparency {
    /// The `quarantined_instances` key of the published block: the empty
    /// list while the lists are published — wired-be keeps no instance
    /// quarantine — and absent from the transparency-off answer, which is
    /// only ever `{"enabled": …, "mrf_policies": [], "exclusions": false}`.
    #[must_use]
    pub fn quarantined_instances(&self) -> Option<Vec<&'static str>> {
        self.published.then(Vec::new)
    }
}

/// The chain's policies by their Pleroma-style names, in chain
/// order — the `mrf_policies` list of the federation metadata.
#[must_use]
pub fn describe_policies(chain: &PolicyChain) -> Vec<String> {
    chain
        .policies
        .iter()
        .map(|policy| policy.name().to_owned())
        .collect()
}

/// The chain's transparency blocks for the federation metadata: every
/// policy's [`describe`](super::MrfPolicy::describe) merged in chain order,
/// under the keys each policy names for itself.
#[must_use]
pub fn describe_transparency(
    chain: &PolicyChain,
    transparency: &Transparency,
) -> Map<String, Value> {
    chain
        .policies
        .iter()
        .flat_map(|policy| policy.describe(transparency))
        .collect()
}

/// The whole transparency answer for `chain` under the operator's
/// `transparency` settings: the names, the policy blocks and the
/// `exclusions` marker the instance documents publish beside them.
///
/// With transparency off nothing is published — not even the policy names,
/// because naming a filter whose lists the instance withholds says more
/// than the setting allows. Pleroma's `InstanceView.federation` likewise
/// skips `MRF.describe/1` when `[:mrf, :transparency]` is false.
#[must_use]
pub fn describe_chain(chain: &PolicyChain, transparency: &Transparency) -> ChainTransparency {
    if !transparency.enabled() {
        return ChainTransparency::default();
    }
    ChainTransparency {
        published: true,
        policies: describe_policies(chain),
        descriptions: describe_transparency(chain, transparency),
        exclusions: transparency.excludes_anything(),
    }
}

/// The transparency the live chain publishes under the instance's own
/// settings.
///
/// This is what [`crate::api::instance`] and [`crate::api::nodeinfo`] put
/// in their `federation` block, computed in the one place so the three
/// documents cannot disagree.
#[must_use]
pub async fn describe(state: &AppState) -> ChainTransparency {
    let chain = active_chain(state).await;
    describe_chain(&chain, &Transparency::from_config(&state.config))
}

/// The one-block entry of a policy that publishes a single object under its
/// own `mrf_<policy>` key.
///
/// That is the shape most of Pleroma's policies describe themselves in, and
/// what the default [`describe`](super::MrfPolicy::describe) hands back
/// empty.
#[must_use]
pub fn single_block(name: &str, body: Value) -> Map<String, Value> {
    Map::from_iter([(transparency_key(name), body)])
}

/// The federation-metadata key a policy's single block is served under,
/// Pleroma's mapping: `"SimplePolicy"` → `"mrf_simple"`.
fn transparency_key(name: &str) -> String {
    let stem = name.strip_suffix("Policy").unwrap_or(name);
    format!("mrf_{}", stem.to_lowercase())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        config::Toggle,
        federation::mrf::{NoOpPolicy, test_support::simple_chain},
    };

    /// The transparency keys follow Pleroma's mapping, and a chain's
    /// blocks are keyed per policy.
    #[test]
    fn transparency_blocks_key_by_pleroma_convention() {
        assert_eq!(transparency_key("SimplePolicy"), "mrf_simple");
        assert_eq!(transparency_key("NoOpPolicy"), "mrf_noop");
        assert_eq!(transparency_key("ObjectAgePolicy"), "mrf_objectage");
        assert_eq!(transparency_key("Bare"), "mrf_bare");
        let chain = PolicyChain {
            policies: vec![Box::new(NoOpPolicy)],
        };
        assert_eq!(
            describe_transparency(&chain, &Transparency::open()),
            Map::from_iter([("mrf_noop".to_owned(), json!({}))]),
            "the shipped policy's describe() is the empty object"
        );
    }

    /// `MRF_TRANSPARENCY` is the whole answer's switch: with it off the
    /// documents publish no policy names, no lists and not even the
    /// quarantine key — Pleroma's `MRF.describe/1` never runs either.
    #[test]
    fn transparency_off_publishes_nothing_at_all() {
        let chain = simple_chain(&json!({"reject": [["evil.example", "bots"]]}));
        let open = describe_chain(&chain, &Transparency::open());
        assert_eq!(
            open.policies,
            vec!["SimplePolicy".to_owned()],
            "with transparency on the chain's names are published"
        );
        assert_eq!(
            open.descriptions["mrf_simple"]["reject"],
            json!(["evil.example"]),
            "beside the lists themselves"
        );
        assert_eq!(
            open.quarantined_instances(),
            Some(Vec::new()),
            "and the block keeps its (empty) quarantine key"
        );
        assert!(!open.exclusions, "with no exclusions configured");

        let shut = describe_chain(&chain, &Transparency::default());
        assert!(
            shut.policies.is_empty(),
            "an instance that will not describe its filters does not name them either"
        );
        assert!(shut.descriptions.is_empty(), "and publishes no lists");
        assert!(
            shut.quarantined_instances().is_none(),
            "the off answer is the fixed keys alone"
        );
        assert!(!shut.exclusions, "and it discloses no exclusion use");
    }

    /// An exclusion is disclosed on the strength of the setting — Pleroma
    /// counts the list, not the matches — and hides the hosts it names.
    #[test]
    fn exclusions_hide_their_hosts_and_say_they_do() {
        let lists = json!({"reject": [["evil.example", "bots"], "press.example"]});
        let chain = simple_chain(&lists);

        let described =
            describe_chain(&chain, &Transparency::hiding(&["press.example".to_owned()]));
        assert!(
            described.exclusions,
            "the published block says something is held back"
        );
        assert_eq!(
            described.descriptions["mrf_simple"]["reject"],
            json!(["evil.example"]),
            "and the excluded host is not in the list"
        );
        assert_eq!(
            described.descriptions["mrf_simple_info"],
            json!({"reject": {"evil.example": {"reason": "bots"}}}),
            "the reason of the host that stands goes into the info object \
             beside it, and the excluded one has no reason left to show"
        );

        let unmatched = describe_chain(
            &chain,
            &Transparency::hiding(&["nobody.example".to_owned()]),
        );
        assert!(
            unmatched.exclusions,
            "an exclusion that matched nothing is still an exclusion in use"
        );
        assert_eq!(
            unmatched.descriptions["mrf_simple"]["reject"],
            json!(["evil.example", "press.example"]),
            "and it hid nobody"
        );
    }

    /// [`Transparency::from_config`] is the two settings, read off the
    /// environment-backed [`Config`] the way every other setting is.
    #[test]
    fn the_settings_read_the_environment_back_into_transparency() {
        let config = Config::for_tests();
        let open = Transparency::from_config(&config);
        assert!(open.enabled(), "transparency is on unless told otherwise");
        assert!(
            !open.excludes_anything(),
            "and nothing is excluded by default"
        );

        let mut shut = Config::for_tests();
        shut.mrf.transparency = Toggle::new(false);
        shut.mrf.transparency_exclusions = vec!["*.partner.example".to_owned()];
        let shut = Transparency::from_config(&shut);
        assert!(!shut.enabled(), "MRF_TRANSPARENCY=false closes the lists");
        assert!(
            shut.excludes_anything() && shut.hides("cdn.partner.example"),
            "and the exclusion pattern is compiled with the same rules the \
             policy lists match with"
        );
    }
}
