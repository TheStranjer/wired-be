//! Batch loading of the [`Relationship`] entity from the `follows` table.
//!
//! [`Relationship::load_many`] answers the relationship flags of one viewer
//! to many targets in a fixed, bounded set of queries: every source of flags
//! is exactly one `WHERE … = ANY(…)` statement, executed once per call
//! whatever the number of targets, so there is no N+1 anywhere in this
//! module. The `FlagSources` struct mirrors that contract — one field per
//! source query — and `flag_sources` is the single place the set runs.
//!
//! The `follows` table (owned by [`crate::models::follow`]) supplies the
//! flags this ticket computes: `following`, `requested` and
//! `showing_reblogs` / `notifying` from the viewer's own follow rows, and
//! `followed_by` from the accepted incoming rows. The `account_notes`
//! table (owned by [`crate::models::account_note`]) supplies `note`: the
//! viewer's stored comment, the empty string where they have none. Every
//! other flag of the entity is a documented default until its own ticket
//! lands; each one then adds exactly one field to `FlagSources`, one line
//! (one query) to `flag_sources` and one flag line to `assemble`:
//!
//! - the blocks ticket: `blocking`, `blocked_by` (one query over `blocks`);
//! - the mutes ticket: `muting`, `muting_notifications` (one query over
//!   `mutes`);
//! - the endorsements ticket: `endorsed` (one query);
//! - the domain-blocks ticket: `domain_blocking` (one query).

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    entities::{account::JsonBool, relationship::Relationship},
    error::AppResult,
    models::{account_note, follow::FollowState},
};

/// [`load_many`](Relationship::load_many)'s outgoing side: the viewer's own
/// follow rows for every requested target, with the two per-follow toggles
/// the entity carries. One statement, one fetch, any number of targets.
const OUTGOING_SQL: &str = "SELECT followee_id AS target_id, state, show_reblogs, notify
      FROM follows
     WHERE follower_id = $1 AND followee_id = ANY($2)";

/// [`load_many`](Relationship::load_many)'s incoming side: which of the
/// requested targets have an accepted follow of the viewer — the whole
/// answer behind `followed_by`.
const INCOMING_SQL: &str = "SELECT follower_id AS target_id
      FROM follows
     WHERE followee_id = $1 AND follower_id = ANY($2) AND state = 'accepted'";

/// One `follows` row of the viewer's, narrowed to the columns that feed
/// relationship flags. The full row type lives in
/// [`crate::models::follow::Follow`]; this is only ever a query result.
#[derive(Debug, sqlx::FromRow)]
struct OutgoingFollow {
    /// The followed account: `follows.followee_id`, aliased to the target
    /// role it always plays here.
    target_id: Uuid,
    /// Lifecycle state of the row: decides `following` and `requested`.
    state: FollowState,
    /// Stored `show_reblogs` column: the `showing_reblogs` flag.
    show_reblogs: bool,
    /// Stored `notify` column: the `notifying` flag (and its Pleroma alias
    /// `subscribing`).
    notify: bool,
}

/// The loaded answer of every flag source for one batch, one field per
/// source query (see the [module docs](self) for the tickets that extend
/// it). A flag whose source is absent here keeps the default
/// [`Relationship::unconnected`] gives it.
#[derive(Debug)]
struct FlagSources {
    /// The viewer's follow rows for the targets, keyed by target id:
    /// decides `following`, `requested`, `showing_reblogs`, `notifying`
    /// and `subscribing`. Loaded by [`outgoing_follows`] — one query.
    outgoing: HashMap<Uuid, OutgoingFollow>,
    /// The targets with an accepted follow of the viewer: decides
    /// `followed_by`. Loaded by [`incoming_follows`] — one query.
    followed_by: HashSet<Uuid>,
    /// The viewer's stored notes, keyed by target: decides `note`. Loaded
    /// by [`account_note::get_many`] — one query. Targets without a stored
    /// note are absent, and [`assemble`] renders them the empty string.
    notes: HashMap<Uuid, String>,
}

/// The single inherent implementation of the entity: the constructor that
/// holds its defaults and the batch loader that computes the rest. It
/// lives here rather than beside the struct because every query belongs
/// to the models layer (decision 3), and the crate denies splitting a
/// type's inherent blocks across modules (`multiple_inherent_impl`).
impl Relationship {
    /// The relationship of a viewer with nothing recorded about
    /// `target_id`: every flag `false` — except
    /// [`showing_reblogs`](Self::showing_reblogs), which only a stored
    /// hiding can make `false` — and an empty
    /// [`note`](Self::note). [`load_many`](Self::load_many) starts from
    /// this and overwrites exactly the flags its sources computed.
    #[must_use]
    pub fn unconnected(target_id: Uuid) -> Self {
        Self {
            id: target_id.to_string(),
            following: JsonBool(false),
            requested: JsonBool(false),
            endorsed: JsonBool(false),
            followed_by: JsonBool(false),
            blocking: JsonBool(false),
            blocked_by: JsonBool(false),
            muting: JsonBool(false),
            muting_notifications: JsonBool(false),
            notifying: JsonBool(false),
            showing_reblogs: JsonBool(true),
            domain_blocking: JsonBool(false),
            note: String::new(),
            subscribing: JsonBool(false),
        }
    }

    /// Loads the relationship of `viewer_id` to each of `target_ids`, one
    /// entity per requested id, in the requested order (a repeated id is
    /// answered once per occurrence).
    ///
    /// The whole answer costs one fixed set of queries — see
    /// `flag_sources` — never a query per target. Ids that name no user
    /// are not an error: a relationship is just the (default) flags, and
    /// deciding what an unknown target means for a caller (skipping it,
    /// 404ing) is the endpoint's job, not the loader's. An empty `target_ids`
    /// answers an empty `Vec` without touching the database.
    ///
    /// # Errors
    ///
    /// Any database failure (connection, encoding) arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn load_many(
        pool: &PgPool,
        viewer_id: Uuid,
        target_ids: &[Uuid],
    ) -> AppResult<Vec<Self>> {
        if target_ids.is_empty() {
            return Ok(Vec::new());
        }
        let sources = flag_sources(pool, viewer_id, target_ids).await?;
        Ok(target_ids
            .iter()
            .map(|&target_id| assemble(target_id, &sources))
            .collect())
    }
}

/// Runs the fixed query set behind [`Relationship::load_many`] and keeps
/// the results together. Every flag source is exactly one batched query
/// awaited once here; the tickets listed in the [module docs](self) add
/// their one line (and their one [`FlagSources`] field) to this list.
async fn flag_sources(
    pool: &PgPool,
    viewer_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<FlagSources> {
    Ok(FlagSources {
        // The follows table (this ticket): one query per direction.
        outgoing: outgoing_follows(pool, viewer_id, target_ids).await?,
        followed_by: incoming_follows(pool, viewer_id, target_ids).await?,
        // The account_notes table: one batched read of the viewer's notes.
        notes: account_note::get_many(pool, viewer_id, target_ids).await?,
    })
}

/// The viewer's own follow rows for `target_ids`, keyed by target. One
/// statement, whatever the batch size.
async fn outgoing_follows(
    pool: &PgPool,
    viewer_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, OutgoingFollow>> {
    let rows = sqlx::query_as::<_, OutgoingFollow>(OUTGOING_SQL)
        .bind(viewer_id)
        .bind(target_ids.to_vec())
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|row| (row.target_id, row)).collect())
}

/// The ids among `target_ids` with an accepted follow of `viewer_id` —
/// the whole `followed_by` answer, in one statement.
async fn incoming_follows(
    pool: &PgPool,
    viewer_id: Uuid,
    target_ids: &[Uuid],
) -> AppResult<HashSet<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(INCOMING_SQL)
        .bind(viewer_id)
        .bind(target_ids.to_vec())
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(target_id,)| target_id).collect())
}

/// Folds the loaded sources into the entity for one target: the six flags
/// the follows table answers and the [`note`] the notes table holds take
/// their sources' values, every other flag keeps the default
/// [`Relationship::unconnected`] states.
///
/// [`note`]: Relationship::note
fn assemble(target_id: Uuid, sources: &FlagSources) -> Relationship {
    let follow = sources.outgoing.get(&target_id);
    let following = follow.is_some_and(|row| row.state == FollowState::Accepted);
    let requested = !following && follow.is_some_and(|row| row.state == FollowState::Pending);
    let notifying = follow.is_some_and(|row| row.notify);
    Relationship {
        following: JsonBool(following),
        requested: JsonBool(requested),
        followed_by: JsonBool(sources.followed_by.contains(&target_id)),
        notifying: JsonBool(notifying),
        subscribing: JsonBool(notifying),
        showing_reblogs: JsonBool(follow.is_none_or(|row| row.show_reblogs)),
        note: sources.notes.get(&target_id).cloned().unwrap_or_default(),
        ..Relationship::unconnected(target_id)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            account_note, follow,
            user::{Flag, NewUser, User, insert, local_ap_id},
        },
        util::id::new_id,
    };

    /// The two per-follow toggles [`set_toggles`] stores, passed as a
    /// struct so the helper does not read as a pair of switch parameters.
    #[derive(Debug)]
    struct Toggles {
        /// Value to store in `show_reblogs`.
        show_reblogs: bool,
        /// Value to store in `notify`.
        notify: bool,
    }

    /// Inserts a local, unlocked user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a local user named `nickname` whose account is locked, so a
    /// follow of it waits for an answer.
    async fn locked_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the locked fixture inserts")
    }

    /// The relationship [`Relationship::load_many`] answers for `viewer`
    /// to `target`, which the test expects to be the only row out.
    async fn one_relationship(pool: &PgPool, viewer: &User, target: &User) -> Relationship {
        let loaded = Relationship::load_many(pool, viewer.id, &[target.id])
            .await
            .unwrap();
        assert_eq!(loaded.len(), 1, "one target in answers one row out");
        let relationship = loaded.first().expect("the batch answered one row");
        assert_eq!(
            relationship.id,
            target.id.to_string(),
            "the row is about the requested target"
        );
        relationship.clone()
    }

    /// Stores `toggles` on the `viewer → target` row the way the (later)
    /// follow-actions ticket's endpoints will: [`follow::follow`] itself
    /// only ever writes the column defaults.
    async fn set_toggles(pool: &PgPool, viewer: &User, target: &User, toggles: Toggles) {
        let updated = sqlx::query(
            "UPDATE follows SET show_reblogs = $3, notify = $4, updated_at = now()
              WHERE follower_id = $1 AND followee_id = $2",
        )
        .bind(viewer.id)
        .bind(target.id)
        .bind(toggles.show_reblogs)
        .bind(toggles.notify)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(
            updated.rows_affected(),
            1,
            "the fixture row is there to update"
        );
    }

    /// Hides reblogs and turns notifications on: the non-default side of
    /// both toggles the loader must read off the viewer's row.
    fn flipped() -> Toggles {
        Toggles {
            show_reblogs: false,
            notify: true,
        }
    }

    /// Writes a `rejected` row by hand: [`follow::reject`] deletes the row
    /// rather than storing that state, so only a mirrored remote rejection
    /// produces one (see the [`crate::models::follow`] docs).
    async fn rejected_relation(pool: &PgPool, follower: &User, target: &User) {
        let written = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state)
             VALUES ($1, $2, $3, 'rejected')",
        )
        .bind(new_id())
        .bind(follower.id)
        .bind(target.id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(written.rows_affected(), 1, "the rejected fixture row lands");
    }

    /// Asserts the flags no source has computed (yet) keep their defaults:
    /// the constants the ticket promises until each flag's own ticket
    /// stores its data. `note` is computed now, but these fixtures store
    /// no note, so its source answers the empty default here too.
    fn assert_placeholder_flags(relationship: &Relationship) {
        assert!(!relationship.endorsed.0, "endorsements have no table yet");
        assert!(
            !relationship.blocking.0,
            "blocks have no table yet, outgoing"
        );
        assert!(
            !relationship.blocked_by.0,
            "blocks have no table yet, incoming"
        );
        assert!(!relationship.muting.0, "mutes have no table yet");
        assert!(
            !relationship.muting_notifications.0,
            "notification mutes have no table yet"
        );
        assert!(
            !relationship.domain_blocking.0,
            "domain blocks have no table yet"
        );
        assert!(
            relationship.note.is_empty(),
            "no stored note reads back empty"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn nothing_recorded_answers_the_default_flags(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(!relationship.following.0, "no row, no follow");
        assert!(!relationship.requested.0, "no row, no request");
        assert!(!relationship.followed_by.0, "no row, no follower");
        assert!(
            !relationship.notifying.0,
            "notifying defaults to false with no row"
        );
        assert!(
            !relationship.subscribing.0,
            "the alias carries the same default"
        );
        assert!(
            relationship.showing_reblogs.0,
            "with no row nothing was ever hidden"
        );
        assert_placeholder_flags(&relationship);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_accepted_follow_computes_following_and_the_toggles(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(
            granted.state,
            FollowState::Accepted,
            "an open local account answers on the spot"
        );

        let defaults = one_relationship(&pool, &alice, &bob).await;
        assert!(defaults.following.0, "the accepted row follows");
        assert!(!defaults.requested.0, "and nothing is being asked");
        assert!(
            defaults.showing_reblogs.0,
            "the row's stored show_reblogs default"
        );
        assert!(!defaults.notifying.0, "the row's stored notify default");

        set_toggles(&pool, &alice, &bob, flipped()).await;
        let toggled = one_relationship(&pool, &alice, &bob).await;
        assert!(toggled.following.0, "toggling does not unfollow");
        assert!(
            !toggled.showing_reblogs.0,
            "show_reblogs rides on the viewer's own row"
        );
        assert!(toggled.notifying.0, "so does notify");
        assert!(
            toggled.subscribing.0,
            "and Pleroma's alias carries the same value"
        );
        assert!(
            !toggled.followed_by.0,
            "following one way does not compute the other"
        );
        assert_placeholder_flags(&toggled);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_pending_request_is_requested_but_not_following(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let waiting = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .unwrap();
        assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");

        let relationship = one_relationship(&pool, &alice, &carol).await;
        assert!(relationship.requested.0, "the ask is visible");
        assert!(!relationship.following.0, "but not granted");
        assert!(!relationship.followed_by.0, "and not in reverse");
        assert!(
            relationship.showing_reblogs.0,
            "a waiting row still carries the show_reblogs default"
        );
        assert!(!relationship.notifying.0, "and the notify default");
        assert_placeholder_flags(&relationship);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_accepted_incoming_follow_computes_followed_by(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let incoming = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .unwrap();
        assert_eq!(
            incoming.state,
            FollowState::Accepted,
            "bob's follow of alice is granted"
        );

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(relationship.followed_by.0, "bob follows alice");
        assert!(
            !relationship.following.0,
            "and alice does not follow bob back from that"
        );
        assert!(
            !relationship.requested.0,
            "an incoming follow is not an outgoing request"
        );
        assert!(
            relationship.showing_reblogs.0,
            "the incoming row cannot set alice's own toggles"
        );
        assert!(!relationship.notifying.0, "nor her notify");
        assert_placeholder_flags(&relationship);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_mutual_follow_computes_both_directions(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let outgoing = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(
            outgoing.state,
            FollowState::Accepted,
            "the outgoing leg is granted"
        );
        let incoming = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .unwrap();
        assert_eq!(
            incoming.state,
            FollowState::Accepted,
            "and so is the incoming one"
        );
        set_toggles(&pool, &alice, &bob, flipped()).await;

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(relationship.following.0, "alice follows bob");
        assert!(relationship.followed_by.0, "and is followed back");
        assert!(!relationship.requested.0, "nothing waits");
        assert!(
            !relationship.showing_reblogs.0,
            "the viewer's own row sets the toggles"
        );
        assert!(relationship.notifying.0, "on both aliases");
        assert!(relationship.subscribing.0, "on both aliases");
        assert_placeholder_flags(&relationship);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rejected_rows_compute_no_following_flags(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        rejected_relation(&pool, &alice, &bob).await;
        rejected_relation(&pool, &bob, &alice).await;

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(
            !relationship.following.0,
            "a rejected row is not an accepted follow"
        );
        assert!(!relationship.requested.0, "nor a pending request");
        assert!(
            !relationship.followed_by.0,
            "and the incoming side is not accepted either"
        );
        assert!(
            relationship.showing_reblogs.0,
            "the rejected row only carries column defaults"
        );
        assert_placeholder_flags(&relationship);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_stored_note_rides_the_relationship(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(granted.state, FollowState::Accepted, "the fixture follows");
        account_note::set(&pool, alice.id, bob.id, "colleague from work")
            .await
            .unwrap();
        account_note::set(&pool, bob.id, alice.id, "who?")
            .await
            .unwrap();

        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert_eq!(
            relationship.note, "colleague from work",
            "the viewer's own note on the target loads"
        );
        assert!(relationship.following.0, "with the follow flags beside it");

        let loaded = Relationship::load_many(&pool, alice.id, &[bob.id, carol.id])
            .await
            .unwrap();
        assert_eq!(loaded.len(), 2, "one answer per requested id");
        assert_eq!(loaded[0].note, "colleague from work", "noted target");
        assert!(loaded[1].note.is_empty(), "unnoted target keeps empty");

        let bobs_view = one_relationship(&pool, &bob, &alice).await;
        assert_eq!(
            bobs_view.note, "who?",
            "and bob sees his own note, not alice's"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_cleared_note_leaves_the_relationship(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        account_note::set(&pool, alice.id, bob.id, "here for a while")
            .await
            .unwrap();
        assert_eq!(
            one_relationship(&pool, &alice, &bob).await.note,
            "here for a while",
            "the note loads while it is stored"
        );

        account_note::set(&pool, alice.id, bob.id, "")
            .await
            .unwrap();
        let relationship = one_relationship(&pool, &alice, &bob).await;
        assert!(
            relationship.note.is_empty(),
            "the empty write deleted the row, and the loader reads the default"
        );
        assert!(
            !relationship.following.0,
            "and nothing else about the pair changed"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn results_keep_the_requested_order_in_one_query_set(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = locked_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        let accepted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .unwrap();
        assert_eq!(accepted.state, FollowState::Accepted, "alice → bob");
        let pending = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
            .await
            .unwrap();
        assert_eq!(pending.state, FollowState::Pending, "alice → carol waits");
        let incoming = follow::follow(&pool, TEST_BASE_URL, &dave, &alice)
            .await
            .unwrap();
        assert_eq!(incoming.state, FollowState::Accepted, "dave → alice");
        // An id no user holds: the loader is not the one deciding to skip
        // it (that is the endpoint's job), it just answers the defaults.
        let unknown = new_id();

        // The batch of mixed states below — outgoing, pending, incoming,
        // unknown, and a repeated id — is answered by the fixed query set:
        // `flag_sources` runs one statement per source exactly once per
        // call, and this asserts every row came back right from those two
        // fetches. There is no per-target query loop in the module to
        // count: one call, one `FlagSources`, N answers.
        let targets = [bob.id, carol.id, unknown, dave.id, bob.id];
        let loaded = Relationship::load_many(&pool, alice.id, &targets)
            .await
            .unwrap();
        assert_eq!(loaded.len(), targets.len(), "one answer per requested id");
        let order: Vec<String> = loaded.iter().map(|row| row.id.clone()).collect();
        let wanted: Vec<String> = targets.iter().map(Uuid::to_string).collect();
        assert_eq!(order, wanted, "the requested order, repeats included");

        let flags = |target: Uuid| {
            let row = loaded
                .iter()
                .find(|relationship| relationship.id == target.to_string())
                .expect("the batch answered the target");
            (row.following.0, row.requested.0, row.followed_by.0)
        };
        assert_eq!(flags(bob.id), (true, false, false), "the accepted follow");
        assert_eq!(flags(carol.id), (false, true, false), "the pending request");
        assert_eq!(flags(dave.id), (false, false, true), "the incoming follow");
        assert_eq!(flags(unknown), (false, false, false), "an unknown target");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_empty_batch_touches_nothing(pool: PgPool) {
        let alice = local_user(&pool, "alice").await;
        let loaded = Relationship::load_many(&pool, alice.id, &[]).await.unwrap();
        assert!(loaded.is_empty(), "no targets, no relationships");
    }
}
