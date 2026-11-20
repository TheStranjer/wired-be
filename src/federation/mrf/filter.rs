//! The pipeline's entry points: the filters that run received and
//! outgoing documents through the chain, the actor-document stage, and
//! the per-activity fact preloads that fill the [`MrfContext`] before a
//! policy reads it.

use chrono::Utc;
use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use tracing::warn;

use super::{
    MrfContext, MrfDecision, anti_link_spam,
    chain::{FOLLOW_TYPE, build_chain, run_chain},
    config::load_config,
    simple, tag,
};
use crate::{
    federation::{dispatch::ActivityView, json},
    models::user,
    state::AppState,
};

/// Runs one received activity through the instance's filter chain, over
/// the view's own document: the dispatcher re-normalizes whatever the
/// answer accepts, so a rewrite is what gets routed.
#[must_use]
pub async fn filter_incoming(state: AppState, view: &ActivityView) -> MrfDecision {
    run_chain(&state, view.raw.clone()).await
}

/// [`filter_incoming`] in the shape the dispatcher's `fn`-pointer policy
/// seam takes: the same filter, boxed so it can name no lifetime.
pub fn filter_incoming_boxed(
    state: AppState,
    view: &ActivityView,
) -> BoxFuture<'static, MrfDecision> {
    let raw = view.raw.clone();
    Box::pin(async move { run_chain(&state, raw).await })
}

/// Runs one activity this instance is about to send through the same
/// chain, before any recipient is computed: [`publisher`](crate::federation::publisher)
/// delivers the accepted document and nothing at all on a rejection.
#[must_use]
pub async fn filter_outgoing(state: &AppState, activity: Value) -> MrfDecision {
    run_chain(state, activity).await
}

/// [`filter_outgoing`] in the shape of a `fn`-pointer seam, boxed so the
/// caller's borrow cannot leak into the future.
pub fn filter_outgoing_boxed(state: AppState, activity: Value) -> BoxFuture<'static, MrfDecision> {
    Box::pin(async move { run_chain(&state, activity).await })
}

/// Runs one actor document through the instance's chain's actor stage.
///
/// This is what [`upsert_remote_user`](crate::federation::remote_actor::upsert_remote_user)'s
/// callers store and what the inbound `Update` of an account folds, after
/// the policies that strip a profile had their way with it. An actor
/// document cannot be rejected — see
/// [`filter_actor`](super::MrfPolicy::filter_actor) — so the answer is the
/// document, and a chain that strips nothing hands back exactly what it
/// was given.
#[must_use]
pub async fn filter_actor(state: &AppState, actor: Value) -> Value {
    let ctx = context(state).await;
    build_chain(ctx.config.as_ref()).apply_actor(&ctx, actor)
}

/// The context one pipeline run preloads: the current configuration beside
/// the chain's single shared "now" and this instance's own domain. The
/// per-actor facts stay at their unloaded defaults here — an activity run
/// fills them through [`preload_facts`] on top of this base, and the
/// actor-document stage has no activity to fill them from.
async fn context(state: &AppState) -> MrfContext {
    MrfContext {
        config: load_config(state).await,
        now: Utc::now(),
        local_domain: state.config.domain().to_owned(),
        author_follower_count: None,
        author_status_count: None,
        author_tags: Vec::new(),
        target_tags: Vec::new(),
    }
}

/// The context of one ACTIVITY run: [`context`] with the per-actor facts
/// the enabled policies read, preloaded from the database first.
pub(super) async fn activity_context(state: &AppState, activity: &Value) -> MrfContext {
    let mut ctx = context(state).await;
    preload_facts(&state.db, &mut ctx, activity).await;
    ctx
}

/// Preloads the per-activity facts: one query each, and only while the
/// policy that reads the fact is enabled, so a chain that has no use for
/// a counter never pays for it. A failed read leaves the fact unloaded —
/// the policies weigh a missing fact conservatively rather than the
/// pipeline ending on a database error.
async fn preload_facts(db: &PgPool, ctx: &mut MrfContext, activity: &Value) {
    let actor = activity.get("actor").and_then(json::value_or_id);
    if ctx.config.enabled(anti_link_spam::PARAMS_KEY) {
        preload_counts(db, ctx, actor.as_deref()).await;
    }
    if ctx.config.enabled(tag::PARAMS_KEY) {
        preload_tags(db, ctx, activity, actor.as_deref()).await;
    }
}

/// The `anti_link_spam` query: the author row's two denormalized counters.
async fn preload_counts(db: &PgPool, ctx: &mut MrfContext, actor: Option<&str>) {
    let Some(ap_id) = actor else { return };
    match user::counts_by_ap_id(db, ap_id).await {
        Ok(Some((followers, statuses))) => {
            ctx.author_follower_count = Some(followers);
            ctx.author_status_count = Some(statuses);
        }
        Ok(None) => {}
        Err(err) => {
            warn!(%err, "the author-counts preload failed; the counts stay unloaded");
        }
    }
}

/// The `tag` queries: the actor's tags on every activity, and — for a
/// `Follow` — the local target's.
async fn preload_tags(db: &PgPool, ctx: &mut MrfContext, activity: &Value, actor: Option<&str>) {
    if let Some(ap_id) = actor {
        ctx.author_tags = preload_user_tags(db, ap_id, false).await;
    }
    let follows = simple::activity_type(activity).is_some_and(|kind| kind == FOLLOW_TYPE);
    let target = follows
        .then(|| activity.get("object").and_then(json::value_or_id))
        .flatten();
    if let Some(ap_id) = target {
        ctx.target_tags = preload_user_tags(db, &ap_id, true).await;
    }
}

/// One tags read: the row's tags; the empty list for an actor this
/// instance does not store, or a `Follow` target it does not own; and the
/// empty list, beside the warning, for a failed read.
async fn preload_user_tags(db: &PgPool, ap_id: &str, local_only: bool) -> Vec<String> {
    let read = if local_only {
        user::local_tags_by_ap_id(db, ap_id).await
    } else {
        user::tags_by_ap_id(db, ap_id).await
    };
    match read {
        Ok(Some(tags)) => tags,
        Ok(None) => Vec::new(),
        Err(err) => {
            warn!(%err, "an MRF tags preload failed; the tags stay unloaded");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::mrf::test_support::{follow_activity, set_mrf_entry, test_state};

    /// The incoming filter runs the configured chain over the view's own
    /// document: names resolve, unknown ones vanish, and the document
    /// arrives unchanged.
    #[tokio::test]
    async fn the_incoming_filter_runs_the_configured_chain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_mrf_entry(&pool, ":mrf", &json!({"policies": ["ghost", "noop"]}))
            .await
            .expect("the setting writes");
        let state = test_state(&pool);
        let activity = follow_activity();
        let view = ActivityView::parse(&activity).expect("the fixture parses");

        let decision = filter_incoming(state, &view).await;

        assert_eq!(
            decision,
            MrfDecision::Accept(activity),
            "noop (beside the skipped ghost name) hands the document back"
        );
    }

    /// The outgoing filter speaks the same answer over the raw document.
    #[tokio::test]
    async fn the_outgoing_filter_accepts_an_unconfigured_chain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool());

        let decision = filter_outgoing(&state, follow_activity()).await;

        assert!(
            matches!(decision, MrfDecision::Accept(_)),
            "no stored row and no env list is the empty chain: {decision:?}"
        );
    }

    /// The context is preloaded for the policies: it carries the local
    /// domain and the shared "now" without the policy touching the
    /// database.
    #[tokio::test]
    async fn the_context_arrives_preloaded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_mrf_entry(&pool, ":mrf", &json!({"noop": {"probe": true}}))
            .await
            .expect("the setting writes");
        let state = test_state(&pool);

        let config = load_config(&state).await;
        let ctx = MrfContext {
            config,
            now: Utc::now(),
            local_domain: state.config.domain().to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        };

        assert_eq!(ctx.local_domain, "test.example", "the preloaded domain");
        assert_eq!(
            ctx.config.params_for("noop"),
            Some(&json!({"probe": true})),
            "and the preloaded setting document"
        );
    }

    /// The remote author the preload fixtures post as: stored with
    /// standing and one moderation tag.
    async fn preloaded_author(pool: &PgPool) -> user::User {
        user::insert(
            pool,
            &user::NewUser {
                nickname: "alice@peer.example".to_owned(),
                ap_id: "https://peer.example/users/alice".to_owned(),
                local: user::Flag::new(false),
                domain: Some("peer.example".to_owned()),
                followers_count: 7,
                statuses_count: 3,
                tags: vec!["mrf_tag:media-strip".to_owned()],
                ..user::NewUser::default()
            },
        )
        .await
        .expect("the author fixture inserts")
    }

    /// The local account the `Follow` fixture addresses, tagged so its
    /// subscription rule is visible in the preload.
    async fn preloaded_target(pool: &PgPool) -> user::User {
        user::insert(
            pool,
            &user::NewUser {
                tags: vec!["mrf_tag:disable-any-subscription".to_owned()],
                ..user::NewUser::local("carol", "https://test.example/users/carol")
            },
        )
        .await
        .expect("the target fixture inserts")
    }

    /// A `Create` of a linked note by `actor`.
    fn create_note(actor: &str) -> Value {
        json!({
            "id": "https://peer.example/activities/preload-1",
            "type": "Create",
            "actor": actor,
            "object": {
                "id": "https://peer.example/objects/preload-1",
                "type": "Note",
                "attributedTo": actor,
                "content": "<p><a href=\"https://spam.example/x\">x</a></p>",
            },
        })
    }

    /// A `Follow` of `target` by `actor`.
    fn follow_of(actor: &str, target: &str) -> Value {
        json!({
            "id": "https://peer.example/activities/preload-2",
            "type": "Follow",
            "actor": actor,
            "object": target,
        })
    }

    /// The per-activity facts preload from the database only while the
    /// policy that reads them is enabled: nothing enabled reads nothing
    /// although both rows would answer, `anti_link_spam` reads the two
    /// counters and no tags, `tag` reads the author's tags on every
    /// activity and the LOCAL `Follow` target's, and the counts stay
    /// unread.
    #[tokio::test]
    async fn the_preload_fills_only_the_enabled_policies_facts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = preloaded_author(&pool).await;
        let carol = preloaded_target(&pool).await;
        let create = create_note(&alice.ap_id);
        let follow = follow_of(&alice.ap_id, &carol.ap_id);

        set_mrf_entry(&pool, ":mrf", &json!({"policies": []}))
            .await
            .expect("the setting writes");
        let bare = activity_context(&test_state(&pool), &create).await;
        assert_eq!(bare.author_follower_count, None, "no policy, no counts");
        assert_eq!(bare.author_status_count, None, "nor their twin");
        assert!(bare.author_tags.is_empty(), "nor any tags");
        let bare_follow = activity_context(&test_state(&pool), &follow).await;
        assert!(bare_follow.target_tags.is_empty(), "nor a Follow target's");

        set_mrf_entry(&pool, ":mrf", &json!({"policies": ["anti_link_spam"]}))
            .await
            .expect("the rewrite writes");
        let counted = activity_context(&test_state(&pool), &create).await;
        assert_eq!(counted.author_follower_count, Some(7), "the counters load");
        assert_eq!(counted.author_status_count, Some(3), "and only they");
        assert!(counted.author_tags.is_empty(), "the tag facts stay unread");

        set_mrf_entry(&pool, ":mrf", &json!({"policies": ["tag"]}))
            .await
            .expect("the rewrite writes");
        let tagged = activity_context(&test_state(&pool), &create).await;
        assert_eq!(
            tagged.author_tags,
            vec!["mrf_tag:media-strip".to_owned()],
            "the actor's tags load"
        );
        assert_eq!(tagged.author_follower_count, None, "the counts stay unread");
        assert!(
            tagged.target_tags.is_empty(),
            "a `Create` addresses no `Follow` target"
        );

        let following = activity_context(&test_state(&pool), &follow).await;
        assert_eq!(
            following.target_tags,
            vec!["mrf_tag:disable-any-subscription".to_owned()],
            "the local target of a `Follow` reads as its tags"
        );

        let remote_target =
            activity_context(&test_state(&pool), &follow_of(&alice.ap_id, &alice.ap_id)).await;
        assert!(
            remote_target.target_tags.is_empty(),
            "a remote target is nobody's subscription subject: the local row is the only one read"
        );

        let stranger = activity_context(
            &test_state(&pool),
            &create_note("https://nowhere.example/users/x"),
        )
        .await;
        assert!(
            stranger.author_tags.is_empty(),
            "and an actor with no row preloads no tags rather than failing"
        );
    }

    /// The whole pipeline with `anti_link_spam` enabled: the preload
    /// reaches the policy, a brand-new remote author's linked note is
    /// rejected and the same note with standing passes.
    #[tokio::test]
    async fn the_incoming_filter_rejects_a_linked_note_from_a_new_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_mrf_entry(&pool, ":mrf", &json!({"policies": ["anti_link_spam"]}))
            .await
            .expect("the setting writes");
        let newcomer = user::insert(
            &pool,
            &user::NewUser {
                nickname: "eve@peer.example".to_owned(),
                ap_id: "https://peer.example/users/eve".to_owned(),
                local: user::Flag::new(false),
                domain: Some("peer.example".to_owned()),
                ..user::NewUser::default()
            },
        )
        .await
        .expect("the newcomer fixture inserts");
        let state = test_state(&pool);

        let spam = create_note(&newcomer.ap_id);
        let view = ActivityView::parse(&spam).expect("the fixture parses");
        assert_eq!(
            filter_incoming(state.clone(), &view).await,
            MrfDecision::Reject(
                "[AntiLinkSpamPolicy] the author has no posts nor followers".to_owned()
            ),
            "the preloaded zeroes reach the policy"
        );

        let mentioned = json!({
            "id": "https://peer.example/activities/preload-3",
            "type": "Create",
            "actor": &newcomer.ap_id,
            "object": {
                "id": "https://peer.example/objects/preload-3",
                "type": "Note",
                "attributedTo": &newcomer.ap_id,
                "content": "<p><a class=\"mention\" href=\"https://test.example/users/bob\">@bob</a></p>",
            },
        });
        let mention_view = ActivityView::parse(&mentioned).expect("the fixture parses");
        assert_eq!(
            filter_incoming(state, &mention_view).await,
            MrfDecision::Accept(mentioned),
            "a mention anchor is not a link"
        );
    }

    /// The whole pipeline with `tag` enabled: the preloaded target tags
    /// refuse a remote `Follow` of the tagged local account.
    #[tokio::test]
    async fn the_incoming_filter_rejects_a_follow_of_a_tagged_local_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_mrf_entry(&pool, ":mrf", &json!({"policies": ["tag"]}))
            .await
            .expect("the setting writes");
        let alice = preloaded_author(&pool).await;
        let carol = preloaded_target(&pool).await;
        let state = test_state(&pool);

        let follow = follow_of(&alice.ap_id, &carol.ap_id);
        let view = ActivityView::parse(&follow).expect("the fixture parses");
        assert_eq!(
            filter_incoming(state, &view).await,
            MrfDecision::Reject(format!(
                "[TagPolicy] Follow from {} tagged with mrf_tag:disable-any-subscription",
                alice.ap_id
            )),
            "the preloaded target tags reach the policy"
        );
    }
}
