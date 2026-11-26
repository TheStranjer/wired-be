//! The hashtag half of the featured-collection `Add` and `Remove`.
//!
//! Mastodon features a hashtag by sending an `Add` (and takes it back with a
//! `Remove`) whose `object` is an embedded `Hashtag` — an `href` and a
//! `#name`, no `id` — and whose `target` is the actor's `featured`
//! collection. [`fold_hashtag`] writes that tag into, or deletes it from, the
//! signer's featured tags, the names normalized the way `sync_remote` reads
//! a `featuredTags` collection. The caller has already checked the target.

use serde_json::Value;
use tracing::debug;

use crate::{
    federation::{
        dispatch::{FederationError, Outcome},
        featured_tags::hashtag_name,
        json,
    },
    models::{
        featured_tag::{self, Featured},
        hashtag,
        user::User,
    },
    state::AppState,
};

/// The `type` of a hashtag object.
const HASHTAG: &str = "Hashtag";

/// Whether `object` is an embedded `Hashtag`.
pub(super) fn is_hashtag(object: &Value) -> bool {
    object
        .get("type")
        .and_then(json::first_type)
        .is_some_and(|kind| kind == HASHTAG)
}

/// Features (`featuring`) or unfeatures the tag `object` names for `signer`.
///
/// An unusable name, a tag already at the ten-tag limit and an unfeatured tag
/// to take back all leave the stored set as it was.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the write.
pub(super) async fn fold_hashtag(
    state: &AppState,
    signer: &User,
    object: &Value,
    featuring: bool,
) -> Result<Outcome, FederationError> {
    let Some(name) = hashtag_name(object) else {
        return Ok(Outcome::Ignored(
            "an Add or Remove of a Hashtag that names no tag",
        ));
    };
    let failed = "could not fold the remote activity's featured hashtag";
    if featuring {
        let Some(tag) = hashtag::find_or_create(&state.db, &name)
            .await
            .map_err(|err| FederationError::transient(failed, &err))?
        else {
            return Ok(Outcome::Ignored(
                "an Add of a Hashtag with an unusable name",
            ));
        };
        let shown = featured_tag::display_name(&name, &tag);
        let featured = featured_tag::feature_as(&state.db, signer.id, &tag, &shown)
            .await
            .map_err(|err| FederationError::transient(failed, &err))?;
        if featured == Featured::LimitReached {
            return Ok(Outcome::Ignored(
                "an Add of a Hashtag beyond the featured tag limit",
            ));
        }
    } else if let Some(tag) = hashtag::get_by_name(&state.db, &name)
        .await
        .map_err(|err| FederationError::transient(failed, &err))?
    {
        let _removed = featured_tag::unfeature_tag(&state.db, signer.id, tag.id)
            .await
            .map_err(|err| FederationError::transient(failed, &err))?;
    }
    debug!(
        actor = %signer.nickname,
        tag = %name,
        featuring,
        "an inbound Add/Remove folded the actor's featured hashtag"
    );
    Ok(Outcome::Handled)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::handlers::misc::{
        handle_add, handle_remove,
        test_support::{
            ALICE, FEATURED, app_state, assert_handled, featured_signer, fresh_user, ignored, view,
        },
    };

    /// The names `user_id` features.
    async fn featured_names(pool: &sqlx::PgPool, user_id: uuid::Uuid) -> Vec<String> {
        featured_tag::list(pool, user_id)
            .await
            .expect("the featured tags read")
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    /// Mastodon's `Hashtag` object: an `href` and a `#name`, no `id`.
    fn hashtag_object(name: &str) -> Value {
        json!({
            "type": "Hashtag",
            "href": format!("https://peer.example/tags/{name}"),
            "name": format!("#{name}"),
        })
    }

    /// A Mastodon-shaped `Add` of a `Hashtag` features it (normalized, and
    /// restated harmlessly) and the matching `Remove` takes it back.
    #[tokio::test]
    async fn an_add_features_a_hashtag_and_a_remove_takes_it_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;

        let added = handle_add(
            state.clone(),
            view(
                "Add",
                "tag-add-1",
                ALICE,
                json!({"object": hashtag_object("MastoR4Feat"), "target": FEATURED}),
            ),
            alice,
        )
        .await;
        assert_handled(&added);
        assert_eq!(
            featured_names(&pool, alice_id).await,
            ["MastoR4Feat"],
            "the tag is featured under the casing the peer wrote"
        );

        let restated = handle_add(
            state.clone(),
            view(
                "Add",
                "tag-add-2",
                ALICE,
                json!({"object": hashtag_object("mastor4feat"), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&restated);
        assert_eq!(
            featured_names(&pool, alice_id).await,
            ["MastoR4Feat"],
            "a restated Add is still one row, in its first casing"
        );

        for n in ["tag-remove-1", "tag-remove-2"] {
            let removed = handle_remove(
                state.clone(),
                view(
                    "Remove",
                    n,
                    ALICE,
                    json!({"object": hashtag_object("MastoR4Feat"), "target": FEATURED}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await;
            assert_handled(&removed);
            assert!(
                featured_names(&pool, alice_id).await.is_empty(),
                "{n} leaves no featured tag"
            );
        }
    }

    /// A `Remove` of a tag nobody ever stored, a nameless `Hashtag` and an
    /// `Add` beyond the limit change nothing.
    #[tokio::test]
    async fn an_unusable_or_excess_hashtag_changes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;

        let unknown = handle_remove(
            state.clone(),
            view(
                "Remove",
                "tag-remove-unknown",
                ALICE,
                json!({"object": hashtag_object("neverstored"), "target": FEATURED}),
            ),
            alice,
        )
        .await;
        assert_handled(&unknown);

        let nameless = handle_add(
            state.clone(),
            view(
                "Add",
                "tag-add-nameless",
                ALICE,
                json!({"object": {"type": "Hashtag"}, "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(nameless).contains("names no tag"),
            "a Hashtag with no name or href is nothing to feature"
        );

        for n in 0..featured_tag::MAX_PER_USER {
            let filled = handle_add(
                state.clone(),
                view(
                    "Add",
                    &format!("tag-fill-{n}"),
                    ALICE,
                    json!({"object": hashtag_object(&format!("fill{n}")), "target": FEATURED}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await;
            assert_handled(&filled);
        }
        let excess = handle_add(
            state,
            view(
                "Add",
                "tag-fill-over",
                ALICE,
                json!({"object": hashtag_object("overflow"), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(excess).contains("limit"),
            "the eleventh tag is refused"
        );
        assert_eq!(
            featured_names(&pool, alice_id).await.len(),
            featured_tag::MAX_PER_USER,
            "the set stays at the limit"
        );
    }

    /// A `Hashtag` `Add` whose target is some other actor's collection (or
    /// any collection but the signer's featured one) is ignored.
    #[tokio::test]
    async fn a_hashtag_add_for_another_actors_collection_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;

        let foreign = handle_add(
            state,
            view(
                "Add",
                "tag-add-foreign",
                ALICE,
                json!({
                    "object": hashtag_object("hijack"),
                    "target": "https://peer.example/users/bob/collections/featured",
                }),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(foreign).contains("featured collection"),
            "only the signer's own featured collection counts"
        );
        assert!(
            featured_names(&pool, alice_id).await.is_empty(),
            "nothing was featured"
        );
    }
}
