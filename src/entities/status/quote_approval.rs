//! The `quote_approval` key: who may quote a status, and whether the
//! viewer may (Mastodon 4.5's `REST::QuoteApprovalSerializer`).
//!
//! `automatic` and `manual` name the audiences whose quotes are accepted
//! at once and the ones that wait for the author, as `public`,
//! `followers`, `following` or `unsupported_policy`. `current_user` is the
//! answer for this viewer: `automatic`, `manual`, `denied`, or `unknown`
//! when it cannot be told (an anonymous viewer, or a remote post that
//! advertised no policy). Clients use it to enable the Quote button.
//!
//! A local status advertises what its visibility allows
//! ([`quote_approval`](crate::federation::quotes::quote_approval)); a
//! remote one what its note's `interactionPolicy.canQuote` listed, as the
//! ingest stored it.

use serde::Serialize;

use crate::{
    entities::{
        account::AccountViewer,
        relationship::{Relationship, RelationshipMap},
    },
    federation::{
        quotes::{QuoteApproval as Advertised, quote_approval},
        vocab::AS_PUBLIC,
    },
    models::{
        status::Status as StatusRow,
        user::{User, followers_address, following},
    },
};

/// One audience of a quote policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Audience {
    /// Anybody.
    Public,
    /// The author's followers.
    Followers,
    /// The accounts the author follows.
    Following,
    /// An address this instance does not understand.
    Unsupported,
}

impl Audience {
    /// The wire word Mastodon uses for this audience.
    const fn key(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Followers => "followers",
            Self::Following => "following",
            Self::Unsupported => "unsupported_policy",
        }
    }

    /// Whether the viewer belongs to this audience of the author, given
    /// the viewer's relationship to the author.
    const fn admits(self, standing: Option<&Relationship>) -> bool {
        match (self, standing) {
            (Self::Public, _) => true,
            (Self::Followers, Some(relationship)) => relationship.following.0,
            (Self::Following, Some(relationship)) => relationship.followed_by.0,
            (Self::Followers | Self::Following, None) | (Self::Unsupported, _) => false,
        }
    }
}

/// The audiences a status advertises, split by how a quote is approved.
#[derive(Debug, Default)]
struct Policy {
    /// Audiences whose quotes are accepted without asking.
    automatic: Vec<Audience>,
    /// Audiences whose quotes wait for the author.
    manual: Vec<Audience>,
}

/// The Mastodon `QuoteApproval` entity of one status, as one viewer sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuoteApproval {
    /// Audiences whose quotes are accepted automatically.
    pub automatic: Vec<&'static str>,
    /// Audiences whose quotes need the author's approval.
    pub manual: Vec<&'static str>,
    /// What this viewer may do: `automatic`, `manual`, `denied` or
    /// `unknown`.
    pub current_user: &'static str,
}

impl QuoteApproval {
    /// The approval of `row` by `author`, as `viewer` sees it.
    ///
    /// `relationships` is the viewer's preloaded relationship to the page's
    /// authors; without one the viewer counts as unconnected to the author.
    #[must_use]
    pub fn for_row(
        row: &StatusRow,
        author: &User,
        viewer: AccountViewer<'_>,
        relationships: Option<&RelationshipMap>,
    ) -> Self {
        let policy = advertised(row, author);
        let standing = relationships.and_then(|map| map.get(&author.id));
        let current_user = current_user(policy.as_ref(), author, viewer, standing);
        let (automatic, manual) = policy.as_ref().map_or((Vec::new(), Vec::new()), |found| {
            (keys(&found.automatic), keys(&found.manual))
        });
        Self {
            automatic,
            manual,
            current_user,
        }
    }
}

/// The wire words of `audiences`, in order.
fn keys(audiences: &[Audience]) -> Vec<&'static str> {
    audiences.iter().copied().map(Audience::key).collect()
}

/// The policy `row` advertises; `None` for a remote status whose note
/// advertised nothing.
fn advertised(row: &StatusRow, author: &User) -> Option<Policy> {
    if row.local {
        let automatic = match quote_approval(row.visibility) {
            Advertised::Everyone => vec![Audience::Public],
            Advertised::Followers => vec![Audience::Followers],
            Advertised::Nobody => Vec::new(),
        };
        return Some(Policy {
            automatic,
            manual: Vec::new(),
        });
    }
    let stored = &row.quote_policy.as_ref()?.0;
    Some(Policy {
        automatic: classify_all(&stored.automatic, author),
        manual: classify_all(&stored.manual, author),
    })
}

/// The audiences `addresses` name for `author`, each once; the author's own
/// address is Mastodon's spelling of "only me" and names no audience.
fn classify_all(addresses: &[String], author: &User) -> Vec<Audience> {
    let followers = followers_address(author);
    let following_address = author
        .following_url
        .clone()
        .unwrap_or_else(|| following(&author.ap_id));
    let mut audiences: Vec<Audience> = Vec::new();
    for address in addresses {
        let audience = if address == AS_PUBLIC {
            Audience::Public
        } else if *address == followers {
            Audience::Followers
        } else if *address == following_address {
            Audience::Following
        } else if *address == author.ap_id {
            continue;
        } else {
            Audience::Unsupported
        };
        if !audiences.contains(&audience) {
            audiences.push(audience);
        }
    }
    audiences
}

/// What `viewer` may do with `policy`: the author always may, an anonymous
/// viewer or a policy nobody advertised cannot be told, and everyone else
/// is measured against the automatic and then the manual audiences.
fn current_user(
    policy: Option<&Policy>,
    author: &User,
    viewer: AccountViewer<'_>,
    standing: Option<&Relationship>,
) -> &'static str {
    let Some(who) = viewer else {
        return "unknown";
    };
    if who.id == author.id {
        return "automatic";
    }
    let Some(found) = policy else {
        return "unknown";
    };
    if found
        .automatic
        .iter()
        .any(|audience| audience.admits(standing))
    {
        "automatic"
    } else if found
        .manual
        .iter()
        .any(|audience| audience.admits(standing))
    {
        "manual"
    } else {
        "denied"
    }
}

#[cfg(test)]
mod tests {
    use sqlx::types::Json;

    use super::*;
    use crate::{
        entities::status::test_support::{base_row, base_user, other_user},
        models::status::{QuotePolicy, Visibility},
    };

    /// The remote author's actor id.
    const REMOTE_AUTHOR: &str = "https://remote.example/users/wanda";
    /// That author's followers collection.
    const REMOTE_FOLLOWERS: &str = "https://remote.example/users/wanda/followers";
    /// A second account id, for the viewer.
    const VIEWER_ID: &str = "01934567-89ab-7cde-f012-3456789abcee";

    /// The viewer account.
    fn a_viewer() -> User {
        other_user("viewer", VIEWER_ID)
    }

    /// A remote author named `wanda`.
    fn remote_author() -> User {
        let mut author = base_user("wanda");
        author.local = crate::models::user::Flag::new(false);
        author.ap_id = REMOTE_AUTHOR.to_owned();
        author.followers_url = Some(REMOTE_FOLLOWERS.to_owned());
        author
    }

    /// A remote row advertising `policy`.
    fn remote_row(policy: Option<QuotePolicy>) -> StatusRow {
        let mut row = base_row();
        row.local = false;
        row.quote_policy = policy.map(Json);
        row
    }

    /// A map holding a viewer who follows `author`.
    fn follower_of(author: &User) -> RelationshipMap {
        let mut relationship = Relationship::unconnected(author.id);
        relationship.following.0 = true;
        RelationshipMap::from([(author.id, relationship)])
    }

    /// A map holding a viewer whom `author` follows.
    fn followed_by(author: &User) -> RelationshipMap {
        let mut relationship = Relationship::unconnected(author.id);
        relationship.followed_by.0 = true;
        RelationshipMap::from([(author.id, relationship)])
    }

    /// A policy with `automatic` and `manual` approvers.
    fn policy(automatic: &[&str], manual: &[&str]) -> QuotePolicy {
        QuotePolicy {
            automatic: automatic.iter().map(|text| (*text).to_owned()).collect(),
            manual: manual.iter().map(|text| (*text).to_owned()).collect(),
        }
    }

    #[test]
    fn a_public_local_status_is_automatic_for_another_account() {
        let author = base_user("alice");
        let viewer = a_viewer();
        let approval = QuoteApproval::for_row(&base_row(), &author, Some(&viewer), None);
        assert_eq!(
            serde_json::to_value(&approval).unwrap(),
            serde_json::json!({"automatic": ["public"], "manual": [], "current_user": "automatic"}),
            "the policy the note advertises, in Mastodon's key order"
        );
    }

    #[test]
    fn a_private_local_status_admits_only_followers() {
        let author = base_user("alice");
        let viewer = a_viewer();
        let mut row = base_row();
        row.visibility = Visibility::Private;

        let stranger = QuoteApproval::for_row(&row, &author, Some(&viewer), None);
        assert_eq!(stranger.automatic, ["followers"]);
        assert_eq!(stranger.manual, Vec::<&str>::new());
        assert_eq!(stranger.current_user, "denied", "a non-follower");

        let map = follower_of(&author);
        let follower = QuoteApproval::for_row(&row, &author, Some(&viewer), Some(&map));
        assert_eq!(follower.current_user, "automatic", "a follower");
    }

    #[test]
    fn a_direct_local_status_advertises_nobody() {
        let author = base_user("alice");
        let viewer = a_viewer();
        let mut row = base_row();
        row.visibility = Visibility::Direct;
        let approval = QuoteApproval::for_row(&row, &author, Some(&viewer), None);
        assert!(approval.automatic.is_empty() && approval.manual.is_empty());
        assert_eq!(approval.current_user, "denied");
        let own = QuoteApproval::for_row(&row, &author, Some(&author), None);
        assert_eq!(own.current_user, "automatic", "the author may always quote");
    }

    #[test]
    fn an_anonymous_viewer_cannot_be_told() {
        let author = base_user("alice");
        let approval = QuoteApproval::for_row(&base_row(), &author, None, None);
        assert_eq!(approval.automatic, ["public"], "the policy still shows");
        assert_eq!(approval.current_user, "unknown");
    }

    #[test]
    fn a_remote_followers_policy_depends_on_the_viewer() {
        let author = remote_author();
        let viewer = a_viewer();
        let row = remote_row(Some(policy(&[REMOTE_FOLLOWERS], &[])));

        let outsider = QuoteApproval::for_row(&row, &author, Some(&viewer), None);
        assert_eq!(outsider.automatic, ["followers"]);
        assert_eq!(outsider.current_user, "denied", "a non-follower");

        let map = follower_of(&author);
        let follower = QuoteApproval::for_row(&row, &author, Some(&viewer), Some(&map));
        assert_eq!(follower.current_user, "automatic", "a follower");
    }

    #[test]
    fn manual_approval_and_the_following_collection_are_read() {
        let author = remote_author();
        let viewer = a_viewer();
        let row = remote_row(Some(policy(
            &[AS_PUBLIC],
            &["https://remote.example/users/wanda/following"],
        )));
        let approval = QuoteApproval::for_row(&row, &author, Some(&viewer), None);
        assert_eq!(approval.automatic, ["public"]);
        assert_eq!(approval.manual, ["following"]);

        let manual_only = remote_row(Some(policy(
            &[],
            &["https://remote.example/users/wanda/following"],
        )));
        let followed = followed_by(&author);
        let asked = QuoteApproval::for_row(&manual_only, &author, Some(&viewer), Some(&followed));
        assert_eq!(asked.current_user, "manual", "someone the author follows");
    }

    #[test]
    fn an_unknown_address_is_an_unsupported_policy() {
        let author = remote_author();
        let viewer = a_viewer();
        let row = remote_row(Some(policy(
            &["https://elsewhere.example/lists/1", REMOTE_AUTHOR],
            &[],
        )));
        let approval = QuoteApproval::for_row(&row, &author, Some(&viewer), None);
        assert_eq!(
            approval.automatic,
            ["unsupported_policy"],
            "the author's own address names no audience"
        );
        assert_eq!(approval.current_user, "denied");
    }

    #[test]
    fn an_empty_remote_policy_denies_and_a_missing_one_is_unknown() {
        let author = remote_author();
        let viewer = a_viewer();
        let empty = remote_row(Some(QuotePolicy::default()));
        let denied = QuoteApproval::for_row(&empty, &author, Some(&viewer), None);
        assert_eq!(denied.current_user, "denied");
        assert!(denied.automatic.is_empty() && denied.manual.is_empty());

        let silent = remote_row(None);
        let unknown = QuoteApproval::for_row(&silent, &author, Some(&viewer), None);
        assert_eq!(unknown.current_user, "unknown");
        assert!(unknown.automatic.is_empty() && unknown.manual.is_empty());
    }
}
