//! Tests of the registry: the whitelisted keys, the lookups, the fallbacks and
//! the description payload.

use super::*;
use crate::services::config_registry::types::ValueKind;

/// The registry holds exactly the ticket's eight `:pleroma` keys, in
/// order, and each one resolves through [`find`].
#[test]
fn the_registry_carries_the_whitelisted_keys() {
    let keys: Vec<&str> = entries().iter().map(|entry| entry.key).collect();
    assert_eq!(
        keys,
        vec![
            KEY_INSTANCE,
            KEY_MRF,
            KEY_MRF_SIMPLE,
            KEY_MRF_KEYWORD,
            KEY_MRF_HELLTHREAD,
            KEY_MRF_OBJECT_AGE,
            KEY_MEDIA_PROXY,
            KEY_RESTRICT_UNAUTHENTICATED,
        ],
        "the ticket's whitelist, in the GET order"
    );
    assert!(
        entries().iter().all(|entry| entry.group == GROUP_PLEROMA),
        "every entry lives in :pleroma"
    );
    for entry in entries() {
        let found = find(entry.group, entry.key).expect("every registered pair resolves");
        assert_eq!(found.key, entry.key, "{} resolves to itself", entry.key);
    }
}

/// `find` refuses everything the whitelist does not carry, and the
/// refusal is what the API answers 400 for.
#[test]
fn unregistered_pairs_find_nothing() {
    assert!(find(GROUP_PLEROMA, ":feeds").is_none(), "not whitelisted");
    assert!(
        find(":wired", KEY_INSTANCE).is_none(),
        "wrong group is no pair"
    );
    assert!(
        find(GROUP_PLEROMA, "instance").is_none(),
        "the colon spelling is part of the key"
    );
    assert!(
        !requires_restart(GROUP_PLEROMA, ":nope"),
        "an unregistered pair restarts nothing"
    );
}

/// `:instance` and `:mrf` are subkey-whitelisted; the whole-value
/// entries are bare, and only the two entries no runtime consumer
/// reads are flagged for a restart.
#[test]
fn subkeys_and_restart_flags_follow_the_ticket() {
    let instance = find(GROUP_PLEROMA, KEY_INSTANCE).expect("registered");
    assert_eq!(
        instance.subkeys,
        &[
            "name",
            "description",
            "email",
            "registrations_open",
            "account_approval_required",
            "account_activation_required",
            "invites_enabled",
            "limit",
            "max_media_attachments",
        ],
        "the nine :instance subkeys"
    );
    let mrf = find(GROUP_PLEROMA, KEY_MRF).expect("registered");
    assert_eq!(mrf.subkeys, &["policies"], ":mrf gates its one subkey");
    for key in [
        KEY_MRF_SIMPLE,
        KEY_MRF_KEYWORD,
        KEY_MRF_HELLTHREAD,
        KEY_MRF_OBJECT_AGE,
    ] {
        let entry = find(GROUP_PLEROMA, key).expect("registered");
        assert!(entry.subkeys.is_empty(), "{key} is a whole value");
        assert!(
            !entry.requires_restart,
            "{key} takes effect without a restart"
        );
    }
    assert!(
        requires_restart(GROUP_PLEROMA, KEY_MEDIA_PROXY),
        "the media proxy shape is read at boot"
    );
    assert!(
        requires_restart(GROUP_PLEROMA, KEY_RESTRICT_UNAUTHENTICATED),
        "and the restriction set is read from the environment"
    );
    assert!(
        !requires_restart(GROUP_PLEROMA, KEY_INSTANCE) && !requires_restart(GROUP_PLEROMA, KEY_MRF),
        "the wired consumers need no restart"
    );
}

/// The `:instance` fallback spells the nine whitelisted subkeys off
/// the environment-derived config defaults.
#[test]
fn instance_defaults_carry_the_whitelisted_subkeys() {
    let config = Config::for_tests();
    let Value::Object(map) = instance_defaults(&config) else {
        panic!("the defaults are a keyword object");
    };
    assert_eq!(
        map.len(),
        instance_keys().len(),
        "exactly the whitelisted subkeys"
    );
    for subkey in instance_keys() {
        assert!(map.contains_key(*subkey), "{subkey} defaults present");
    }
    assert_eq!(map["name"], json!(config.instance_name), "name from env");
    assert_eq!(map["email"], json!(""), "unset email is empty");
    assert_eq!(map["registrations_open"], json!(true), "the env default");
    assert_eq!(
        map["limit"],
        json!(config.max_status_chars),
        "limit is the status length"
    );
}

/// The whitelist's `:instance` subkey list and the defaults' key set
/// are the same nine names, stated once.
fn instance_keys() -> &'static [&'static str] {
    find(GROUP_PLEROMA, KEY_INSTANCE)
        .expect("registered")
        .subkeys
}

/// The `:mrf` fallback is the `MRF_POLICIES` list; the bare entries
/// fall back to the empty object, and the two derived ones mirror
/// their env settings.
#[test]
fn the_other_defaults_follow_their_environment() {
    let mut config = Config::for_tests();
    config.mrf.policies = vec!["simple".to_owned()];
    assert_eq!(
        mrf_defaults(&config),
        json!({"policies": ["simple"]}),
        "the env chain"
    );
    assert_eq!(
        empty_defaults(&config),
        json!({}),
        "a bare entry defaults to nothing configured"
    );
    config.media_proxy.enabled = true;
    config.media_proxy.blacklist = vec!["evil.example".to_owned()];
    let Value::Object(proxy) = media_proxy_defaults(&config) else {
        panic!("the media proxy defaults are a keyword object");
    };
    assert_eq!(proxy["enabled"], json!(true));
    assert_eq!(proxy["blacklist"], json!(["evil.example"]));
    assert!(
        !proxy.contains_key("secret"),
        "the signing secret is never part of the store"
    );
    config.restrict_unauthenticated = std::iter::once("timelines_local".to_owned()).collect();
    let restricted = restrict_unauthenticated_defaults(&config);
    assert_eq!(
        restricted["timelines"]["local"],
        json!(true),
        "the flat name regroups under its half"
    );
    assert_eq!(
        restricted["timelines"]["federated"],
        json!(false),
        "and the siblings stay open"
    );
}

/// The `GET /config/descriptions` payload: one documented object per
/// registry entry in table order, the `:instance` element spelled
/// like the ticket's example, leaves without example values
/// rendering no `suggestions` field, and every subkey-whitelisted
/// entry documenting exactly its whitelist's children.
#[test]
fn descriptions_document_every_whitelisted_key() {
    let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
    let Value::Array(objects) = &rendered else {
        panic!("the payload is an array");
    };
    assert_eq!(objects.len(), entries().len(), "one object per entry");
    let keys: Vec<&str> = objects
        .iter()
        .map(|object| {
            assert_eq!(
                object["group"],
                json!(":pleroma"),
                "every entry is :pleroma"
            );
            assert!(
                object["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "no entry goes undocumented"
            );
            object["key"].as_str().expect("the key is a string")
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            KEY_INSTANCE,
            KEY_MRF,
            KEY_MRF_SIMPLE,
            KEY_MRF_KEYWORD,
            KEY_MRF_HELLTHREAD,
            KEY_MRF_OBJECT_AGE,
            KEY_MEDIA_PROXY,
            KEY_RESTRICT_UNAUTHENTICATED,
        ],
        "the ticket's order, the ticket's eight keys"
    );
}

/// The ticket's example element in full: `:instance` as a `group`
/// whose children carry `key`, `type`, `description` and the
/// `suggestions` hint only where the docs have one.
#[test]
fn the_instance_element_is_the_tickets_example() {
    let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
    let Value::Array(objects) = &rendered else {
        panic!("the payload is an array");
    };
    let instance = &objects[0];
    assert_eq!(instance["key"], json!(KEY_INSTANCE), "the example key");
    assert_eq!(instance["type"], json!("group"), "a documented container");
    assert_eq!(
        instance["description"],
        json!("Instance-related settings"),
        "Pleroma's `config/description.exs` wording"
    );
    let Value::Array(children) = &instance["children"] else {
        panic!("children is an array");
    };
    assert_eq!(children.len(), 9, "one child per whitelisted subkey");
    assert_eq!(children[0]["key"], json!("name"), "the example child");
    assert_eq!(children[0]["type"], json!("string"), "its type");
    assert_eq!(
        children[0]["description"],
        json!("Name of the instance"),
        "and its description"
    );
    assert_eq!(
        children[0]["suggestions"],
        json!(["Wired"]),
        "the example value, as the ticket's example shows one"
    );
    let registrations = &children[3];
    assert_eq!(registrations["key"], json!("registrations_open"));
    assert_eq!(registrations["type"], json!("boolean"), "booleans type");
    assert!(
        registrations.get("suggestions").is_none(),
        "a leaf without hints renders no suggestions field"
    );
}

/// The children track the whitelist: every subkey-whitelisted entry
/// documents exactly those subkeys; the whole-value policy entries
/// document their parameters, and the `policies` chain names the
/// registry's own type spelling `[:atom]` with the wired policies as
/// suggestions.
#[test]
fn children_follow_the_subkeys_and_the_policies() {
    for entry in entries() {
        let Value::Array(children) =
            serde_json::to_value(entry.children).expect("children serialize")
        else {
            panic!("children are an array");
        };
        assert!(!children.is_empty(), "{} carries child metadata", entry.key);
        if entry.subkeys.is_empty() {
            continue;
        }
        let documented: Vec<&str> = children
            .iter()
            .map(|child| child["key"].as_str().expect("a string key"))
            .collect();
        assert_eq!(
            documented, entry.subkeys,
            "{} documents its whitelisted subkeys",
            entry.key
        );
    }
    let mrf = find(GROUP_PLEROMA, KEY_MRF).expect("registered");
    assert_eq!(mrf.kind, ValueKind::Group, ":mrf is a documented group");
    let policies = &mrf.children[0];
    let rendered = serde_json::to_value(policies).expect("the child serializes");
    assert_eq!(
        rendered["type"],
        json!("[:atom]"),
        "the list-of-names spelling"
    );
    let suggestions = rendered["suggestions"].as_array().expect("suggestions");
    assert!(
        suggestions.contains(&json!("simple")) && suggestions.contains(&json!("keyword")),
        "the wired policy names are offered as examples"
    );
}

/// The nested `:map` documentation survives to the payload:
/// `:restrict_unauthenticated` groups its halves, and each half
/// nests its own booleans.
#[test]
fn nested_maps_carry_their_own_children() {
    let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
    let Value::Array(objects) = &rendered else {
        panic!("the payload is an array");
    };
    let restricted = &objects[7];
    assert_eq!(restricted["key"], json!(KEY_RESTRICT_UNAUTHENTICATED));
    assert_eq!(restricted["type"], json!("group"));
    let Value::Array(halves) = &restricted["children"] else {
        panic!("the halves are an array");
    };
    assert_eq!(halves.len(), 3, "timelines, profiles, activities");
    assert_eq!(halves[0]["key"], json!("timelines"));
    assert_eq!(halves[0]["type"], json!("map"), "a map of booleans");
    let Value::Array(fields) = &halves[0]["children"] else {
        panic!("the map nests its fields");
    };
    assert_eq!(fields.len(), 3, "local, federated and remote");
    assert_eq!(fields[0]["type"], json!("boolean"));
    assert!(
        halves[1].get("children").is_some() && halves[2].get("children").is_some(),
        "every documented container renders its children"
    );
}

/// The `type` strings stay inside the ticket's vocabulary everywhere
/// in the payload, at every nesting depth.
#[test]
fn every_type_is_a_ticket_spelling() {
    const VOCABULARY: [&str; 7] = [
        "string", "boolean", "integer", "[:atom]", "keyword", "map", "group",
    ];
    let rendered = serde_json::to_value(descriptions()).expect("the payload serializes");
    let Value::Array(objects) = &rendered else {
        panic!("the payload is an array");
    };
    let mut seen = Vec::new();
    for object in objects {
        seen.push(object["type"].as_str().expect("every entry has a type"));
        collect_child_types(&object["children"], &mut seen);
    }
    for name in &seen {
        assert!(VOCABULARY.contains(name), "{name} is not a ticket type");
    }
    assert_eq!(
        seen.first().copied(),
        Some("group"),
        "the example entry types as the example spells it"
    );
}

/// Walks one `children` array, adding each `type` to `seen`.
fn collect_child_types<'a>(children: &'a Value, seen: &mut Vec<&'a str>) {
    if let Value::Array(items) = children {
        for child in items {
            seen.push(child["type"].as_str().expect("every child has a type"));
            collect_child_types(&child["children"], seen);
        }
    }
}
