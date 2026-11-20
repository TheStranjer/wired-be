//! The `data` maps: the subject, permission, tag, report and status
//! fragments an entry adds to the actor, action and message keys, in
//! Pleroma's map shapes.

use serde_json::{Map, Value};
use uuid::Uuid;

use super::{TYPE_USER, put};
use crate::models::user::User;

/// Pleroma's `user_to_map`: `{id, nickname, type: "user"}`, the id as its
/// hyphenated text (decision 4).
pub(super) fn user_map(user: &User) -> Map<String, Value> {
    let mut map = Map::new();
    put(&mut map, "id", Value::String(user.id.to_string()));
    put(&mut map, "nickname", Value::String(user.nickname.clone()));
    put(&mut map, "type", Value::String(TYPE_USER.to_owned()));
    map
}

/// The `subject` entry for the user-list actions: an array of
/// [`user_map`]s, Pleroma's `user_to_map` over the list.
pub(super) fn subject_data(users: &[User]) -> Map<String, Value> {
    subjects_under("subject", users)
}

/// The same array under Pleroma's plural `subjects` key, which only the
/// `create` clause of `get_log_entry_message` matches on.
pub(super) fn subjects_data(users: &[User]) -> Map<String, Value> {
    subjects_under("subjects", users)
}

/// A user array under `key`: the one shape the singular and plural
/// subject entries share.
pub(super) fn subjects_under(key: &str, users: &[User]) -> Map<String, Value> {
    let mut data = Map::new();
    let subjects = users
        .iter()
        .map(|user| Value::Object(user_map(user)))
        .collect::<Vec<Value>>();
    put(&mut data, key, Value::Array(subjects));
    data
}

/// The same with the role the grant/revoke sentence names.
pub(super) fn permission_data(users: &[User], permission: &str) -> Map<String, Value> {
    let mut data = subject_data(users);
    put(
        &mut data,
        "permission",
        Value::String(permission.to_owned()),
    );
    data
}

/// The `nicknames` and `tags` keys Pleroma's tag/untag clauses read.
pub(super) fn tags_data(tags: &[String], nicknames: &[String]) -> Map<String, Value> {
    let mut data = Map::new();
    put(
        &mut data,
        "nicknames",
        Value::Array(
            nicknames
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<Value>>(),
        ),
    );
    put(
        &mut data,
        "tags",
        Value::Array(tags.iter().cloned().map(Value::String).collect()),
    );
    data
}

/// The `subject` map of a report action (Pleroma's `report_to_map`,
/// state-included for [`report_update`]) plus the optional
/// `subject_actor` user map the sentence reads its ` on user @…` tail
/// from.
pub(super) fn report_data(
    report_id: Uuid,
    state: Option<&str>,
    subject_actor: Option<&User>,
) -> Map<String, Value> {
    let mut subject = Map::new();
    put(&mut subject, "type", Value::String("report".to_owned()));
    put(&mut subject, "id", Value::String(report_id.to_string()));
    if let Some(state) = state {
        put(&mut subject, "state", Value::String(state.to_owned()));
    }
    let mut data = Map::new();
    put(&mut data, "subject", Value::Object(subject));
    if let Some(actor) = subject_actor {
        put(&mut data, "subject_actor", Value::Object(user_map(actor)));
    }
    data
}

/// The `subject` map of a status action plus both changed parts: Pleroma's
/// `status_update` data always carries `sensitive` and `visibility`, the
/// untouched one as `null`.
pub(super) fn status_data(
    status_id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<&str>,
) -> Map<String, Value> {
    let mut subject = Map::new();
    put(&mut subject, "type", Value::String("status".to_owned()));
    put(&mut subject, "id", Value::String(status_id.to_string()));
    let mut data = Map::new();
    put(&mut data, "subject", Value::Object(subject));
    put(
        &mut data,
        "sensitive",
        sensitive.map_or(Value::Null, Value::Bool),
    );
    put(
        &mut data,
        "visibility",
        visibility.map_or(Value::Null, |visibility| {
            Value::String(visibility.to_owned())
        }),
    );
    data
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::super::{ModAction, record};
    use super::*;
    use crate::models::moderation_log::test_support::{one, report_id, stored};

    #[tokio::test]
    async fn record_stores_actor_subjects_and_merged_data() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let row = record(
            &pool,
            &actor,
            ModAction {
                action: "test_action".to_owned(),
                subjects: vec![&alice],
                message: "a sentence".to_owned(),
                data: subject_data(one(&alice)),
            },
        )
        .await
        .expect("the entry records");
        assert_eq!(row.action, "test_action", "the action column");
        assert_eq!(row.message, "a sentence", "the message column");
        assert_eq!(row.actor_id, Some(actor.id), "the actor column");
        assert_eq!(
            row.actor_nickname, actor.nickname,
            "and its recorded nickname"
        );
        assert_eq!(row.subject_ids, vec![alice.id], "the subject ids column");
        assert_eq!(
            row.subject_nicknames,
            vec!["alice".to_owned()],
            "and the subject nicknames column"
        );
        assert!(
            row.inserted_at > Utc::now() - Duration::minutes(1),
            "stamped by the column default"
        );
        assert_eq!(
            row.data
                .get("actor")
                .and_then(Value::as_object)
                .and_then(|entry| entry.get("nickname"))
                .and_then(Value::as_str),
            Some("mod"),
            "the data map carries Pleroma's actor map"
        );
        assert_eq!(
            row.data.get("message").and_then(Value::as_str),
            Some("a sentence"),
            "and the keys record injects"
        );
        assert_eq!(
            row.data
                .get("subject")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1),
            "beside the caller's own extras"
        );
    }

    #[test]
    fn data_maps_keep_pleromas_shapes() {
        let subject = subject_data(&[]);
        assert_eq!(
            subject.get("subject"),
            Some(&Value::Array(Vec::new())),
            "the user-list actions carry an array of user maps"
        );
        let status = status_data(report_id(), None, Some("public"));
        assert_eq!(
            status.get("sensitive"),
            Some(&Value::Null),
            "the untouched part rides as null, as Pleroma merges it"
        );
        assert_eq!(
            status
                .get("subject")
                .and_then(Value::as_object)
                .and_then(|inner| inner.get("type"))
                .and_then(Value::as_str),
            Some("status"),
            "the subject map says what it is"
        );
    }
}
