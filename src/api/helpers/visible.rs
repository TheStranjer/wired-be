//! The batched "fetch, filter by visibility, render" step the status-id
//! listings share ([`render_visible`]) and the JSON-keying helper its map
//! callers use ([`keyed_json`]).

use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

use crate::{
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    models::{
        status::{self, Status},
        status_visibility,
        user::User,
    },
    services::render_status::{ViewerMode, render_statuses},
    state::AppState,
    util::id::parse_id,
};

/// The status rows `ids` names, as `viewer` may see them, rendered and in
/// the requested order.
///
/// One [`status::get_many`] read, one batched
/// [`status_visibility::filter_visible`] statement, one
/// [`render_statuses`] pass. An id that names no live row, one a delete
/// took, and one the visibility rules hide from `viewer` simply drop out;
/// an id asked twice answers once, at its first position.
///
/// # Errors
///
/// [`AppError::Database`] from the status and visibility reads and from
/// the renderer.
pub async fn render_visible(
    state: &AppState,
    viewer: Option<&User>,
    ids: &[Uuid],
) -> AppResult<Vec<StatusEntity>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = status::get_many(&state.db, ids).await?;
    let mut by_id: HashMap<Uuid, Status> = rows.into_iter().map(|row| (row.id, row)).collect();
    let ordered: Vec<Status> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
    let visible = status_visibility::filter_visible(&state.db, ordered, viewer).await?;
    render_statuses(state, ViewerMode::User(viewer), &visible).await
}

/// Serializes `entities` to JSON values keyed by their row ids; an entity
/// whose id is not id text is skipped.
///
/// # Errors
///
/// [`AppError::Internal`] if an entity would not serialize.
pub fn keyed_json(entities: Vec<StatusEntity>) -> AppResult<HashMap<Uuid, Value>> {
    let mut keyed = HashMap::with_capacity(entities.len());
    for entity in entities {
        let Some(id) = parse_id(&entity.id) else {
            continue;
        };
        let value = serde_json::to_value(entity).map_err(|err| {
            AppError::Internal(format!("status entity failed to serialize: {err}"))
        })?;
        let _first = keyed.insert(id, value);
    }
    Ok(keyed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::{status::Visibility, user},
    };

    /// Inserts a local user named `nickname`.
    async fn local_user(state: &AppState, nickname: &str) -> User {
        user::insert(
            &state.db,
            &user::NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts `author`'s status of `visibility`.
    async fn post(state: &AppState, author: &User, visibility: Visibility) -> Status {
        status::insert(
            &state.db,
            TEST_BASE_URL,
            &status::NewStatus {
                visibility,
                ..status::NewStatus::local(author, "a post")
            },
        )
        .await
        .expect("the post inserts")
    }

    #[tokio::test]
    async fn render_visible_drops_what_the_viewer_may_not_see_and_keeps_the_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let alice = local_user(&state, "alice").await;
        let bob = local_user(&state, "bob").await;
        let first = post(&state, &alice, Visibility::Public).await;
        let hidden = post(&state, &alice, Visibility::Private).await;
        let last = post(&state, &alice, Visibility::Public).await;
        let unknown = Uuid::from_u128(1);
        let asked = [last.id, unknown, hidden.id, first.id, last.id];

        let seen = render_visible(&state, Some(&bob), &asked)
            .await
            .expect("the batch renders");
        assert_eq!(
            seen.iter()
                .map(|entity| entity.id.clone())
                .collect::<Vec<_>>(),
            vec![last.id.to_string(), first.id.to_string()],
            "the private post of an unfollowed author, the unknown id and the repeat drop out; \
             the rest keep the requested order"
        );

        let own = render_visible(&state, Some(&alice), &[hidden.id, first.id])
            .await
            .expect("the author's batch renders");
        assert_eq!(
            own.iter()
                .map(|entity| entity.id.clone())
                .collect::<Vec<_>>(),
            vec![hidden.id.to_string(), first.id.to_string()],
            "the author sees her own private post, in the requested order"
        );

        let none = render_visible(&state, Some(&bob), &[])
            .await
            .expect("an empty ask renders");
        assert!(none.is_empty(), "no ids, no statuses");

        let keyed = keyed_json(seen).expect("the entities serialize");
        assert_eq!(keyed.len(), 2, "one value per rendered status");
        let value = keyed.get(&last.id).expect("the newest status is keyed");
        assert_eq!(
            value["id"],
            last.id.to_string(),
            "each value sits under its row id"
        );
    }

    #[test]
    fn keyed_json_of_nothing_is_empty() {
        assert!(
            keyed_json(Vec::new())
                .expect("nothing to serialize")
                .is_empty()
        );
    }
}
