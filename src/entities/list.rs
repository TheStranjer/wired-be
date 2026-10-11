//! The Mastodon v1 `List` entity: one account's list of accounts as clients
//! render it.
//!
//! The reference shape is the ticket's four keys — `id`, `title`,
//! `replies_policy` and `exclusive` — which is Mastodon's `List` serializer.
//! Pleroma 2.10's `ListView.render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/list_view.ex`) also carries only a
//! `pleroma.emoji` block besides `id`/`title`/`exclusive`: it has no
//! `replies_policy` column and no Pleroma extension belongs in this entity,
//! so the shape is Mastodon's. `id` is Pleroma's integer counter replaced
//! by the row's `UUIDv7` as lowercase hyphenated text (decision 4).
//!
//! [`List::from_row`] is pure (decision 3): it renders a
//! [`models::list`](crate::models::list) row and nothing else — the owner,
//! the row's timestamps and the membership table live behind it, not in
//! this shape.

use serde::Serialize;

use crate::models::list::List as ListRow;

/// The Mastodon v1 `List` entity.
///
/// Build one with [`List::from_row`]; its [`Serialize`] impl emits `id`,
/// `title`, `replies_policy` and `exclusive`.
#[derive(Debug, Serialize)]
pub struct List {
    /// The list id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The user-defined title, verbatim as its owner wrote it.
    pub title: String,
    /// Whose replies the list shows: `none`, `list` or `followed`.
    pub replies_policy: String,
    /// Whether members are removed from the owner's Home feed.
    pub exclusive: bool,
}

impl List {
    /// Renders `row` as the entity.
    #[must_use]
    pub fn from_row(row: &ListRow) -> Self {
        Self {
            id: row.id.to_string(),
            title: row.title.clone(),
            replies_policy: row.replies_policy.clone(),
            exclusive: row.exclusive,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    /// The list id, as a client sees it.
    const LIST_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

    /// Every JSON key the entity must emit, sorted, for an exact key-set
    /// check.
    const EXPECTED_KEYS: [&str; 4] = ["exclusive", "id", "replies_policy", "title"];

    /// A stored list with an explicit policy and exclusiveness on.
    fn row() -> ListRow {
        ListRow {
            id: Uuid::parse_str(LIST_ID).expect("the fixture id is an id"),
            user_id: Uuid::now_v7(),
            title: "Close friends".to_owned(),
            replies_policy: "followed".to_owned(),
            exclusive: true,
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// `row` rendered as a JSON value; entities always serialize.
    fn render(row: &ListRow) -> Value {
        serde_json::to_value(List::from_row(row)).expect("an entity serializes")
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("lists render objects")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn a_list_renders_the_mastodon_entity() {
        let value = render(&row());

        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(value["id"], LIST_ID, "the id as hyphenated text");
        assert_eq!(value["title"], "Close friends", "the title verbatim");
        assert_eq!(value["replies_policy"], "followed", "the stored policy");
        assert_eq!(value["exclusive"], true, "the stored flag");
    }

    #[test]
    fn the_column_defaults_render_through() {
        let stored = ListRow {
            title: String::new(),
            replies_policy: "list".to_owned(),
            exclusive: false,
            ..row()
        };
        let value = render(&stored);

        assert_eq!(value["title"], "", "an empty title is empty, not null");
        assert_eq!(value["replies_policy"], "list", "the default policy");
        assert_eq!(value["exclusive"], false, "and the default flag");
    }

    #[test]
    fn every_policy_value_renders_as_stored() {
        for policy in ["none", "list", "followed"] {
            let stored = ListRow {
                replies_policy: policy.to_owned(),
                ..row()
            };
            let value = render(&stored);
            assert_eq!(
                value["replies_policy"],
                json!(policy),
                "`{policy}` goes through verbatim"
            );
        }
    }
}
