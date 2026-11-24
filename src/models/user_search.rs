//! Account search: the query behind `GET /api/v1/accounts/search` and the
//! accounts arm of `GET /api/v1/search` and `GET /api/v2/search`
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! Reference: Pleroma's `User.Search.search`
//! (`lib/pleroma/user/search.ex`). This ticket implements its account side
//! in the shape the endpoint's ticket pins: the query is normalised (the
//! leading `@` stripped, lowercased), an account matches when its nickname
//! or display name starts with the query (`ILIKE` prefix) or is
//! trigram-similar to it (the `pg_trgm` `%` operator at the 0.3 threshold,
//! over the two trigram indexes the users migration builds), and a query
//! that looks like `nick@domain` also matches that exact nickname — the
//! handle of a remote mirror, stored whole by decision 7. The `%` operator
//! reads its threshold from a GUC, so every search runs in a transaction
//! that sets `pg_trgm.similarity_threshold` locally.
//!
//! The rows that survive a search are the active accounts
//! ([`is_active`](User::is_active), Pleroma's `filter_deactivated_users`).
//! A local account must also be approved and email-confirmed
//! ([`approval_pending`](User::approval_pending) unset,
//! [`email_confirmed`](User::email_confirmed) set), as Mastodon's
//! `Account.searchable` is `without_unapproved.without_suspended`
//! (`app/models/account.rb`); a remote mirror is exempt from that test. No
//! `Application` actor (a relay, the instance actor) is ever listed, and the
//! instance actor ([`NOT_INSTANCE_ACTOR_U`]) is dropped by nickname as well,
//! as Pleroma's `filter_internal_users` drops internal actors. For an authenticated
//! viewer, accounts that blocked the viewer never appear, and the viewer's
//! own blocks, blocked domains (Pleroma's `filter_blocked_domains`,
//! `lib/pleroma/user/search.ex`; the `user_domain_blocks` timelines honour)
//! and unexpired mutes are hidden too — unless `following_only` asks for
//! the viewer's following list instead, where neither a mute nor a blocked
//! domain hides the row (the list is the viewer's own) and only *accepted*
//! follows are in at all.
//!
//! The order is the ranking the ticket fixes: exact nickname match first,
//! then the accounts the viewer follows, then local accounts before remote
//! mirrors, then trigram similarity descending; `id DESC` breaks the final
//! tie so `offset` pages cannot straddle an unstable order (decision 4).
//!
//! [`SearchOpts::resolve`] is accepted and ignored here: the endpoint reads
//! that flag itself and prepends whatever it fetches to the ranked page this
//! query answers (see [`crate::api::search`]), which is where Pleroma folds
//! its resolved id into `top_user_ids` before the same page runs.

use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{
    error::AppResult,
    models::{
        block::push_not_blocked,
        mute::push_not_muted,
        user::{NOT_INSTANCE_ACTOR_U, User},
    },
    util::like,
};

/// The knobs of one [`search_accounts`] call: the page and the two flags
/// the client sends (Pleroma's `search_options` reduced to the account
/// side this ticket implements).
#[derive(Clone, Copy, Debug)]
pub struct SearchOpts {
    /// Rows to answer; the endpoint clamps it to its own default and cap
    /// before building this, and the query treats a negative value as 0.
    pub limit: i64,
    /// Rows to skip; `offset` pagination, the only paging search has (a
    /// ranking has no stable cursor to key on). A negative value reads
    /// as 0.
    pub offset: i64,
    /// `following=true`: restrict the search to the viewer's accepted
    /// follows, and stop excluding the accounts the viewer blocks or mutes
    /// (that list is the viewer's own, so hiding parts of it from them
    /// answers nothing they did not choose to follow).
    pub following_only: bool,
    /// `resolve=true`: accepted and ignored by this module. The endpoint
    /// reads the flag and prepends the account it fetches to this query's
    /// ranked page (see [`crate::api::search`]), where Pleroma's
    /// `maybe_resolve` sits.
    pub resolve: bool,
}

/// The shared head of every [`search_accounts`] statement: active accounts
/// only (Pleroma's `filter_deactivated_users`), never an `Application`
/// actor (the instance actor, relays: Pleroma's `filter_internal_users`),
/// and a local account only once approved and confirmed (Mastodon's
/// `without_unapproved`; remote mirrors carry neither flag's meaning, so
/// they pass whatever the flags say). [`NOT_INSTANCE_ACTOR_U`] and [`MATCH_OPEN_SQL`] follow it.
const BASE_SQL: &str = "SELECT u.* FROM users u WHERE u.is_active \
                        AND u.actor_type <> 'Application' \
                        AND (NOT u.local OR (NOT u.approval_pending AND u.email_confirmed))";

/// The opening of the match clause [`push_match_arms`] fills and closes.
/// The viewer exclusions, the ranking and the page follow, appended in
/// that order.
const MATCH_OPEN_SQL: &str = " AND (";

/// Sets the `pg_trgm` similarity threshold the `%` operator arms of the
/// match clause compare against, locally to the caller's transaction:
/// every search runs in a transaction for this statement alone, so the
/// setting cannot leak into anything else on the connection.
const SET_THRESHOLD_SQL: &str = "SET LOCAL pg_trgm.similarity_threshold = 0.3";

/// The [`follows`] state the `following_only` restriction filters on and
/// the "followed by the viewer" rank class checks: the accepted follow. A
/// `pending` request answers nothing to either — the same reading every
/// other follow-side listing of this repo takes.
///
/// [`follows`]: crate::models::follow
const ACCEPTED_STATE: &str = "accepted";

/// The accounts matching `query` for `viewer`, ranked and paged by `opts`,
/// as rows (rendering them into entities is the endpoint's job).
///
/// A normalised-empty query (nothing but whitespace, or a bare `@`) and
/// `following_only` without a viewer — nobody's following list — answer
/// the empty `Vec` without touching the database. The viewer drives the
/// block and mute exclusions, the following restriction and the "followed"
/// rank class; an anonymous search sees every active matching account.
///
/// `opts.resolve` changes nothing here (see [`SearchOpts`]).
///
/// # Errors
///
/// Any database failure (connection, encoding, the page transaction)
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn search_accounts(
    pool: &PgPool,
    viewer: Option<&User>,
    query: &str,
    opts: &SearchOpts,
) -> AppResult<Vec<User>> {
    let Some(needle) = normalize_query(query) else {
        return Ok(Vec::new());
    };
    if opts.following_only && viewer.is_none() {
        return Ok(Vec::new());
    }
    let mut tx = pool.begin().await?;
    sqlx::query(SET_THRESHOLD_SQL)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    let mut builder = QueryBuilder::<Postgres>::new(BASE_SQL);
    let _head: &mut _ = builder.push(NOT_INSTANCE_ACTOR_U).push(MATCH_OPEN_SQL);
    push_match_arms(&mut builder, &needle);
    push_exclusions(&mut builder, viewer, opts.following_only);
    push_ranking(&mut builder, &needle, viewer);
    push_page(&mut builder, opts);
    let users = builder.build_query_as::<User>().fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(users)
}

/// The normalised form of a client's query: trimmed, the leading `@`
/// stripped (Pleroma's `String.trim_leading(query, "@")`, so a handle
/// typed with its sigil searches like one without), lowercased, and
/// `None` when nothing usable is left.
fn normalize_query(query: &str) -> Option<String> {
    let trimmed = query.trim();
    let stripped = trimmed.strip_prefix('@').unwrap_or(trimmed).trim();
    let lowered = stripped.to_lowercase();
    (!lowered.is_empty()).then_some(lowered)
}

/// Whether a normalised query spells a full `nick@domain` handle: the
/// nickname of a remote mirror (decision 7), and the flag that adds the
/// exact nickname arm to the match clause.
fn is_full_handle(needle: &str) -> bool {
    needle.contains('@')
}

/// Fills the match clause [`BASE_SQL`] opened and closes it: nickname or
/// display name starting with the query, or trigram-similar to it at the
/// threshold the transaction set (the two `%` arms ride the
/// `users_nickname_trgm_idx` and `users_name_trgm_idx` indexes, which is
/// why the citext nickname is cast exactly the way the index expression
/// is), and — for a `nick@domain` query — the exact nickname.
fn push_match_arms(builder: &mut QueryBuilder<Postgres>, needle: &str) {
    let pattern = like::prefix(needle);
    let _prefix: &mut _ = builder
        .push("u.nickname::text ILIKE ")
        .push_bind(pattern.clone())
        .push(" OR u.name ILIKE ")
        .push_bind(pattern);
    if is_full_handle(needle) {
        let _exact: &mut _ = builder
            .push(" OR lower(u.nickname::text) = ")
            .push_bind(needle.to_owned());
    }
    let _trigram: &mut _ = builder
        .push(" OR u.nickname::text % ")
        .push_bind(needle.to_owned())
        .push(" OR u.name % ")
        .push_bind(needle.to_owned());
    let _closed: &mut _ = builder.push(")");
}

/// Appends the per-viewer `WHERE` arms: the accounts that blocked the
/// viewer are always out; without `following_only` the viewer's own
/// blocks and unexpired mutes are out too (an expired mute is a row no
/// read counts, the same test every other mute read of this repo
/// applies); with it the search is restricted to the viewer's accepted
/// follows instead. An anonymous viewer adds no condition: nobody's
/// relationships hide rows from a stranger.
fn push_exclusions(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>, restricted: bool) {
    let Some(viewer_id) = viewer.map(|found| found.id) else {
        return;
    };
    let _blocked_the_viewer: &mut _ = builder
        .push(
            " AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.blocker_id = u.id AND b.blocked_id = ",
        )
        .push_bind(viewer_id)
        .push(")");
    if restricted {
        let _followed: &mut _ = builder
            .push(" AND EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = ")
            .push_bind(viewer_id)
            .push(" AND f.followee_id = u.id AND f.state = ")
            .push_bind(ACCEPTED_STATE)
            .push(")");
        return;
    }
    push_not_blocked(builder, viewer_id, &["u.id"]);
    let _blocked_domain: &mut _ = builder
        .push(" AND NOT EXISTS (SELECT 1 FROM user_domain_blocks udb WHERE udb.user_id = ")
        .push_bind(viewer_id)
        .push(" AND udb.domain = u.domain)");
    push_not_muted(builder, viewer_id, &["u.id"]);
}

/// Appends the `ORDER BY` of the ticket's ranking: exact nickname match,
/// then the accounts the viewer follows, then local accounts, then the
/// greater of the two trigram similarities, then `id DESC` as the stable
/// tiebreak offset paging needs. An anonymous viewer has nobody followed,
/// so that arm is a constant `FALSE` and the class falls out.
fn push_ranking(builder: &mut QueryBuilder<Postgres>, needle: &str, viewer: Option<&User>) {
    let _exact: &mut _ = builder
        .push(" ORDER BY (lower(u.nickname::text) = ")
        .push_bind(needle.to_owned())
        .push(") DESC, ");
    match viewer.map(|found| found.id) {
        Some(viewer_id) => {
            let _followed: &mut _ = builder
                .push("EXISTS (SELECT 1 FROM follows f2 WHERE f2.follower_id = ")
                .push_bind(viewer_id)
                .push(" AND f2.followee_id = u.id AND f2.state = ")
                .push_bind(ACCEPTED_STATE)
                .push(") DESC, ");
        }
        None => {
            // Postgres reads a bare (even parenthesized) constant in
            // `ORDER BY` as an ordinal position and refuses one that is
            // not an integer; the cast spells the boolean value instead.
            // An anonymous viewer follows nobody, so the arm keeps every
            // row in one class and only holds the order of the keys after
            // it.
            let _no_viewer: &mut _ = builder.push("FALSE::boolean DESC, ");
        }
    }
    let _ranked: &mut _ = builder
        .push("u.local DESC, greatest(similarity(u.nickname::text, ")
        .push_bind(needle.to_owned())
        .push("), similarity(u.name, ")
        .push_bind(needle.to_owned())
        .push(")) DESC, u.id DESC");
}

/// Appends the page: the `LIMIT` and `OFFSET` of [`SearchOpts`], each
/// floored at 0 so a negative cannot be a database error (the endpoint
/// clamps through [`PageParams`](crate::pagination::PageParams) before
/// building these options; the floor is the model's own guard).
fn push_page(builder: &mut QueryBuilder<Postgres>, opts: &SearchOpts) {
    let _paged: &mut _ = builder
        .push(" LIMIT ")
        .push_bind(opts.limit.max(0))
        .push(" OFFSET ")
        .push_bind(opts.offset.max(0));
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            block,
            domain_block::block_domain,
            follow,
            mute::{self, MuteOptions},
            user::{Flag, INSTANCE_ACTOR_NICKNAME, NewUser, insert},
        },
        test_fixtures::{local_new, local_user as user, remote_user as remote},
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;

    /// A local user named `nickname` with display name `name`.
    async fn named(pool: &PgPool, nickname: &str, name: &str) -> User {
        insert(
            pool,
            &NewUser {
                name: name.to_owned(),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the named fixture inserts")
    }

    /// The ids of `users`, in the order they came back.
    fn ids(users: &[User]) -> Vec<Uuid> {
        users.iter().map(|found| found.id).collect()
    }

    /// The options of a full page over `query` (anonymous, unrestricted).
    fn opts(limit: i64, offset: i64) -> SearchOpts {
        SearchOpts {
            limit,
            offset,
            following_only: false,
            resolve: false,
        }
    }

    /// The full page of a query for `viewer`: every match, in the ranking.
    async fn search(pool: &PgPool, viewer: Option<&User>, query: &str) -> Vec<User> {
        search_accounts(pool, viewer, query, &opts(80, 0))
            .await
            .expect("the search runs")
    }

    /// The unrestricted query as an anonymous request.
    async fn paged(pool: &PgPool, query: &str, limit: i64, offset: i64) -> AppResult<Vec<User>> {
        search_accounts(pool, None, query, &opts(limit, offset)).await
    }

    /// The query restricted to `viewer`'s following list.
    async fn following_only(pool: &PgPool, viewer: &User, query: &str) -> Vec<User> {
        search_accounts(
            pool,
            Some(viewer),
            query,
            &SearchOpts {
                limit: 80,
                offset: 0,
                following_only: true,
                resolve: false,
            },
        )
        .await
        .expect("the restricted search runs")
    }

    /// Records `follower`'s accepted follow of `target`.
    async fn follow_pair(pool: &PgPool, follower: &User, target: &User) {
        let row = follow::follow(pool, BASE, follower, target)
            .await
            .expect("the fixture follow records");
        assert_eq!(
            row.state,
            follow::FollowState::Accepted,
            "an open local account answers on the spot"
        );
    }

    /// Records `blocker`'s block of `target`.
    async fn block_pair(pool: &PgPool, blocker: &User, target: &User) {
        let row = block::block(pool, BASE, blocker, target)
            .await
            .expect("the fixture block writes");
        assert_eq!(row.blocked_id, target.id, "the fixture block landed");
    }

    /// Records `muter`'s mute of `target`.
    async fn mute_pair(pool: &PgPool, muter: &User, target: &User) {
        let row = mute::mute(pool, muter, target, MuteOptions::default())
            .await
            .expect("the fixture mute writes");
        assert_eq!(row.muted_id, target.id, "the fixture mute landed");
    }

    #[test]
    fn a_query_is_trimmed_de_atted_and_lowercased() {
        assert_eq!(
            normalize_query("  @Alice  ").as_deref(),
            Some("alice"),
            "the sigil goes and the case folds"
        );
        assert_eq!(
            normalize_query("BOB@Remote.Example").as_deref(),
            Some("bob@remote.example"),
            "the domain part folds too"
        );
        assert_eq!(
            normalize_query("@@x").as_deref(),
            Some("@x"),
            "one sigil is stripped and the rest is the query"
        );
        for blank in ["", "   ", "@", " @ "] {
            assert_eq!(normalize_query(blank), None, "`{blank}` is no query");
        }
    }

    #[test]
    fn is_full_handle_spots_a_nick_at_domain() {
        assert!(is_full_handle("bob@remote.example"));
        assert!(!is_full_handle("bob"), "the leading @ was already stripped");
    }

    #[tokio::test]
    async fn a_prefix_matches_nickname_and_display_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let zaphod = named(&pool, "zz", "Zaphod Beeblebrox").await;

        assert_eq!(ids(&search(&pool, None, "ali").await), vec![alice.id]);
        assert_eq!(
            ids(&search(&pool, None, "zaph").await),
            vec![zaphod.id],
            "the display name carries the prefix the nickname does not"
        );
        assert_eq!(
            ids(&search(&pool, None, "@ALICE").await),
            vec![alice.id],
            "the sigil and the case are normalised away"
        );
        assert!(
            search(&pool, None, "carol").await.is_empty(),
            "nobody matches a name nobody has"
        );
    }

    #[tokio::test]
    async fn a_trigram_typo_matches_beyond_the_prefix() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        assert!(
            !alice.nickname.starts_with("alce"),
            "the query is not a prefix of the nickname"
        );

        assert_eq!(
            ids(&search(&pool, None, "alce").await),
            vec![alice.id],
            "the 0.5 similarity of alice/alce is over the 0.3 threshold"
        );
        assert!(
            search(&pool, None, "zzzz").await.is_empty(),
            "dissimilar noise matches nothing"
        );
    }

    #[tokio::test]
    async fn a_full_handle_query_matches_the_remote_nickname() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let eve = remote(&pool, "eve").await;
        let evelyn = user(&pool, "evelyn").await;

        let seen = ids(&search(&pool, None, "eve@remote.example").await);
        assert!(seen.contains(&eve.id), "the exact remote handle is in");
        assert!(
            !seen.contains(&evelyn.id),
            "and a merely similar local account is not: {seen:?}"
        );
        assert_eq!(
            ids(&search(&pool, None, "@eve@remote.example").await),
            vec![eve.id],
            "the leading sigil of a typed handle is stripped first"
        );
    }

    #[tokio::test]
    async fn the_ranking_is_exact_followed_local_similarity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let viewer = user(&pool, "viewer").await;
        let followed = user(&pool, "someread").await;
        follow_pair(&pool, &viewer, &followed).await;
        let exact = user(&pool, "so").await;
        let local = user(&pool, "someone").await;
        let mirror = remote(&pool, "someuser").await;

        let seen = ids(&search(&pool, Some(&viewer), "@SO").await);
        assert_eq!(
            seen,
            vec![exact.id, followed.id, local.id, mirror.id],
            "exact nickname first, then the follow, then local, then the mirror"
        );
    }

    #[tokio::test]
    async fn similarity_orders_within_equal_rank_classes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let long = user(&pool, "percival").await;
        let short = user(&pool, "percy").await;

        let seen = ids(&search(&pool, None, "perco").await);
        assert_eq!(
            seen,
            vec![short.id, long.id],
            "both clear the threshold on trigram similarity alone; the closer one leads"
        );
    }

    #[tokio::test]
    async fn following_only_restricts_to_the_viewers_follows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let viewer = user(&pool, "finder").await;
        let followed = user(&pool, "followed").await;
        let muted = user(&pool, "flunky").await;
        let followed_muted = user(&pool, "followmuted").await;
        let unfollowed = user(&pool, "face").await;
        follow_pair(&pool, &viewer, &followed).await;
        follow_pair(&pool, &viewer, &followed_muted).await;
        mute_pair(&pool, &viewer, &muted).await;
        mute_pair(&pool, &viewer, &followed_muted).await;

        let open = ids(&search(&pool, Some(&viewer), "f").await);
        for hidden in [muted.id, followed_muted.id] {
            assert!(!open.contains(&hidden), "a mute is out of the open search");
        }
        for kept in [viewer.id, followed.id, unfollowed.id] {
            assert!(open.contains(&kept), "unrestricted search keeps {kept}");
        }

        let restricted = ids(&following_only(&pool, &viewer, "f").await);
        assert_eq!(
            restricted.len(),
            2,
            "exactly the two accounts of the following list: {restricted:?}"
        );
        assert!(
            restricted.contains(&followed.id) && restricted.contains(&followed_muted.id),
            "the mute does not hide a row from its follower's own following search: \
             {restricted:?}"
        );
    }

    #[tokio::test]
    async fn following_only_without_the_right_viewer_answers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let _followed = user(&pool, "followed").await;
        let nobody = user(&pool, "ghostviewer").await;

        assert!(
            following_only(&pool, &nobody, "followed").await.is_empty(),
            "a viewer with no follows has no list to search"
        );
        assert!(
            search_accounts(
                &pool,
                None,
                "followed",
                &SearchOpts {
                    limit: 80,
                    offset: 0,
                    following_only: true,
                    resolve: false,
                },
            )
            .await
            .expect("the anonymous restricted search runs")
            .is_empty(),
            "an anonymous request follows nobody"
        );
    }

    #[tokio::test]
    async fn blocks_are_excluded_for_their_other_side_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let viewer = user(&pool, "viewer").await;
        let stranger = user(&pool, "other").await;
        let bane = user(&pool, "banshee").await;
        let cursed = user(&pool, "banished").await;
        let neutral = user(&pool, "blithe").await;
        block_pair(&pool, &bane, &viewer).await;
        block_pair(&pool, &viewer, &cursed).await;

        assert_eq!(
            ids(&search(&pool, Some(&viewer), "b").await),
            vec![neutral.id],
            "both sides of a block are out of the viewer's search"
        );
        let anonymous = ids(&search(&pool, None, "b").await);
        for hidden in [bane.id, cursed.id, neutral.id] {
            assert!(anonymous.contains(&hidden), "a stranger sees all three");
        }
        let third = ids(&search(&pool, Some(&stranger), "b").await);
        for seen in [bane.id, cursed.id, neutral.id] {
            assert!(
                third.contains(&seen),
                "someone else's block hides nothing from a third account"
            );
        }
    }

    #[tokio::test]
    async fn an_expired_mute_does_not_exclude() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let viewer = user(&pool, "viewer").await;
        let lapsed = user(&pool, "mutedpal").await;
        let timed = mute::mute(
            &pool,
            &viewer,
            &lapsed,
            MuteOptions {
                notifications: false,
                expires_in: Some(60),
            },
        )
        .await
        .expect("the timed mute writes");
        assert_eq!(
            timed.muted_id, lapsed.id,
            "the timed mute is the fixture row"
        );
        let lapsed_rows = sqlx::query(
            "UPDATE mutes SET expires_at = now() - interval '1 hour'
              WHERE muter_id = $1 AND muted_id = $2",
        )
        .bind(viewer.id)
        .bind(lapsed.id)
        .execute(&pool)
        .await
        .expect("the fixture mute lapses");
        assert_eq!(lapsed_rows.rows_affected(), 1, "and it was the mute row");

        assert!(
            ids(&search(&pool, Some(&viewer), "muted").await).contains(&lapsed.id),
            "a lapsed mute is no longer a mute: the account is back"
        );
    }

    #[tokio::test]
    async fn deactivated_accounts_are_never_returned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let gone = user(&pool, "alicida").await;
        let deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .expect("the fixture deactivates");
        assert_eq!(deactivated.rows_affected(), 1, "and it was one row");

        assert_eq!(ids(&search(&pool, None, "ali").await), vec![alice.id]);
        assert_eq!(
            ids(&search(&pool, None, "alic").await),
            vec![alice.id],
            "not even for a query both accounts match by prefix"
        );
    }

    /// A local user named `nickname` awaiting staff approval.
    async fn pending_approval(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                approval_pending: Flag::new(true),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the pending fixture inserts")
    }

    /// A local user named `nickname` whose email is not confirmed.
    async fn unconfirmed(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                email_confirmed: Flag::new(false),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the unconfirmed fixture inserts")
    }

    #[tokio::test]
    async fn an_account_awaiting_approval_is_not_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let approved = user(&pool, "waitapproved").await;
        let _pending = pending_approval(&pool, "waitpending").await;

        assert_eq!(
            ids(&search(&pool, None, "wait").await),
            vec![approved.id],
            "only the approved account is searchable"
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_local_account_is_not_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let confirmed = user(&pool, "mailok").await;
        let _waiting = unconfirmed(&pool, "mailwait").await;

        assert_eq!(
            ids(&search(&pool, None, "mail").await),
            vec![confirmed.id],
            "an unconfirmed address keeps the account out"
        );
    }

    #[tokio::test]
    async fn a_remote_account_is_found_whatever_its_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "ghost@remote.example".to_owned(),
                ap_id: "https://remote.example/users/ghost".to_owned(),
                domain: Some("remote.example".to_owned()),
                approval_pending: Flag::new(true),
                email_confirmed: Flag::new(false),
                ..NewUser::default()
            },
        )
        .await
        .expect("the flagged mirror inserts");

        assert_eq!(
            ids(&search(&pool, None, "ghost").await),
            vec![mirror.id],
            "the registration flags mean nothing for a remote mirror"
        );
    }

    #[tokio::test]
    async fn the_instance_actor_is_never_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = user(&pool, INSTANCE_ACTOR_NICKNAME).await;
        let other = user(&pool, "internalaffairs").await;
        let viewer = user(&pool, "viewer").await;

        assert_eq!(
            ids(&search(&pool, None, "internal").await),
            vec![other.id],
            "the actor is hidden from an anonymous search"
        );
        assert!(
            !ids(&search(&pool, Some(&viewer), "internal.fetch").await).contains(&actor.id),
            "and from an exact-nickname search by a viewer"
        );
    }

    /// A local user named `nickname` with the given `actor_type`.
    async fn actor_of_type(pool: &PgPool, nickname: &str, actor_type: &str) -> User {
        insert(
            pool,
            &NewUser {
                actor_type: actor_type.to_owned(),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the typed actor fixture inserts")
    }

    #[tokio::test]
    async fn an_application_actor_is_never_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let relay = actor_of_type(&pool, "relayhub", "Application").await;
        let person = actor_of_type(&pool, "relayfan", "Person").await;
        let viewer = user(&pool, "viewer").await;

        assert_eq!(
            ids(&search(&pool, None, "relay").await),
            vec![person.id],
            "an anonymous search drops the Application actor and keeps the Person"
        );
        let seen = ids(&search(&pool, Some(&viewer), "relay").await);
        assert_eq!(
            seen,
            vec![person.id],
            "an authenticated search does the same: {seen:?}"
        );
        assert!(
            !ids(&search(&pool, Some(&viewer), "relayhub").await).contains(&relay.id),
            "not even the exact nickname finds it"
        );
    }

    #[tokio::test]
    async fn a_service_actor_is_still_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bot = actor_of_type(&pool, "helperbot", "Service").await;
        let _relay = actor_of_type(&pool, "helperrelay", "Application").await;

        assert_eq!(
            ids(&search(&pool, None, "helper").await),
            vec![bot.id],
            "a bot account is a Service actor and stays searchable"
        );
    }

    #[tokio::test]
    async fn a_viewer_does_not_find_accounts_on_a_domain_they_blocked() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let viewer = user(&pool, "viewer").await;
        let other = user(&pool, "bystander").await;
        let blocked = remote(&pool, "zed").await;
        block_domain(&pool, viewer.id, "remote.example")
            .await
            .expect("the fixture domain block writes");

        assert!(
            search(&pool, Some(&viewer), "zed").await.is_empty(),
            "the blocked domain's account is out of the viewer's search"
        );
        assert_eq!(
            ids(&search(&pool, Some(&other), "zed").await),
            vec![blocked.id],
            "another viewer still finds it"
        );
        assert_eq!(
            ids(&search(&pool, None, "zed").await),
            vec![blocked.id],
            "so does an anonymous search"
        );
        assert_eq!(
            ids(&following_only(&pool, &viewer, "zed").await),
            Vec::<Uuid>::new(),
            "the following search still needs an accepted follow"
        );
    }

    #[tokio::test]
    async fn limit_and_offset_walk_the_ranked_page() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        for nickname in ["seaa", "seab", "seac", "sead", "seae"] {
            let _match = user(&pool, nickname).await;
        }
        let all = ids(&search(&pool, None, "sea").await);
        assert_eq!(all.len(), 5, "five matches on the first page");

        let first = paged(&pool, "sea", 2, 0).await.expect("page one");
        assert_eq!(
            ids(&first),
            all[..2],
            "the same ranking, cut to the requested size"
        );
        let tail = paged(&pool, "sea", 2, 4).await.expect("the tail");
        assert_eq!(ids(&tail), all[4..], "and the last row of the walk");
        assert!(
            paged(&pool, "sea", 2, 99)
                .await
                .expect("past the end is no error")
                .is_empty(),
            "an offset beyond the results answers nothing"
        );
        assert!(
            paged(&pool, "sea", -1, -1)
                .await
                .expect("a negative page is no error either")
                .is_empty(),
            "a negative limit or offset floors at zero"
        );
    }

    #[tokio::test]
    async fn an_empty_query_answers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let _alice = user(&pool, "alice").await;
        for blank in ["", "   ", "@", " @ "] {
            assert!(
                search(&pool, None, blank).await.is_empty(),
                "`{blank}` is no query at all"
            );
        }
    }

    #[tokio::test]
    async fn resolve_is_accepted_and_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let seen = search_accounts(
            &pool,
            None,
            "ali",
            &SearchOpts {
                limit: 80,
                offset: 0,
                following_only: false,
                resolve: true,
            },
        )
        .await
        .expect("the resolving flag runs");
        assert_eq!(ids(&seen), vec![alice.id], "and answers the stored rows");
    }
}
