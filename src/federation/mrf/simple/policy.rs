//! The chain stage itself: the refusals [`SimplePolicy`] draws from the
//! domain lists of its [`SimpleConfig`], the rewrites it applies to what it
//! admits, and the `[SimplePolicy]` wording a rejection carries.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::federation::mrf::{MrfContext, MrfPolicy, MrfResult, Transparency};

use super::{
    ACTOR_FIELDS, DELETE_TYPE, FLAG_TYPE, SimpleConfig, SimplePolicy,
    lists::{List, Reason},
    rewrite::{
        activity_host, activity_type, actor_document_host, followers_collection, has_attachments,
        is_note_activity, limit_document, note_object_mut, object_reference_host,
        rewrite_each_document, unlist_document,
    },
};

impl SimplePolicy {
    /// The policy over the compiled `simple` configuration `config`.
    #[must_use]
    pub const fn new(config: Arc<SimpleConfig>) -> Self {
        Self { config }
    }

    /// Why the lists refuse `activity` from `host`, `None` when they admit
    /// it. `local` is this instance's own domain, which the allowlist
    /// never excludes.
    fn refusal(&self, host: &str, local: &str, activity: &Value) -> Option<String> {
        self.domain_refusal(host, local)
            .or_else(|| self.type_refusal(host, activity))
    }

    /// The refusals every activity of the host meets: its own `reject`
    /// entry, or an `accept` allowlist that does not name it.
    fn domain_refusal(&self, host: &str, local: &str) -> Option<String> {
        let config = self.config.as_ref();
        if let Some(reason) = config.reason(List::Reject, host) {
            return Some(refusal_text(List::Reject, host, Some(reason)));
        }
        let listed = host.eq_ignore_ascii_case(local) || config.matches(List::Accept, host);
        if config.configured(List::Accept) && !listed {
            return Some(refusal_text(List::Accept, host, None));
        }
        None
    }

    /// The refusals that depend on what the activity is: `Delete` under
    /// `reject_deletes`, `Flag` under `report_removal`.
    fn type_refusal(&self, host: &str, activity: &Value) -> Option<String> {
        match activity_type(activity).as_deref()? {
            DELETE_TYPE => self.refusal_for(List::RejectDeletes, host),
            FLAG_TYPE => self.refusal_for(List::ReportRemoval, host),
            _ => None,
        }
    }

    /// The refusal `list` produces for `host`, `None` when the list does
    /// not name it.
    fn refusal_for(&self, list: List, host: &str) -> Option<String> {
        self.config
            .reason(list, host)
            .map(|reason| refusal_text(list, host, Some(reason)))
    }

    /// The rewrites the lists apply to an accepted activity: the
    /// attachments first — and `media_nsfw` reads them before
    /// `media_removal` has taken them away, so a host on both lists gets
    /// its note stripped *and* marked.
    fn rewrite(&self, mut activity: Value, host: &str) -> Value {
        self.rewrite_media(&mut activity, host);
        self.rewrite_addressing(&mut activity, host);
        activity
    }

    /// `media_removal` strips a note's `attachment` entries; `media_nsfw`
    /// marks a note that had any `sensitive`.
    fn rewrite_media(&self, activity: &mut Value, host: &str) {
        let strip = self.config.matches(List::MediaRemoval, host);
        let mark = self.config.matches(List::MediaNsfw, host);
        if !strip && !mark {
            return;
        }
        let Some(object) = note_object_mut(activity) else {
            return;
        };
        let had_media = has_attachments(object);
        if strip {
            let _gone = object.remove("attachment");
        }
        if mark && had_media {
            let _previous = object.insert("sensitive".to_owned(), Value::Bool(true));
        }
    }

    /// `federated_timeline_removal` unlists and `followers_only` limits a
    /// note's addressing, on the activity and on the object it embeds:
    /// peers spell one post's recipients in both places, this instance
    /// reads the object back when it folds the note and the activity when
    /// it would send one on.
    fn rewrite_addressing(&self, activity: &mut Value, host: &str) {
        let unlist = self.config.matches(List::FederatedTimelineRemoval, host);
        let limited = self.config.matches(List::FollowersOnly, host);
        if !unlist && !limited {
            return;
        }
        if !is_note_activity(activity) {
            return;
        }
        let followers = followers_collection(activity);
        if unlist {
            rewrite_each_document(activity, unlist_document);
        }
        if limited {
            rewrite_each_document(activity, |document| limit_document(document, &followers));
        }
    }
}

impl MrfPolicy for SimplePolicy {
    fn name(&self) -> &'static str {
        "SimplePolicy"
    }

    fn filter(&self, ctx: &MrfContext, activity: Value) -> MrfResult {
        let Some(host) = activity_host(&activity) else {
            // Nothing to compare the lists against: Pleroma's catch-all
            // clause hands the document on.
            return Ok(activity);
        };
        if let Some(reason) = self.refusal(&host, &ctx.local_domain, &activity) {
            return Err(reason);
        }
        // Pleroma's `check_object`: the host the activity points at is
        // judged by the domain lists too, but not by the type refusals.
        if let Some(object_host) = object_reference_host(&activity)
            && let Some(reason) = self.domain_refusal(&object_host, &ctx.local_domain)
        {
            return Err(reason);
        }
        Ok(self.rewrite(activity, &host))
    }

    fn filter_actor(&self, _ctx: &MrfContext, actor: Value) -> Value {
        let Some(host) = actor_document_host(&actor) else {
            return actor;
        };
        let mut actor = actor;
        for (list, field) in ACTOR_FIELDS {
            if !self.config.matches(list, &host) {
                continue;
            }
            if let Some(document) = actor.as_object_mut() {
                let _gone = document.remove(field);
            }
        }
        actor
    }

    fn describe(&self, transparency: &Transparency) -> Map<String, Value> {
        self.config.describe(transparency)
    }
}

/// The `[SimplePolicy]` wording one list hit produces, the entry's own
/// reason (when it has one) beside it.
fn refusal_text(list: List, host: &str, reason: Option<&Reason>) -> String {
    let relation = if list == List::Accept {
        "is not on"
    } else {
        "is on"
    };
    let suffix = reason.map_or_default(Reason::suffix);
    format!(
        "[SimplePolicy] the host {host} {relation} the `{}` list{suffix}",
        list.key()
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::mrf::simple::test_support::*;

    /// [`List::Reject`]: the host it names is refused, reason and all, and
    /// a host below a wildcard with it.
    #[test]
    fn the_reject_list_refuses_everything_a_matching_host_sends() {
        let named = json!({"reject": [["peer.example", "bots"]]});
        for activity in [create(), delete(), flag(), follow()] {
            assert_eq!(
                filtered(&named, activity.clone()),
                Err(format!(
                    "[SimplePolicy] the host {PEER} is on the `reject` list (bots)"
                )),
                "a `reject`ed host sends nothing, not even a deletion: {:?}",
                activity["type"]
            );
        }
        assert_eq!(
            filtered(&json!({"reject": ["*.example"]}), create()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is on the `reject` list"
            )),
            "the wildcard covers the peer, and an entry without a reason says nothing"
        );
        assert_eq!(
            filtered(&json!({"reject": ["other.example"]}), create()),
            Ok(create()),
            "another host is untouched"
        );
    }

    /// [`List::Accept`]: once the allowlist holds anything, only its hosts
    /// — and this instance itself — get through.
    #[test]
    fn the_accept_list_federates_with_nobody_but_itself() {
        let lists = json!({"accept": ["friend.example", "*.partner.example"]});
        assert_eq!(
            filtered(&lists, create()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is not on the `accept` list"
            )),
            "a host the allowlist does not name is refused"
        );
        assert_eq!(
            filtered(&lists, create_from("cdn.partner.example")),
            Ok(create_from("cdn.partner.example")),
            "a wildcard of the allowlist admits what is below it, unchanged"
        );
        assert_eq!(
            filtered(&lists, create_from(LOCAL)),
            Ok(create_from(LOCAL)),
            "and this instance's own domain is never excluded by it"
        );
        assert_eq!(
            filtered(&json!({"accept": []}), create()),
            Ok(create()),
            "an empty allowlist is no allowlist at all"
        );
    }

    /// [`List::RejectDeletes`]: deletions go, the rest of the host's
    /// traffic stays.
    #[test]
    fn reject_deletes_drops_only_deletions() {
        let lists = json!({"reject_deletes": ["peer.example"]});
        assert_eq!(
            filtered(&lists, delete()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is on the `reject_deletes` list"
            )),
            "the deletion of a host on the list is refused"
        );
        for (kind, activity) in [("Create", create()), ("Flag", flag()), ("Follow", follow())] {
            assert_eq!(
                filtered(&lists, activity.clone()),
                Ok(activity),
                "everything but a deletion from that host passes: {kind}"
            );
        }
    }

    /// [`List::ReportRemoval`]: reports go, the rest of the host's traffic
    /// stays.
    #[test]
    fn report_removal_drops_only_reports() {
        let lists = json!({"report_removal": [["peer.example", "spam reports"]]});
        assert_eq!(
            filtered(&lists, flag()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is on the `report_removal` list (spam reports)"
            )),
            "a flag from a host on the list is refused, with the operator's reason"
        );
        assert_eq!(
            filtered(&lists, create()),
            Ok(create()),
            "and its notes are still folded"
        );
    }

    /// The object reference is judged by the domain lists too: an
    /// `Announce` or `Like` by an admitted host of a post on a `reject`ed
    /// one is refused, naming the rejected host.
    #[test]
    fn an_object_on_a_rejected_host_is_refused() {
        let lists = json!({"reject": [["evil.example", "bots"]]});
        for kind in ["Announce", "Like"] {
            assert_eq!(
                filtered(&lists, reacting(kind, "https://evil.example/notes/1")),
                Err(
                    "[SimplePolicy] the host evil.example is on the `reject` list (bots)"
                        .to_owned()
                ),
                "{kind} of a rejected host's post is refused"
            );
        }
        let allowed = reacting("Announce", "https://good.example/notes/1");
        assert_eq!(
            filtered(&lists, allowed.clone()),
            Ok(allowed),
            "an Announce of an unlisted host's post passes"
        );
    }

    /// An embedded object with an `actor` (an `Undo`'s `Follow`) is judged
    /// by that actor's host, an embedded actor document by its own `id`'s,
    /// and any other embedded object — a note — by nobody's.
    #[test]
    fn an_embedded_object_is_judged_by_its_actor_or_its_own_identity() {
        let lists = json!({"reject": ["evil.example"]});
        let undo = json!({
            "id": format!("https://{PEER}/activities/8"),
            "type": "Undo",
            "actor": ALICE,
            "object": {
                "type": "Follow",
                "actor": "https://evil.example/users/mallory",
                "object": ALICE,
            },
        });
        assert_eq!(
            filtered(&lists, undo),
            Err("[SimplePolicy] the host evil.example is on the `reject` list".to_owned()),
            "the embedded Follow's actor is on a rejected host"
        );
        for kind in ["Person", "Service", "Group", "Organization", "Application"] {
            let update = json!({
                "id": format!("https://{PEER}/activities/9"),
                "type": "Update",
                "actor": ALICE,
                "object": {"id": "https://evil.example/users/mallory", "type": kind},
            });
            assert_eq!(
                filtered(&lists, update),
                Err("[SimplePolicy] the host evil.example is on the `reject` list".to_owned()),
                "an embedded {kind} document is judged by its own id"
            );
        }
        let by_array_type = json!({
            "id": format!("https://{PEER}/activities/10"),
            "type": "Update",
            "actor": ALICE,
            "object": {"id": "https://evil.example/users/mallory", "type": ["Person", "Extra"]},
        });
        assert!(
            filtered(&lists, by_array_type).is_err(),
            "the first of several types decides"
        );
        let mut foreign_note = create();
        foreign_note["object"]["attributedTo"] = json!("https://evil.example/users/mallory");
        foreign_note["object"]["id"] = json!("https://evil.example/objects/1");
        assert_eq!(
            filtered(&lists, foreign_note.clone()),
            Ok(foreign_note),
            "an embedded note has no actor and is no actor: Pleroma lets it by"
        );
    }

    /// The object's host meets the domain lists only: a `reject_deletes`
    /// or `report_removal` entry for it does not refuse the activity.
    #[test]
    fn the_object_host_meets_no_type_refusals() {
        let lists = json!({
            "reject_deletes": ["evil.example"],
            "report_removal": ["evil.example"],
        });
        for kind in ["Delete", "Flag"] {
            let activity = reacting(kind, "https://evil.example/notes/1");
            assert_eq!(
                filtered(&lists, activity.clone()),
                Ok(activity),
                "{kind} of a post there is not refused: the lists name the sender"
            );
        }
    }

    /// With an allowlist, an object on an unlisted host is refused, and one
    /// on this instance or a listed host passes.
    #[test]
    fn the_accept_list_covers_the_object_host() {
        let lists = json!({"accept": ["peer.example", "friend.example"]});
        assert_eq!(
            filtered(
                &lists,
                reacting("Announce", "https://stranger.example/notes/1")
            ),
            Err("[SimplePolicy] the host stranger.example is not on the `accept` list".to_owned()),
            "an unlisted host's post is refused"
        );
        for object in [
            format!("https://{LOCAL}/notes/1"),
            "https://friend.example/notes/1".to_owned(),
        ] {
            let activity = reacting("Announce", &object);
            assert_eq!(
                filtered(&lists, activity.clone()),
                Ok(activity),
                "the local domain and listed hosts pass: {object}"
            );
        }
    }

    /// The actor stage: `avatar_removal`, `banner_removal` and
    /// `background_removal` take the named field out of a matching host's
    /// document, each on its own.
    #[test]
    fn the_actor_lists_strip_their_fields_from_a_profile() {
        let avatar = filtered_actor(&json!({"avatar_removal": [PEER]}), person(PEER));
        assert!(avatar.get("icon").is_none(), "the avatar is gone: {avatar}");
        assert_eq!(avatar["image"], person(PEER)["image"], "the banner stays");
        assert_eq!(
            avatar["background"],
            person(PEER)["background"],
            "and the background"
        );

        let banner = filtered_actor(&json!({"banner_removal": ["*.example"]}), person(PEER));
        assert!(
            banner.get("image").is_none(),
            "a wildcard covers the host: {banner}"
        );
        assert_eq!(
            banner["icon"],
            person(PEER)["icon"],
            "and only its own field goes"
        );

        let background = filtered_actor(
            &json!({"background_removal": [[PEER, "lewd headers"]]}),
            person(PEER),
        );
        assert!(
            background.get("background").is_none(),
            "the background is gone: {background}"
        );

        assert_eq!(
            filtered_actor(&json!({"avatar_removal": ["other.example"]}), person(PEER)),
            person(PEER),
            "another host keeps its profile images"
        );
        let no_lists = filtered_actor(&json!({}), person(PEER));
        assert_eq!(
            no_lists,
            person(PEER),
            "and with nothing configured nothing goes"
        );
    }

    /// A document the lists have nothing to say about passes through
    /// unchanged — including one that names no actor at all.
    #[test]
    fn a_document_the_lists_do_not_name_passes_through() {
        let lists = json!({"reject": ["other.example"], "media_removal": ["other.example"]});
        for activity in [
            create(),
            follow(),
            delete(),
            json!({"type": "Note", "content": "an object, not an activity"}),
            json!({"id": format!("https://{PEER}/activities/9"), "type": "Create", "object": NOTE}),
        ] {
            assert_eq!(
                filtered(&lists, activity.clone()),
                Ok(activity),
                "an unconfigured host and an actor-less document both pass"
            );
        }
    }

    /// The policy is Pleroma's `SimplePolicy`, and its transparency blocks
    /// are `mrf_simple` — all eleven lists, the configured ones holding
    /// their patterns verbatim — beside `mrf_simple_info`, the reasons the
    /// entries carry.
    #[test]
    fn the_policy_names_itself_and_describes_its_lists() {
        let lists = json!({
            "reject": [["evil.example", "bots"], "*.worse.example"],
            "accept": ["friend.example"],
        });
        let policy = policy(&lists);

        assert_eq!(policy.name(), "SimplePolicy", "Pleroma's module name");
        let described = policy.describe(&Transparency::open());
        assert_eq!(
            described["mrf_simple"]["reject"],
            json!(["evil.example", "*.worse.example"]),
            "the patterns come out as configured, wildcard verbatim, in order"
        );
        assert_eq!(
            described["mrf_simple"]["accept"],
            json!(["friend.example"]),
            "the allowlist is published too"
        );
        assert_eq!(
            described["mrf_simple_info"],
            json!({"reject": {"evil.example": {"reason": "bots"}}}),
            "the reasons ride in the separate object Pleroma keeps them in, \
             for the one list whose entries carry one"
        );
        let published = described["mrf_simple"]
            .as_object()
            .expect("the block is an object");
        assert_eq!(
            published.len(),
            List::ALL.len(),
            "every list is published, empty ones as the empty array, the way \
             Pleroma maps its whole `:mrf_simple` config"
        );
        for list in List::ALL {
            assert!(
                published.contains_key(list.key()),
                "`{}` names one of the published keys",
                list.key()
            );
        }
    }
}
