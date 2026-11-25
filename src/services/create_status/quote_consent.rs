//! The quoted author's consent, checked before a quote is written.
//!
//! Mastodon refuses a quote its author's policy denies up front, with a 422,
//! instead of posting it and waiting for the `Reject`. `require_consent`
//! asks the same question the `quote_approval` key answers clients:
//! `current_user` of the quoted status for the poster. `denied` is refused;
//! `automatic`, `manual` and `unknown` (a remote post that advertised no
//! policy) go through.

use super::QUOTE_DENIED;
use crate::{
    entities::{
        relationship::{Relationship, RelationshipMap},
        status::QuoteApproval,
    },
    error::{AppError, AppResult},
    models::{
        activitypub_actor, follow,
        status::Status,
        user::{self, User},
    },
    state::AppState,
};

/// Lets the quote of `target` by `poster` through, or refuses it.
///
/// The poster's own posts are always quotable, and a target whose author
/// row is gone is left to the later steps.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`QUOTE_DENIED`] when the quoted
/// author's policy denies the poster; any database failure arrives through
/// `?` as [`AppError::Database`].
pub(super) async fn require_consent(
    state: &AppState,
    poster: &User,
    target: &Status,
) -> AppResult<()> {
    if target.user_id == poster.id {
        return Ok(());
    }
    let Some(author) = user::get_by_id(&state.db, target.user_id).await? else {
        return Ok(());
    };
    // A remote policy names the author's collections, which the actor row holds.
    let author = activitypub_actor::with_plumbing(&state.db, author).await?;
    let mut standing = Relationship::unconnected(author.id);
    standing.following.0 = follow::has_accepted_follow(&state.db, poster.id, author.id).await?;
    standing.followed_by.0 = follow::has_accepted_follow(&state.db, author.id, poster.id).await?;
    let standings = RelationshipMap::from([(author.id, standing)]);
    let approval = QuoteApproval::for_row(target, &author, Some(poster), Some(&standings));
    if approval.current_user == "denied" {
        return Err(AppError::Unprocessable(QUOTE_DENIED.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{
        federation::vocab::AS_PUBLIC,
        models::{
            quote_request,
            status::{self, NewStatus, QuoteApprovalPolicy, QuotePolicy, Visibility},
        },
        services::{
            create_status::{
                CreateStatusParams, create_status,
                test_support::{BASE, created, posted, state, stored, user},
            },
            render_status::{ViewerMode, render_status},
        },
        test_fixtures::remote_user,
    };

    /// A body posting `text` as a quote of `quoted`.
    fn quoting(text: &str, quoted: &Status) -> CreateStatusParams {
        CreateStatusParams {
            quote_id: Some(quoted.id.to_string()),
            ..posted(text)
        }
    }

    /// The `quote.state` of an entity, as it serializes.
    fn quote_state<T: serde::Serialize>(entity: &T) -> String {
        let value = serde_json::to_value(entity).expect("the entity serializes");
        value["quote"]["state"]
            .as_str()
            .expect("the entity carries a quote state")
            .to_owned()
    }

    #[tokio::test]
    async fn a_denied_quote_is_a_422_and_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "denyalice").await;
        let bob = remote_user(pool, "denybob").await;
        let nobody = remote_post(pool, &bob, Some(QuotePolicy::default())).await;

        let refusal = create_status(&app_state, &alice, None, quoting("quoting", &nobody))
            .await
            .expect_err("the author's policy denies the quote");

        assert!(
            matches!(&refusal, AppError::Unprocessable(text) if text == QUOTE_DENIED),
            "Mastodon's wording: {refusal:?}"
        );
        let mine = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE local")
            .fetch_one(pool)
            .await
            .expect("the count reads");
        assert_eq!(mine, 0, "no status was written");
    }

    #[tokio::test]
    async fn a_remote_quote_is_pending_until_the_author_accepts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "pendingalice").await;
        let bob = remote_user(pool, "pendingbob").await;
        let open = remote_post(
            pool,
            &bob,
            Some(QuotePolicy {
                automatic: vec![AS_PUBLIC.to_owned()],
                manual: Vec::new(),
            }),
        )
        .await;

        let entity = created(&app_state, &alice, quoting("quoting", &open)).await;
        assert_eq!(
            quote_state(&entity),
            "pending",
            "the create answers pending"
        );

        let row = stored(pool, &entity).await;
        let request_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM quote_requests WHERE quoting_status_id = $1",
        )
        .bind(row.id)
        .fetch_one(pool)
        .await
        .expect("the request was recorded");
        let again = render_status(&app_state, ViewerMode::user(&alice), &row)
            .await
            .expect("the render runs")
            .expect("a live row renders");
        assert_eq!(quote_state(&again), "pending", "so does the next read");

        quote_request::accept(pool, request_id, "https://remote.example/grants/1")
            .await
            .expect("the grant stores");
        let accepted = render_status(&app_state, ViewerMode::user(&alice), &row)
            .await
            .expect("the render runs")
            .expect("a live row renders");
        assert_eq!(quote_state(&accepted), "accepted", "after the Accept");
    }

    #[tokio::test]
    async fn a_local_quote_is_accepted_at_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "acceptalice").await;
        let bob = user(pool, "acceptbob").await;
        let theirs = status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Public,
                ..NewStatus::local(&bob, "quote me")
            },
        )
        .await
        .expect("the post inserts");

        let entity = created(&app_state, &alice, quoting("quoting", &theirs)).await;

        assert_eq!(quote_state(&entity), "accepted", "no consent round-trip");
    }

    #[tokio::test]
    async fn a_quote_policy_in_the_body_is_stored_and_edited() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "policyalice").await;

        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                quote_approval_policy: Some("nobody".to_owned()),
                ..posted("no quotes")
            },
        )
        .await;
        let row = stored(pool, &entity).await;
        assert_eq!(
            row.quote_approval_policy,
            Some(QuoteApprovalPolicy::Nobody),
            "the create stores the author's choice"
        );

        let edit = crate::extract::into_params::<crate::services::create_status::EditStatusParams>(
            serde_json::json!({"quote_approval_policy": "followers"}),
        )
        .expect("an edit body naming only the policy");
        let _edited = crate::services::create_status::edit_status(&app_state, &alice, &row, edit)
            .await
            .expect("the edit writes")
            .expect("the status is live");
        let after = status::get_by_id(pool, row.id)
            .await
            .unwrap()
            .expect("the status is stored");
        assert_eq!(
            after.quote_approval_policy,
            Some(QuoteApprovalPolicy::Followers),
            "the edit rewrites it"
        );

        let invalid = CreateStatusParams {
            quote_approval_policy: Some("friends".to_owned()),
            ..posted("bad policy")
        };
        let refusal = create_status(&app_state, &alice, None, invalid)
            .await
            .expect_err("an unknown policy is refused");
        assert!(
            matches!(&refusal, AppError::Unprocessable(text) if text == "Invalid quote_approval_policy"),
            "the 422 wording: {refusal:?}"
        );
    }

    /// A mirrored post of `author` advertising `policy`.
    async fn remote_post(
        pool: &sqlx::PgPool,
        author: &User,
        policy: Option<QuotePolicy>,
    ) -> Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                uri: Some(format!(
                    "https://remote.example/objects/{}",
                    author.nickname
                )),
                user_id: author.id,
                quote_policy: policy,
                ..NewStatus::default()
            },
        )
        .await
        .expect("the mirrored post inserts")
    }

    #[tokio::test]
    async fn a_remote_policy_that_denies_the_poster_refuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "consentalice").await;
        let bob = remote_user(pool, "consentbob").await;
        let nobody = remote_post(pool, &bob, Some(QuotePolicy::default())).await;

        let refusal = require_consent(&app_state, &alice, &nobody)
            .await
            .expect_err("an empty policy denies everybody");

        assert!(
            matches!(&refusal, AppError::Unprocessable(text) if text == QUOTE_DENIED),
            "the 422 carries Mastodon's wording: {refusal:?}"
        );
    }

    #[tokio::test]
    async fn an_allowing_or_silent_remote_policy_goes_through() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "consentcarol").await;
        let bob = remote_user(pool, "consentdave").await;
        let open = remote_post(
            pool,
            &bob,
            Some(QuotePolicy {
                automatic: vec![AS_PUBLIC.to_owned()],
                manual: Vec::new(),
            }),
        )
        .await;
        require_consent(&app_state, &alice, &open)
            .await
            .expect("a public policy allows");

        let erin = remote_user(pool, "consenterin").await;
        let silent = remote_post(pool, &erin, None).await;
        require_consent(&app_state, &alice, &silent)
            .await
            .expect("a post that advertised nothing is unknown, which is allowed");
    }

    #[tokio::test]
    async fn a_local_nobody_policy_refuses_everyone_but_its_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "consentfrank").await;
        let bob = user(pool, "consentgina").await;
        let post = status::insert(
            pool,
            BASE,
            &NewStatus {
                quote_approval_policy: Some(QuoteApprovalPolicy::Nobody),
                ..NewStatus::local(&alice, "not quotable")
            },
        )
        .await
        .expect("the post inserts");

        require_consent(&app_state, &alice, &post)
            .await
            .expect("the author may quote their own post");
        assert!(
            require_consent(&app_state, &bob, &post).await.is_err(),
            "anybody else is refused"
        );
    }
}
