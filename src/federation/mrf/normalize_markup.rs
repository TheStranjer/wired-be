//! Pleroma's `NormalizeMarkupPolicy`: the sanitizer on the wire.
//!
//! The reference is
//! `lib/pleroma/web/activity_pub/mrf/normalize_markup_policy.ex` — the
//! stage that runs a note's `content` through the instance's markup
//! pipeline before anything else looks at it. Here that pipeline is
//! [`sanitize_html`], the same
//! allowlist the note parser and the profile bio already share: a remote
//! document's markup arrives in the shape this instance stores and serves,
//! with scripts, event handlers and disallowed tags already gone rather
//! than already stored.
//!
//! ```json
//! {"policies": ["normalize_markup"]}
//! ```
//!
//! The policy acts on the embedded note of a `Create` or `Update` — the
//! same shape the [`simple`](super::simple) policy's content rules speak
//! about — and only on a `content` spelled as one string: a multi-type
//! content array is left for its own ticket. Like every stage of the
//! [chain](super) it runs on the way in AND on the way out, so a locally
//! rendered document passes the same normalization before delivery.
//!
//! The policy reads no [`MrfContext`] facts; the `normalize_markup` key of
//! the setting is only echoed back through
//! [`describe`](MrfPolicy::describe).

use serde_json::{Map, Value, json};

use crate::{
    federation::mrf::{
        MrfContext, MrfPolicy, MrfResult, Transparency, simple::note_object_mut, single_block,
    },
    util::sanitize::sanitize_html,
};

/// The key of this policy's parameters inside the `mrf` instance setting,
/// and the name [`build_chain`](super::build_chain) resolves.
pub const PARAMS_KEY: &str = "normalize_markup";

/// The markup-normalizing stage: Pleroma's `MRF.NormalizeMarkupPolicy`.
#[derive(Clone, Debug)]
pub struct NormalizeMarkupPolicy {
    /// The parameter block this policy was configured with, for
    /// [`describe`](MrfPolicy::describe); the filter itself reads nothing.
    params: Option<Value>,
}

impl NormalizeMarkupPolicy {
    /// The policy beside the `normalize_markup` key of the `mrf` setting
    /// (`None` when the setting carries none).
    #[must_use]
    pub const fn new(params: Option<Value>) -> Self {
        Self { params }
    }
}

impl MrfPolicy for NormalizeMarkupPolicy {
    fn name(&self) -> &'static str {
        "NormalizeMarkupPolicy"
    }

    fn filter(&self, _ctx: &MrfContext, mut activity: Value) -> MrfResult {
        if let Some(object) = note_object_mut(&mut activity)
            && let Some(Value::String(content)) = object.get_mut("content")
        {
            let cleaned = sanitize_html(content);
            *content = cleaned;
        }
        Ok(activity)
    }

    fn describe(&self, _transparency: &Transparency) -> Map<String, Value> {
        single_block(
            self.name(),
            self.params.clone().unwrap_or_else(|| json!({})),
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::Map;
    use std::sync::Arc;

    use super::*;
    use crate::federation::mrf::{MrfConfig, simple::PUBLIC_ADDRESS};

    /// The author the fixtures post as.
    const ALICE: &str = "https://peer.example/users/alice";
    /// The note the fixture `Create` announces.
    const NOTE: &str = "https://peer.example/objects/1";
    /// The public address, full `ActivityStreams` spelling.
    const PUBLIC: &str = PUBLIC_ADDRESS;

    /// The context of an empty run: nothing this policy reads.
    fn ctx() -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(
                vec![PARAMS_KEY.to_owned()],
                Map::from_iter([(PARAMS_KEY.to_owned(), json!({}))]),
            )),
            now: Utc::now(),
            local_domain: "test.example".to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        }
    }

    /// The policy over an empty parameter block.
    fn policy() -> NormalizeMarkupPolicy {
        NormalizeMarkupPolicy::new(Some(json!({})))
    }

    /// A `Create` of a note whose `content` is `content`, by alice.
    fn create_with(content: &str) -> Value {
        json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "content": content,
            },
        })
    }

    /// The golden in and out: hostile markup leaves the stage as exactly
    /// what `sanitize_html` makes of it, everything else verbatim.
    #[test]
    fn a_notes_content_is_replaced_by_its_sanitized_form() {
        let raw = create_with(
            "<p>hi</p><script>alert(1)</script><img src=\"https://x.example/e.png\" onerror=\"alert(1)\">",
        );

        let accepted = policy()
            .filter(&ctx(), raw)
            .expect("normalization never rejects");

        let expected = json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "content": "<p>hi</p><img src=\"https://x.example/e.png\">",
            },
        });
        assert_eq!(
            accepted, expected,
            "the script is gone with its contents, the handler attribute goes, the note stands"
        );
        assert_eq!(
            accepted["object"]["content"],
            sanitize_html(
                "<p>hi</p><script>alert(1)</script><img src=\"https://x.example/e.png\" onerror=\"alert(1)\">"
            ),
            "and what stands is the sanitizer's own output"
        );
    }

    /// Clean-enough markup still comes back rewritten the sanitizer's way:
    /// forced `rel` on links, preserved mention classes, dropped `target`.
    #[test]
    fn a_clean_note_is_rewritten_the_sanitizer_way() {
        let raw = create_with(
            r#"<p>see <a href="https://example.org" target="_blank">this</a> and <a href="https://test.example/users/bob" class="mention">@bob</a></p>"#,
        );

        let accepted = policy()
            .filter(&ctx(), raw)
            .expect("normalization never rejects");

        assert_eq!(
            accepted["object"]["content"],
            json!(
                r#"<p>see <a href="https://example.org" rel="ugc nofollow noopener noreferrer">this</a> and <a href="https://test.example/users/bob" class="mention" rel="ugc nofollow noopener noreferrer">@bob</a></p>"#
            ),
            "`target` goes, `rel` is forced, the mention class stays"
        );
    }

    /// Everything outside the note shape passes untouched: no embedded
    /// object, no `content` key, a content spelled as something but a
    /// string, and an activity type the stage does not speak about.
    #[test]
    fn documents_without_a_string_note_content_pass_unchanged() {
        for activity in [
            json!({
                "id": "https://peer.example/activities/4",
                "type": "Follow",
                "actor": ALICE,
                "object": "https://test.example/users/bob",
            }),
            json!({
                "id": "https://peer.example/activities/2",
                "type": "Create",
                "actor": ALICE,
                "object": NOTE,
            }),
            json!({
                "id": "https://peer.example/activities/3",
                "type": "Create",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note" },
            }),
            json!({
                "id": "https://peer.example/activities/5",
                "type": "Create",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "content": ["text/html", "<b>x</b>"] },
            }),
        ] {
            assert_eq!(
                policy().filter(&ctx(), activity.clone()),
                Ok(activity),
                "there is nothing here to normalize"
            );
        }
    }

    /// An `Update` is normalized exactly like a `Create`.
    #[test]
    fn updates_are_normalized_like_creates() {
        let update = json!({
            "id": "https://peer.example/activities/6",
            "type": "Update",
            "actor": ALICE,
            "object": { "id": NOTE, "type": "Note", "content": "<i>x</i><script>y</script>" },
        });

        let accepted = policy()
            .filter(&ctx(), update)
            .expect("normalization never rejects");

        assert_eq!(
            accepted["object"]["content"],
            json!("<i>x</i>"),
            "the script goes with its contents, the emphasis stands"
        );
    }

    /// The policy is Pleroma's `NormalizeMarkupPolicy`: it names itself and
    /// publishes the subset it was configured with as its one
    /// `mrf_normalizemarkup` object — the empty object when nothing was.
    #[test]
    fn the_policy_names_itself_and_describes_its_config() {
        assert_eq!(
            NormalizeMarkupPolicy::new(None).name(),
            "NormalizeMarkupPolicy"
        );
        assert_eq!(
            Value::Object(NormalizeMarkupPolicy::new(None).describe(&Transparency::open())),
            json!({"mrf_normalizemarkup": {}}),
            "no key, nothing to describe but its own name"
        );
        assert_eq!(
            Value::Object(
                NormalizeMarkupPolicy::new(Some(json!({"echo": true})))
                    .describe(&Transparency::open())
            ),
            json!({"mrf_normalizemarkup": {"echo": true}}),
            "the configured subset comes back verbatim under the policy's key"
        );
    }
}
