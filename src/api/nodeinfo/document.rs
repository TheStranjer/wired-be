//! The `NodeInfo` documents themselves: the discovery document, the
//! per-schema statistics document and its `software` and `services` blocks.

use serde::Serialize;

use super::{REL_20, REL_21, SOFTWARE_NAME, SchemaVersion, metadata::Metadata, usage::Usage};
use crate::{
    config::{Config, Toggle},
    protocols::{adapter::ProtocolAdapter, registry},
    services::instance_meta::{PLEROMA_COMPATIBILITY_VERSION, SOURCE_URL},
};

/// The discovery document of `/.well-known/nodeinfo`.
#[derive(Debug, Serialize)]
pub(super) struct Discovery {
    /// The schema links, 2.0 first, exactly as Pleroma orders them.
    links: Vec<SchemaLink>,
}

/// One `links` entry of a [`Discovery`] document.
#[derive(Debug, Serialize)]
struct SchemaLink {
    /// The schema URI this document is written in.
    rel: &'static str,
    /// This instance's document for that schema.
    href: String,
}

impl Discovery {
    /// The document naming the two schema documents of the instance served
    /// at `base_url` (which never ends in a slash).
    pub(super) fn for_base_url(base_url: &str) -> Self {
        Self {
            links: vec![
                SchemaLink {
                    rel: REL_20,
                    href: format!("{base_url}/nodeinfo/2.0.json"),
                },
                SchemaLink {
                    rel: REL_21,
                    href: format!("{base_url}/nodeinfo/2.1.json"),
                },
            ],
        }
    }
}

/// The whole `NodeInfo` document. `NodeInfo` has no fixed key order, but the
/// schema's own example and Pleroma both lead with `version`, so the
/// fields are declared in that order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NodeInfo {
    /// The schema this document is written in.
    version: &'static str,
    /// Which software this instance runs.
    software: Software,
    /// The federation protocols this instance speaks, as `NodeInfo` names
    /// them, one per enabled protocol that has a name.
    protocols: Vec<&'static str>,
    /// Third-party sign-in and sharing services; this server integrates
    /// with none (Pleroma's own answer is the same two empty lists).
    services: Services,
    /// Whether anyone may register here, from `REGISTRATIONS_OPEN`.
    open_registrations: Toggle,
    /// The live counters.
    usage: Usage,
    /// The Pleroma-compatible metadata block.
    metadata: Metadata,
}

impl NodeInfo {
    /// The document for `version`, describing the instance `config`
    /// serves with the database-read `usage` counters and the built
    /// `metadata` block (its `federation` half reads live state, so the
    /// endpoint assembles it).
    pub(super) fn build(
        version: SchemaVersion,
        config: &Config,
        usage: Usage,
        metadata: Metadata,
    ) -> Self {
        Self {
            version: version.as_str(),
            software: Software::for_version(version),
            protocols: registry::enabled(config)
                .filter_map(ProtocolAdapter::nodeinfo_protocol)
                .collect(),
            services: Services::empty(),
            open_registrations: config.registrations_open,
            usage,
            metadata,
        }
    }
}

/// The `software` block.
#[derive(Debug, Serialize)]
struct Software {
    /// Always `"wired"`: the name this server identifies itself with.
    name: &'static str,
    /// The wired-be crate version, commented with the Pleroma release
    /// this server's HTTP interface is compatible with.
    version: String,
    /// Source repository — only the 2.1 schema has this key.
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'static str>,
    /// Project homepage — only the 2.1 schema has this key.
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<&'static str>,
}

impl Software {
    /// The software block of a `version` document: the 2.1 schema carries
    /// the repository links, the 2.0 schema has no keys for them (Pleroma's
    /// 2.1 document is its 2.0 document with `repository` added).
    fn for_version(version: SchemaVersion) -> Self {
        let with_links = version.has_software_links();
        Self {
            name: SOFTWARE_NAME,
            version: format!(
                "{} (compatible; Pleroma {PLEROMA_COMPATIBILITY_VERSION})",
                env!("CARGO_PKG_VERSION")
            ),
            repository: with_links.then_some(SOURCE_URL),
            homepage: with_links.then_some(SOURCE_URL),
        }
    }
}

/// The `services` block: third-party services clients may plug into.
#[derive(Debug, Serialize)]
struct Services {
    /// Services users can bring their posts from (external posting).
    inbound: Vec<&'static str>,
    /// Services posts can be shared to.
    outbound: Vec<&'static str>,
}

impl Services {
    /// This server integrates with no third-party service.
    const fn empty() -> Self {
        Self {
            inbound: Vec::new(),
            outbound: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::usage::UserCounts;
    use super::*;
    use crate::{
        config::{BASE_URL_VAR, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL},
        services::instance_meta::{self, Federation},
    };

    /// The empty-usage document for `version`, built on the test config.
    fn document(version: SchemaVersion) -> Value {
        document_with(version, &Config::for_tests())
    }

    /// The empty-usage document for `version`, built on `config`.
    fn document_with(version: SchemaVersion, config: &Config) -> Value {
        let document = NodeInfo::build(
            version,
            config,
            Usage {
                users: UserCounts {
                    total: 0,
                    active_month: 0,
                    active_halfyear: 0,
                },
                local_posts: 0,
            },
            Metadata::build(config, Vec::new(), Federation::federating()),
        );
        serde_json::to_value(&document).unwrap()
    }

    /// The test configuration with `PROTOCOLS` set to `protocols`.
    fn config_with_protocols(protocols: &str) -> Config {
        Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            PROTOCOLS_VAR => Some(protocols.to_owned()),
            _ => None,
        })
        .expect("the fixed protocols test config is valid")
    }

    #[test]
    fn the_default_config_lists_activitypub_in_both_versions() {
        for version in [SchemaVersion::V20, SchemaVersion::V21] {
            assert_eq!(
                document(version)["protocols"],
                json!(["activitypub"]),
                "the default PROTOCOLS names only ActivityPub"
            );
        }
    }

    #[test]
    fn a_protocol_nodeinfo_has_no_name_for_is_omitted() {
        let config = config_with_protocols("activitypub,fixture");
        for version in [SchemaVersion::V20, SchemaVersion::V21] {
            assert_eq!(
                document_with(version, &config)["protocols"],
                json!(["activitypub"]),
                "the fixture returns no NodeInfo name"
            );
        }
    }

    #[test]
    fn a_fixture_only_instance_lists_no_protocol() {
        let config = config_with_protocols("fixture");
        assert_eq!(
            document_with(SchemaVersion::V20, &config)["protocols"],
            json!([]),
            "no enabled protocol has a NodeInfo name"
        );
    }

    #[test]
    fn the_metadata_reports_the_configured_restrictions() {
        let mut config = Config::for_tests();
        config.restrict_unauthenticated = ["timelines_federated", "activities_local"]
            .iter()
            .map(|area| (*area).to_owned())
            .collect();
        let document = document_with(SchemaVersion::V21, &config);
        assert_eq!(
            document["metadata"]["restrict_unauthenticated"],
            json!({
                "timelines": { "local": false, "federated": true },
                "profiles": { "local": false, "remote": false },
                "activities": { "local": true, "remote": false },
            })
        );
    }

    #[test]
    fn the_discovery_document_links_both_json_suffixed_docs() {
        let value = serde_json::to_value(Discovery::for_base_url("https://test.example")).unwrap();
        assert_eq!(
            value,
            json!({
                "links": [
                    {
                        "rel": "http://nodeinfo.diaspora.software/ns/schema/2.0",
                        "href": "https://test.example/nodeinfo/2.0.json",
                    },
                    {
                        "rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
                        "href": "https://test.example/nodeinfo/2.1.json",
                    },
                ],
            }),
            "the two links, in Pleroma's order"
        );
    }

    /// The object keys of `value`, for comparing key sets.
    fn keys_of(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .expect("a document is an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn both_documents_share_their_key_sets_except_version_and_software() {
        let v20 = document(SchemaVersion::V20);
        let v21 = document(SchemaVersion::V21);
        assert_eq!(keys_of(&v20), keys_of(&v21), "the same top-level keys");
        assert_eq!(
            keys_of(&v20["software"]),
            vec!["name", "version"],
            "2.0 software has no link keys"
        );
        assert_eq!(
            keys_of(&v21["software"]),
            vec!["homepage", "name", "repository", "version"],
            "2.1 adds repository and homepage (Value maps sort keys)"
        );
    }

    #[test]
    fn the_document_identifies_the_server_as_wired() {
        let v20 = document(SchemaVersion::V20);
        assert_eq!(v20["version"], "2.0", "the version field");
        assert_eq!(v20["software"]["name"], "wired", "the server is Wired");
        assert_eq!(
            v20["software"]["version"],
            format!("{} (compatible; Pleroma 2.10.2)", env!("CARGO_PKG_VERSION")),
            "Wired's own version first, Pleroma named as the compatibility target"
        );
        assert_eq!(v20["protocols"], json!(["activitypub"]), "one protocol");
        assert_eq!(
            v20["services"],
            json!({"inbound": [], "outbound": []}),
            "no third-party services"
        );
    }

    #[test]
    fn the_metadata_block_holds_the_pleroma_defaults() {
        let v21 = document(SchemaVersion::V21);
        let metadata = &v21["metadata"];
        assert_eq!(metadata["nodeName"], "Wired", "from the test config");
        assert_eq!(
            metadata["nodeDescription"], "A Wired instance, compatible with Pleroma clients",
            "from the test config"
        );
        assert_eq!(metadata["private"], false, "a public instance");
        assert_eq!(metadata["suggestions"], json!({"enabled": false}));
        assert_eq!(metadata["accountActivationRequired"], false);
        assert_eq!(metadata["invitesEnabled"], false, "INVITES_ENABLED default");
        assert_eq!(metadata["mailerEnabled"], false);
        assert_eq!(metadata["skipThreadContainment"], true, "Pleroma 2.10's");
        assert_eq!(
            metadata["postFormats"],
            json!(["text/plain", "text/html", "text/markdown"]),
            "the three formats this server renders"
        );
        assert_eq!(
            metadata["federation"],
            json!({
                "enabled": true,
                "mrf_policies": [],
                "quarantined_instances": [],
                "exclusions": false,
            }),
            "plain federation, no filters"
        );
        assert_eq!(
            metadata["pollLimits"],
            json!({"max_options": 20, "max_option_chars": 200, "min_expiration": 0, "max_expiration": 31_536_000}),
            "the configured option count with Pleroma's window"
        );
        assert_eq!(
            metadata["uploadLimits"],
            json!({"general": 16_000_000, "avatar": 2_000_000, "banner": 4_000_000, "background": 4_000_000}),
            "the configured general limit with Pleroma's per-purpose defaults"
        );
        assert_eq!(
            metadata["fieldsLimits"],
            json!({"maxFields": 10, "maxRemoteFields": 20, "nameLength": 512, "valueLength": 2048}),
            "Pleroma's shipped field limits"
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
        assert_eq!(
            metadata["features"],
            json!(instance_meta::features()),
            "the shared feature list"
        );
        assert_eq!(
            metadata["restrict_unauthenticated"],
            instance_meta::restrict_unauthenticated(&Config::for_tests()),
            "the shared access restrictions, under its snake_case key"
        );
        assert_eq!(metadata["staffAccounts"], json!([]), "no staff fixture");
    }

    #[test]
    fn usage_counts_serialize_under_the_schema_names() {
        let config = Config::for_tests();
        let document = NodeInfo::build(
            SchemaVersion::V20,
            &config,
            Usage {
                users: UserCounts {
                    total: 12,
                    active_month: 4,
                    active_halfyear: 9,
                },
                local_posts: 77,
            },
            Metadata::build(
                &config,
                vec!["https://test.example/users/alice".to_owned()],
                Federation::federating(),
            ),
        );
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(
            value["usage"],
            json!({
                "users": {"total": 12, "activeMonth": 4, "activeHalfyear": 9},
                "localPosts": 77,
            }),
            "the schema's camelCase counters"
        );
        assert_eq!(value["openRegistrations"], true, "the test config default");
        assert_eq!(
            value["metadata"]["staffAccounts"],
            json!(["https://test.example/users/alice"]),
            "staff actor ids pass through"
        );
    }
}
