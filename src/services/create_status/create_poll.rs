//! The create path's poll persistence tests: the `polls` row and options
//! a polled create stores beside its status, and the [`Poll`] entity the
//! fresh post answers — the `store_poll` half of [`create_status`].
//!
//! [`Poll`]: crate::entities::poll::Poll
//! [`create_status`]: crate::services::create_status::create_status

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use serde_json::{Value, json};

    use crate::{
        models::poll,
        services::create_status::test_support::{
            created, params, poll_rows, polled, state, stored, user,
        },
    };
    /// A create whose body carries a poll stores the `polls` row and its
    /// options beside the status — and the entity answers the whole
    /// [`Poll`]: the stored row's id, options in submission order at zero
    /// votes, the deadline the posted span counts out, and the posted
    /// flags. The status's own expiry stays what the body's top-level
    /// `expires_in` said, which here is nothing.
    ///
    /// [`Poll`]: crate::entities::poll::Poll
    #[tokio::test]
    async fn a_polled_create_stores_the_poll_and_renders_its_entity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "balloter").await;

        let before = Utc::now();
        let entity = created(
            &app_state,
            &alice,
            params(json!({
                "status": "ballot",
                "poll": {
                    "options": ["a", "b"],
                    "expires_in": 3600,
                    "multiple": true,
                    "hide_totals": false,
                },
            })),
        )
        .await;
        let row = stored(pool, &entity).await;
        let stand = poll::get_for_status(pool, row.id)
            .await
            .expect("the poll reads")
            .expect("the create stored the poll beside its status");

        assert_eq!(stand.status_id, row.id, "the poll names its status");
        assert!(stand.multiple, "the posted any-choice flag is stored");
        assert!(
            !stand.hide_totals,
            "and so is the posted show-totals decision"
        );
        let due = stand.expires_at.expect("an hour-out deadline");
        let span = due - before;
        assert!(
            (TimeDelta::seconds(3600) - span).num_seconds().abs() <= 5,
            "the stored instant is now plus the posted span, not {span}"
        );
        assert_eq!(stand.voters_count, 0, "a fresh poll counts nothing");
        assert_eq!(stand.votes_count, 0, "in either column");

        let options = poll::options(pool, stand.id)
            .await
            .expect("the options read");
        let titles: Vec<&str> = options.iter().map(|found| found.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b"], "options store in submission order");
        assert!(
            options.iter().all(|found| found.votes_count == 0),
            "a fresh option has no votes"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["poll"]["id"],
            stand.id.to_string(),
            "the entity answers the stored poll's id"
        );
        assert_eq!(
            value["poll"]["options"],
            json!([
                {"title": "a", "votes_count": 0},
                {"title": "b", "votes_count": 0},
            ]),
            "options render in order with their fresh counts"
        );
        assert_eq!(value["poll"]["multiple"], true, "the posted flags render");
        assert_eq!(value["poll"]["expired"], false, "an hour out is wide open");
        assert!(
            value["poll"]["expires_at"].is_string(),
            "the deadline renders as a timestamp"
        );
        assert_eq!(
            value["pleroma"]["expires_at"],
            Value::Null,
            "the poll's closing time is not the status's deletion time"
        );
        assert_eq!(
            poll_rows(pool).await,
            1,
            "exactly one polls row was written"
        );
    }

    /// A body whose poll carries only the options — no span, no flags —
    /// stores the never-closing single-choice question the checked body
    /// decided, and the entity agrees: `expires_at` null, `expired` false,
    /// `multiple` false.
    #[tokio::test]
    async fn a_bare_poll_stores_the_defaults_the_check_computed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "plain-balloter").await;

        let entity = created(&app_state, &alice, polled("which one?")).await;
        let row = stored(pool, &entity).await;
        let stand = poll::get_for_status(pool, row.id)
            .await
            .expect("the poll reads")
            .expect("the create stored the poll");

        assert!(
            stand.expires_at.is_none(),
            "no expires_in is a poll that never closes"
        );
        assert!(!stand.multiple, "an absent flag stores `false`");
        assert!(!stand.hide_totals, "and so does the other one");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(value["poll"]["expires_at"], Value::Null);
        assert_eq!(
            value["poll"]["expired"], false,
            "never closes, never closed"
        );
        assert_eq!(value["poll"]["multiple"], false);
        assert_eq!(
            value["poll"]["votes_count"], 0,
            "the roll-ups start at zero"
        );
        assert_eq!(value["poll"]["voters_count"], 0);
        assert_eq!(
            value["poll"]["voted"], true,
            "the author reads her poll as voted"
        );
        assert_eq!(value["poll"]["own_votes"], json!([]));
        assert_eq!(
            value["poll"]["options"],
            json!([
                {"title": "a", "votes_count": 0},
                {"title": "b", "votes_count": 0},
            ]),
            "the checked options reach the rows and the render in order"
        );
    }
}
