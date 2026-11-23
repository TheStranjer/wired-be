//! The flat (ungrouped) reads: [`NotificationFilters`] and the scope
//! every notification read shares ([`push_scope`], and [`push_visible_scope`]
//! with [`BLOCKED_CAUSES`] and [`MUTED_CAUSES`]), [`FlatPageParams`] and
//! [`page_for_user`], and [`get_visible`] — one row as the listings
//! show it.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams};

use super::{BLOCKED_CAUSES, MUTED_CAUSES, Notification};

/// The client filters every filtered read shares: which types to show,
/// which to hide, and which single actor's notifications to keep. All
/// three are open at their absence.
#[derive(Clone, Debug, Default)]
pub struct NotificationFilters<'a> {
    /// `types[]`: keep only these types; empty keeps all.
    pub types: Vec<&'a str>,
    /// `exclude_types[]`: drop these types; empty drops none.
    pub exclude_types: Vec<&'a str>,
    /// `account_id`: keep only notifications caused by this account.
    pub account_id: Option<Uuid>,
}

impl NotificationFilters<'_> {
    /// The `AND (…)` fragments of the three filters (each skipped when it
    /// asks for nothing), continuing the `WHERE` the caller's statement
    /// already opened on the `n` alias.
    pub(super) fn push(&self, qb: &mut QueryBuilder<Postgres>) {
        if !self.types.is_empty() {
            let _in: &mut _ = qb
                .push(" AND n.type = ANY(")
                .push_bind(self.types.clone())
                .push(")");
        }
        if !self.exclude_types.is_empty() {
            let _out: &mut _ = qb
                .push(" AND n.type <> ALL(")
                .push_bind(self.exclude_types.clone())
                .push(")");
        }
        if let Some(account_id) = self.account_id {
            let _by: &mut _ = qb.push(" AND n.from_account_id = ").push_bind(account_id);
        }
    }
}

/// The scope every notification read shares, as one `WHERE` clause on the
/// `n` alias: the recipient's own, undismissed, from a still-active actor
/// (Pleroma's `for_user_query`).
pub(super) fn push_scope(qb: &mut QueryBuilder<Postgres>, user_id: Uuid) {
    let _scoped: &mut _ = qb.push(" WHERE n.user_id = ").push_bind(user_id).push(
        " AND n.dismissed_at IS NULL AND EXISTS \
             (SELECT 1 FROM users au WHERE au.id = n.from_account_id AND au.is_active)",
    );
}

/// [`push_scope`] plus what the recipient refused: [`BLOCKED_CAUSES`] always
/// and [`MUTED_CAUSES`] unless `with_muted` (Pleroma's `for_user_query` with
/// `exclude_blocked` and `exclude_notification_muted`). Every read a client
/// sees goes through this one helper; only the dismissals stay on the plain
/// scope, so they still reach hidden rows.
pub(super) fn push_visible_scope(qb: &mut QueryBuilder<Postgres>, user_id: Uuid, with_muted: bool) {
    push_scope(qb, user_id);
    let _blocked: &mut _ = qb.push(BLOCKED_CAUSES);
    if !with_muted {
        let _muted: &mut _ = qb.push(MUTED_CAUSES);
    }
}

/// Everything [`page_for_user`] needs: whose notifications, filtered how,
/// whether the recipient's own mutes hide them, and which page of rows.
#[derive(Clone, Debug)]
pub struct FlatPageParams<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The client's `types[]` / `exclude_types[]` / `account_id` filters.
    pub filters: NotificationFilters<'a>,
    /// Whether notifications whose cause the recipient muted — a muted
    /// actor, a muted thread — stay in the page (Pleroma's `:with_muted`,
    /// the endpoint's `with_muted` parameter).
    ///
    /// Blocked actors and blocked domains stay hidden either way.
    pub with_muted: bool,
    /// The cursor page over notification rows, keyed on the row id.
    pub page: PageParams,
}

/// One page of the flat listing: the recipient's notifications, newest row
/// first, under `push_scope`'s read scope (dismissed rows never appear,
/// and neither do an invisible actor's).
///
/// The page is narrowed by the client's [`NotificationFilters`] — `types`
/// restricts to the named types, `exclude_types` removes them,
/// `account_id` keeps one actor, and a type name no row carries simply
/// matches nothing — and by `MUTED_CAUSES` unless the request asked to
/// keep what its reader muted.
///
/// The cursors walk plain id comparison (decision 4) through
/// [`PageParams::apply_keyset`], so a `min_id` page reads forward and comes
/// back flipped like every other listing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    params: &FlatPageParams<'_>,
) -> AppResult<Vec<Notification>> {
    let mut qb = QueryBuilder::<Postgres>::new("SELECT n.* FROM notifications n");
    push_visible_scope(&mut qb, params.user_id, params.with_muted);
    params.filters.push(&mut qb);
    params.page.apply_keyset(&mut qb, "n.id");
    let rows = qb.build_query_as::<Notification>().fetch_all(pool).await?;
    Ok(params.page.finish(rows))
}

/// One notification of `user_id` as the listings show it.
///
/// The owner's, undismissed, from a still-active actor the owner has not
/// blocked (nor whose domain they blocked): `push_visible_scope` narrowed to
/// one row.
///
/// Muted causes stay readable, like v1's `show` (`with_muted` is on).
///
/// `None` says the caller owns no such live row (the unknown, the dismissed,
/// the blocked and the invisible-actor id all answer the shared 404), where
/// [`get`](super::get) reports the row whatever its state.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn get_visible(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
) -> AppResult<Option<Notification>> {
    let mut qb = QueryBuilder::<Postgres>::new("SELECT n.* FROM notifications n");
    push_visible_scope(&mut qb, user_id, true);
    let _one: &mut _ = qb.push(" AND n.id = ").push_bind(id);
    let row = qb
        .build_query_as::<Notification>()
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::{
        notification::{
            dismiss,
            test_support::{note, solo, trio},
        },
        user,
    };

    /// Inserts the mirror of the remote actor `nickname` on `host`.
    async fn remote(pool: &PgPool, nickname: &str, host: &str) -> user::User {
        user::insert(
            pool,
            &user::NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id: format!("https://{host}/users/{nickname}"),
                domain: Some(host.to_owned()),
                name: nickname.to_owned(),
                ..user::NewUser::default()
            },
        )
        .await
        .expect("remote user fixture")
    }

    /// A public status caused by `author`, returning its id.
    async fn post(pool: &PgPool, author: &user::User) -> Uuid {
        use crate::{
            config::TEST_BASE_URL,
            models::status::{self, NewStatus, Visibility},
        };
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Public,
                ..NewStatus::local(author, "a thread")
            },
        )
        .await
        .expect("status fixture")
        .id
    }

    /// The flat page's parameters with everything open but `page`.
    fn flat(user_id: Uuid, page: PageParams) -> FlatPageParams<'static> {
        FlatPageParams {
            user_id,
            filters: NotificationFilters::default(),
            with_muted: false,
            page,
        }
    }

    /// The default page from `query`, as the endpoint parses it.
    fn page(query: &str) -> PageParams {
        use crate::pagination::MAX_LIMIT;
        PageParams::from_query(Some(query), MAX_LIMIT).expect("fixture cursors parse")
    }

    /// [`page_for_user`]'s ids, newest first.
    async fn flat_ids(pool: &PgPool, params: &FlatPageParams<'_>) -> Vec<Uuid> {
        page_for_user(pool, params)
            .await
            .expect("the page reads")
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[tokio::test]
    async fn the_flat_page_orders_rows_and_walks_the_cursors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let older = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let middle = note(pool, recipient, actor, "mention", Some(status_id)).await;
        let newer = note(pool, recipient, actor, "follow", None).await;
        let all = vec![newer.id, middle.id, older.id];

        let open = flat_ids(pool, &flat(recipient, page(""))).await;
        assert_eq!(open, all, "newest row first");
        let capped = flat_ids(pool, &flat(recipient, page("limit=2"))).await;
        assert_eq!(capped, vec![newer.id, middle.id], "limit cuts the page");
        let back = flat_ids(
            pool,
            &flat(recipient, page(&format!("max_id={}", middle.id))),
        )
        .await;
        assert_eq!(back, vec![older.id], "max_id keeps strictly older rows");
        let up = flat_ids(
            pool,
            &flat(recipient, page(&format!("since_id={}", middle.id))),
        )
        .await;
        assert_eq!(up, vec![newer.id], "since_id keeps strictly newer rows");
        let forward = flat_ids(
            pool,
            &flat(recipient, page(&format!("min_id={}", older.id))),
        )
        .await;
        assert_eq!(
            forward,
            vec![newer.id, middle.id],
            "a min_id page still answers newest-first"
        );
    }

    #[tokio::test]
    async fn the_flat_filters_restrict_remove_and_keep_one_actor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let carol = solo(pool, "flat-carol").await;
        let outsider = solo(pool, "flat-outsider").await;
        let favourite = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let mention = note(pool, recipient, actor, "mention", Some(status_id)).await;
        let follow = note(pool, recipient, carol.id, "follow", None).await;
        let _foreign = note(pool, outsider.id, carol.id, "follow", None).await;

        let mut params = flat(recipient, page(""));
        let kept = NotificationFilters {
            types: vec!["favourite", "mention"],
            ..NotificationFilters::default()
        };
        params.filters = kept;
        assert_eq!(
            flat_ids(pool, &params).await,
            vec![mention.id, favourite.id],
            "types[] restricts to the named types, newest first"
        );
        params.filters = NotificationFilters {
            exclude_types: vec!["favourite"],
            ..NotificationFilters::default()
        };
        assert_eq!(
            flat_ids(pool, &params).await,
            vec![follow.id, mention.id],
            "exclude_types[] removes the named type"
        );
        params.filters = NotificationFilters {
            account_id: Some(carol.id),
            ..NotificationFilters::default()
        };
        assert_eq!(
            flat_ids(pool, &params).await,
            vec![follow.id],
            "account_id keeps one actor's rows"
        );
        params.filters = NotificationFilters {
            types: vec!["not-a-type"],
            ..NotificationFilters::default()
        };
        assert!(
            flat_ids(pool, &params).await.is_empty(),
            "a type string no row carries matches nothing"
        );
    }

    #[tokio::test]
    async fn the_flat_page_hides_dismissed_and_deactivated_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let ghost = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let live = note(pool, recipient, actor, "mention", Some(status_id)).await;

        let _dismissed = dismiss(pool, recipient, ghost.id)
            .await
            .expect("the dismissal runs");
        assert_eq!(
            flat_ids(pool, &flat(recipient, page(""))).await,
            vec![live.id],
            "dismissed rows never appear"
        );

        let _inactive = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(actor)
            .execute(pool)
            .await
            .expect("the deactivation lands");
        assert!(
            flat_ids(pool, &flat(recipient, page(""))).await.is_empty(),
            "a deactivated actor's notifications hide too"
        );
    }

    #[tokio::test]
    async fn with_muted_decides_the_mute_exclusions_but_never_the_blocks() {
        use crate::{
            config::TEST_BASE_URL,
            models::{
                block, conversation_mute, domain_block,
                mute::{self, MuteOptions},
            },
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = solo(pool, "flat-alice").await;
        let muted = solo(pool, "flat-muted").await;
        let blocked = solo(pool, "flat-blocked").await;
        let timeline_only = solo(pool, "flat-timeline").await;
        let mallory = remote(pool, "mallory", "evil.example").await;
        let erin = solo(pool, "flat-erin").await;
        let thread = post(pool, &erin).await;

        let from_muted = note(pool, alice.id, muted.id, "follow", None).await;
        let from_blocked = note(pool, alice.id, blocked.id, "follow", None).await;
        let from_timeline = note(pool, alice.id, timeline_only.id, "mention", None).await;
        let from_domain = note(pool, alice.id, mallory.id, "follow", None).await;
        let in_muted_thread = note(pool, alice.id, erin.id, "favourite", Some(thread)).await;
        let clean = note(pool, alice.id, erin.id, "follow", None).await;

        let _muted = mute::mute(pool, &alice, &muted, MuteOptions::default())
            .await
            .expect("the mute fixture lands");
        let _timeline = mute::mute(
            pool,
            &alice,
            &timeline_only,
            MuteOptions {
                notifications: false,
                expires_in: None,
            },
        )
        .await
        .expect("the timeline-only mute lands");
        let _blocked = block::block(pool, TEST_BASE_URL, &alice, &blocked)
            .await
            .expect("the block fixture lands");
        domain_block::block_domain(pool, alice.id, "evil.example")
            .await
            .expect("the domain block lands");
        conversation_mute::mute(pool, alice.id, thread)
            .await
            .expect("the thread mute lands");

        let hidden = vec![clean.id, from_timeline.id];
        assert_eq!(
            flat_ids(pool, &flat(alice.id, page(""))).await,
            hidden,
            "by default the muted actor and muted thread hide; a \
             timeline-only mute never does, and so do the blocked actor \
             and the blocked domain"
        );

        let mut shown = flat(alice.id, page(""));
        shown.with_muted = true;
        let kept = flat_ids(pool, &shown).await;
        assert!(kept.contains(&from_muted.id), "the muted actor reads back");
        assert!(kept.contains(&in_muted_thread.id), "and the muted thread");
        assert!(
            !kept.contains(&from_blocked.id),
            "the blocked actor stays hidden, Pleroma's unconditional \
             `exclude_blocked`: {kept:?}"
        );
        assert!(
            !kept.contains(&from_domain.id),
            "and so does the blocked domain"
        );
    }

    #[tokio::test]
    async fn get_visible_answers_only_live_owned_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let live = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let doomed = note(pool, recipient, actor, "mention", Some(status_id)).await;

        assert_eq!(
            get_visible(pool, recipient, live.id)
                .await
                .expect("the read runs")
                .map(|row| row.id),
            Some(live.id),
            "the owner's live row reads back"
        );
        assert!(
            get_visible(pool, Uuid::now_v7(), live.id)
                .await
                .expect("the read runs")
                .is_none(),
            "a stranger's read finds nothing"
        );
        assert!(
            get_visible(pool, recipient, Uuid::now_v7())
                .await
                .expect("the read runs")
                .is_none(),
            "and so does an unknown id"
        );
        let _dismissed = dismiss(pool, recipient, doomed.id)
            .await
            .expect("the dismissal runs");
        assert!(
            get_visible(pool, recipient, doomed.id)
                .await
                .expect("the read runs")
                .is_none(),
            "a dismissed row hides"
        );
    }

    #[tokio::test]
    async fn get_visible_hides_blocked_actors_but_keeps_muted_ones() {
        use crate::{
            config::TEST_BASE_URL,
            models::{
                block,
                mute::{self, MuteOptions},
            },
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = solo(pool, "visible-alice").await;
        let blocked = solo(pool, "visible-blocked").await;
        let muted = solo(pool, "visible-muted").await;
        let from_blocked = note(pool, alice.id, blocked.id, "follow", None).await;
        let from_muted = note(pool, alice.id, muted.id, "follow", None).await;
        let _edge = block::block(pool, TEST_BASE_URL, &alice, &blocked)
            .await
            .expect("the block lands");
        let _mute = mute::mute(pool, &alice, &muted, MuteOptions::default())
            .await
            .expect("the mute lands");

        assert!(
            get_visible(pool, alice.id, from_blocked.id)
                .await
                .expect("the read runs")
                .is_none(),
            "a blocked actor's row hides"
        );
        assert!(
            get_visible(pool, alice.id, from_muted.id)
                .await
                .expect("the read runs")
                .is_some(),
            "a muted actor's row still shows, like v1's show"
        );
    }
}
