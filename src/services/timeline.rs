//! The timeline query builder: the one statement behind every status
//! timeline (`docs/ARCHITECTURE.md` decision 3: a cross-table read is a
//! service, not an endpoint).
//!
//! A [`TimelineQuery`] composes four things into ONE parametrized
//! [`sqlx::QueryBuilder`] statement — user input only ever rides in as bind
//! arguments, never as text spliced into the SQL:
//!
//! - where the rows come from: [`TimelineSource`] (who authored them, or
//!   which tags they carry);
//! - what the request narrows them by: [`TimelineFilters`] (the Pleroma
//!   timeline parameters: `local_only`, `remote_only`, `only_media`,
//!   `with_muted`, `visibility` exclusions, `reply_visibility`,
//!   `exclude_replies`, `exclude_reblogs`, `instance`, `pinned_only`,
//!   `tagged`);
//! - what the viewer asked to never see again: the viewer's blocks,
//!   unexpired mutes, domain blocks and thread mutes — always applied unless
//!   `with_muted` is set;
//! - what the viewer may see at all: [`push_visibility_clause`], always
//!   applied, on top of the unconditional `deleted_at IS NULL` (a scheduled
//!   `expires_at` is not a filter: an expired row stays readable until the
//!   sweep takes it, as everywhere else in this schema).
//!
//! Finally [`PageParams::apply_keyset`] runs on `statuses.id` (decision 4:
//! `UUIDv7` text sorts by creation, so the cursors are plain id
//! comparisons); endpoints extract the page with the default
//! [`MAX_LIMIT`](crate::pagination::MAX_LIMIT) cap of 40.
//!
//! Reference: Pleroma assembles the same layers in
//! `Pleroma.Web.ActivityPub.ActivityPub.fetch_activities/3` and
//! `fetch_public_activities/2`
//! (`lib/pleroma/web/activity_pub/activity_pub.ex`) behind the actions of
//! `lib/pleroma/web/mastodon_api/controllers/timeline_controller.ex`; the
//! filter names and defaults are Pleroma's, the ticket body is
//! authoritative.
//!
//! One deliberate absence:
//!
//! - This module does not render. [`fetch`](TimelineQuery::fetch) answers
//!   [`Status`] model rows;
//!   the timelines' endpoint helper turns them into entities with
//!   [`render_statuses`] and builds the `Link` header with
//!   [`crate::pagination::link_header`].
//!
//! [`Status`]: crate::models::status::Status
//! [`render_statuses`]: crate::services::render_status::render_statuses

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        status::{Status, Visibility},
        status_visibility::push_visibility_clause,
        user::{Flag, User},
    },
    pagination::PageParams,
    services::filter::FilterContext,
};

/// The head of every [`TimelineQuery::fetch`] statement: the live rows
/// (`deleted_at IS NULL`, always applied) of `statuses`, aliased `s` for the
/// fragments [`push_visibility_clause`] and the keyset paging take. Every
/// clause below appends its own ` AND ` to this.
const SELECT_SQL: &str = "SELECT s.* FROM statuses s WHERE s.deleted_at IS NULL";

/// The author of the original a reblog row boosts, as a scalar subquery over
/// `s`: `NULL` when `s` is not a reblog, which no `= ` comparison ever
/// matches.
const REBLOG_AUTHOR_SQL: &str = "(SELECT su.user_id FROM statuses su WHERE su.id = s.reblog_of_id)";

/// The existence check for one tag on `s`: the `status_hashtags` link joined
/// to the tag's row, ready for an `h.name` comparison.
const TAG_JOIN_SQL: &str = "SELECT 1 FROM status_hashtags sh \
     JOIN hashtags h ON h.id = sh.hashtag_id WHERE sh.status_id = s.id";

/// The live-mute arms, the same test every mute read in this repository
/// applies: an indefinite mute counts, a mute at or past its expiry does
/// not (see [`crate::models::mute`]).
const MUTE_LIVE_ARMS: &str = " AND (m.expires_at IS NULL OR m.expires_at > now())";

/// The accepted-follow arms shared by the [`TimelineSource::Home`] predicate
/// and the [`ReplyVisibility::Following`] filter.
const ACCEPTED_FOLLOW_STATE: &str = " AND f.state = 'accepted'";

/// Which set of statuses a timeline answers from — the `from` half of Pleroma's
/// `fetch_activities` recipients / `fetch_public_activities` split.
///
/// The visibility an answered row must carry per source: [`Self::Home`],
/// [`Self::Account`] and [`Self::List`] defer entirely to
/// [`push_visibility_clause`]; `Public` is
/// `public` (plus `local` for an authenticated local viewer — `unlisted`
/// posts stay out of the public timeline, as in Mastodon/Pleroma) and
/// `Hashtag` is `public` only; `Direct` is `direct` rows the viewer is a
/// party to, the audience [`push_visibility_clause`] already restricts to
/// the author and the mentioned accounts.
///
/// [`filter_context`] is the same source read as a filter listing: where
/// the page's rendered statuses live for the viewer's phrase filters.
///
/// [`filter_context`]: Self::filter_context
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineSource {
    /// The viewer's following timeline: statuses the viewer authored, plus
    /// statuses by accounts the viewer has an **accepted** follow of —
    /// a reblog row among them only while that follow still has
    /// `show_reblogs` (Mastodon's `reblogs` toggle, see
    /// [`crate::models::follow`]). Members of the viewer's `exclusive`
    /// lists are pulled back out of it: no home row comes from an account
    /// the viewer keeps on an exclusive list, unless the row's author is
    /// the viewer. An anonymous request has nobody to follow, and
    /// [`fetch`](TimelineQuery::fetch) answers it the empty
    /// page without touching the database.
    Home,
    /// The instance-wide public timeline: `public` statuses, and `local`
    /// ones too for an authenticated viewer this instance owns.
    Public,
    /// A tag timeline over the `public` statuses carrying tags by their
    /// stored name: lowercase and without the leading `#` (see
    /// [`crate::models::hashtag`]); the endpoint normalizes the client's
    /// input before building the source.
    Hashtag {
        /// Keep rows carrying at least one of these tags; empty adds no
        /// condition.
        any: Vec<String>,
        /// Keep only rows carrying every one of these tags; empty adds no
        /// condition.
        all: Vec<String>,
        /// Drop rows carrying any of these tags; empty drops nothing.
        none: Vec<String>,
    },
    /// The viewer's direct conversations: `direct` statuses authored by the
    /// viewer or @-mentioning them. Like [`Self::Home`], an anonymous
    /// request answers the empty page.
    Direct,
    /// One account's posting history: every row authored by `user_id`,
    /// reblogs included (the endpoint passes
    /// [`exclude_reblogs`](TimelineFilters::exclude_reblogs) for Mastodon's
    /// default listing). Each row is still judged by
    /// [`push_visibility_clause`], so followers see `private` and the
    /// mentioned parties see `direct` where a stranger sees only `public`
    /// and `unlisted`.
    Account {
        /// The author every row of this timeline is written by.
        user_id: Uuid,
    },
    /// One list's timeline: every row authored by an account of the list's
    /// `list_members` — membership alone is enough, a follow of the member
    /// is not required — reblog rows by members included, each row still
    /// judged by [`push_visibility_clause`] (a member's `private` post
    /// reaches a viewer who follows them, and only such a viewer).
    ///
    /// Replies are then narrowed by the list row's stored
    /// [`replies_policy`](crate::models::list::List::replies_policy),
    /// judged — as [`ReplyVisibility`] judges — by the reply's stored
    /// target and its @-mentions:
    ///
    /// - `list`: a reply survives only when it answers or @-mentions a
    ///   member, or the viewer;
    /// - `followed`: also when it answers or @-mentions an account the
    ///   viewer has an **accepted** follow of;
    /// - `none`: no reply survives at all; only non-replies remain.
    ///
    /// Like [`Self::Home`] and [`Self::Direct`], an anonymous request has
    /// no viewer whose list, blocks or policy could judge the page, and
    /// [`fetch`](TimelineQuery::fetch) answers it the empty page without
    /// touching the database; the route that fills this source is never
    /// anonymous either.
    List {
        /// The `lists` row whose members fill this timeline and whose
        /// `replies_policy` narrows its replies.
        list_id: Uuid,
    },
}

impl TimelineSource {
    /// The filter context this timeline's page is rendered in, or `None`
    /// where the v1 filter contexts do not reach: [`Self::Home`] is the
    /// `home` context, [`Self::Public`] and [`Self::Hashtag`] the `public`
    /// one and [`Self::Account`] the `account` one — the mapping the
    /// filter ticket gives its endpoints. [`Self::Direct`] and
    /// [`Self::List`] name no v1 context, and the two Pleroma extensions
    /// built on them are unfiltered pages.
    #[must_use]
    pub const fn filter_context(&self) -> Option<FilterContext> {
        match self {
            Self::Home => Some(FilterContext::Home),
            Self::Public | Self::Hashtag { .. } => Some(FilterContext::Public),
            Self::Account { .. } => Some(FilterContext::Account),
            Self::Direct | Self::List { .. } => None,
        }
    }
}

/// Pleroma's `reply_visibility` timeline parameter
/// (`lib/pleroma/web/mastodon_api/controllers/timeline_controller.ex`), the
/// [`TimelineFilters::reply_visibility`] values.
///
/// Only a reply's stored target decides: its [`in_reply_to_account_id`](Status::in_reply_to_account_id)
/// and its @-mentions (the [`status_mentions`] rows), which are this schema's
/// relational reading of Pleroma's `recipients` list; a status that replies
/// to nothing is never dropped. The filter applies only when there is a
/// viewer to reply *to* — an anonymous request ignores it, as Pleroma's
/// `restrict_replies` clause that never matches does.
///
/// [`Status::in_reply_to_account_id`]: crate::models::status::Status::in_reply_to_account_id
/// [`status_mentions`]: crate::models::mention
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyVisibility {
    /// Keep a reply only when it answers the viewer or an account the viewer
    /// follows with an accepted follow, or @-mentions either (Pleroma's
    /// `following`).
    Following,
    /// Keep a reply only when it is addressed to the viewer: answering them,
    /// or @-mentioning them (Pleroma's `self`).
    SelfOnly,
}

/// The optional narrowings a timeline request carries, every one of them a
/// no-op in its [`Default`] spelling (all-off / all-empty / `None`).
///
/// The booleans are [`Flag`]s because that is what this repository models
/// every boolean with (see [`crate::models::user`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimelineFilters {
    /// Keep only statuses this instance authored (`s.local`).
    pub local_only: Flag,
    /// Keep only statuses mirrored from other instances (`NOT s.local`).
    pub remote_only: Flag,
    /// Keep only statuses carrying at least one row of `media_attachments`.
    pub only_media: Flag,
    /// Show what the viewer's own blocks, mutes, domain blocks and thread
    /// mutes would otherwise drop — Pleroma's `with_muted` parameter, whose
    /// default is `false`. [`push_visibility_clause`] is applied either
    /// way: the toggle brings back what the viewer put away, never the rows
    /// of authors who blocked the viewer.
    pub with_muted: Flag,
    /// Drop rows whose visibility is one of these, whatever the source
    /// (Pleroma's repeated `visibility[]` filter parameter, read as the
    /// exclusions the timeline endpoints pass). An empty list drops
    /// nothing.
    pub exclude_visibilities: Vec<Visibility>,
    /// Which replies survive; `None` keeps them all.
    pub reply_visibility: Option<ReplyVisibility>,
    /// Drop replies (rows with an `in_reply_to_id`).
    pub exclude_replies: Flag,
    /// Drop reblog rows (rows with a `reblog_of_id`).
    pub exclude_reblogs: Flag,
    /// Keep only statuses whose author is a remote account of this domain —
    /// Pleroma's `instance` parameter, matched against the canonical
    /// `users.domain` spelling (a `local` author has no domain and never
    /// matches). `None` filters nothing.
    pub instance: Option<String>,
    /// Keep only statuses their own author pinned (see
    /// [`crate::models::pinned_status`]).
    pub pinned_only: Flag,
    /// Keep only statuses carrying this tag, by stored name (lowercase and
    /// without the leading `#`, see [`crate::models::hashtag`]) — the
    /// account timeline's `tagged` parameter, Pleroma's `statuses` action
    /// reading it as `tag`. `None` filters nothing; a name no row of
    /// `hashtags` carries answers the empty page. Unlike
    /// [`TimelineSource::Hashtag`] this narrows whatever the source already
    /// selected and never adds a visibility of its own.
    pub tagged: Option<String>,
}

impl TimelineFilters {
    /// Appends the optional filters' conditions to `builder`, each in its
    /// own ` AND ` group; everything off / empty / `None` appends nothing.
    fn push_clauses(&self, builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
        if self.local_only.get() {
            let _local: &mut _ = builder.push(" AND s.local");
        }
        if self.remote_only.get() {
            let _remote: &mut _ = builder.push(" AND NOT s.local");
        }
        if self.only_media.get() {
            let _media: &mut _ = builder
                .push(" AND EXISTS (SELECT 1 FROM media_attachments ma WHERE ma.status_id = s.id)");
        }
        if !self.exclude_visibilities.is_empty() {
            let names: Vec<String> = self
                .exclude_visibilities
                .iter()
                .map(|visibility| visibility.as_str().to_owned())
                .collect();
            let _excluded: &mut _ = builder
                .push(" AND s.visibility <> ALL(")
                .push_bind(names)
                .push(")");
        }
        if self.exclude_replies.get() {
            let _rooted: &mut _ = builder.push(" AND s.in_reply_to_id IS NULL");
        }
        if self.exclude_reblogs.get() {
            let _boosted: &mut _ = builder.push(" AND s.reblog_of_id IS NULL");
        }
        if let Some(domain) = &self.instance {
            let _instance: &mut _ = builder
                .push(
                    " AND EXISTS (SELECT 1 FROM users au WHERE au.id = s.user_id AND au.domain = ",
                )
                .push_bind(domain.clone())
                .push(")");
        }
        if self.pinned_only.get() {
            let _pinned: &mut _ = builder.push(
                " AND EXISTS (SELECT 1 FROM pinned_statuses ps \
                   WHERE ps.status_id = s.id AND ps.user_id = s.user_id)",
            );
        }
        if let Some(tag) = &self.tagged {
            let _tagged: &mut _ = builder
                .push(" AND EXISTS (")
                .push(TAG_JOIN_SQL)
                .push(" AND h.name = ")
                .push_bind(tag.clone())
                .push(")");
        }
        if let Some(reply_visibility) = self.reply_visibility {
            push_reply_visibility(builder, reply_visibility, viewer);
        }
    }
}

/// The single query builder behind every status timeline, composed into
/// one statement and run by [`fetch`](Self::fetch).
///
/// It carries a source of rows, the request's filters, the viewer's blocks
/// and mutes, the visibility clause and a keyset page.
#[derive(Debug)]
pub struct TimelineQuery {
    /// The account asking, or `None` for an anonymous request: it decides
    /// whose follows fill [`TimelineSource::Home`], whose replies survive
    /// [`TimelineSource::List`]'s `replies_policy`, whose visibility
    /// [`push_visibility_clause`] answers for, whose exclusions apply, and
    /// which `local` rows appear on [`TimelineSource::Public`].
    pub viewer: Option<User>,
    /// Which set of statuses the timeline answers from.
    pub source: TimelineSource,
    /// The optional narrowings of the request.
    pub filters: TimelineFilters,
    /// The keyset page, run on `statuses.id` by
    /// [`PageParams::apply_keyset`] and flipped back to newest-first by
    /// [`PageParams::finish`]; the endpoints extract it with the default
    /// 40-row cap ([`MAX_LIMIT`](crate::pagination::MAX_LIMIT)).
    pub page: PageParams,
}

impl TimelineQuery {
    /// Builds the one statement the timeline is and runs it: `SELECT s.*
    /// FROM statuses s WHERE s.deleted_at IS NULL`, then the source
    /// predicate, the filters, the moderation exclusions (unless
    /// `with_muted`), [`push_visibility_clause`], and the keyset page.
    /// Answers the rows newest-first (a `min_id` page read ascending comes
    /// back flipped).
    ///
    /// The only page that never reaches the database is the impossible one:
    /// [`TimelineSource::Home`], [`TimelineSource::Direct`] or
    /// [`TimelineSource::List`] for an anonymous request — there is nobody
    /// to follow, no conversation to be a party of, and no viewer whose
    /// list could answer.
    ///
    /// # Errors
    ///
    /// Any database failure (connection, encoding) arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn fetch(&self, pool: &PgPool) -> AppResult<Vec<Status>> {
        if self.anonymous_impossible_page() {
            return Ok(Vec::new());
        }
        let viewer = self.viewer.as_ref();
        let mut builder = QueryBuilder::<Postgres>::new(SELECT_SQL);
        push_source(&mut builder, &self.source, viewer);
        self.filters.push_clauses(&mut builder, viewer);
        if !self.filters.with_muted.get() {
            push_moderation_exclusions(&mut builder, viewer);
        }
        push_visibility_clause(&mut builder, viewer, "s");
        self.page.apply_keyset(&mut builder, "s.id");
        let rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
        Ok(self.page.finish(rows))
    }

    /// Whether this is the impossible page [`fetch`](Self::fetch) answers
    /// without asking the database: a viewer-relative source behind an
    /// anonymous request.
    const fn anonymous_impossible_page(&self) -> bool {
        self.viewer.is_none()
            && matches!(
                self.source,
                TimelineSource::Home | TimelineSource::Direct | TimelineSource::List { .. }
            )
    }
}

/// Appends the [`TimelineSource`] predicate — the ` AND ` group that decides
/// which rows the timeline is made of (the visibility it must carry lives
/// here too, where the source narrows it; see [`TimelineSource`]'s docs).
fn push_source(
    builder: &mut QueryBuilder<Postgres>,
    source: &TimelineSource,
    viewer: Option<&User>,
) {
    match source {
        TimelineSource::Home => push_home(builder, viewer),
        TimelineSource::Public => push_public(builder, viewer),
        TimelineSource::Hashtag { any, all, none } => push_hashtag(builder, any, all, none),
        TimelineSource::Direct => {
            let _dm: &mut _ = builder.push(" AND s.visibility = 'direct'");
        }
        TimelineSource::Account { user_id } => {
            let _authored: &mut _ = builder.push(" AND s.user_id = ").push_bind(*user_id);
        }
        TimelineSource::List { list_id } => push_list(builder, *list_id, viewer),
    }
}

/// Appends the `Home` predicate: the viewer's own rows (their reblogs
/// included), plus rows by accounts the viewer has an **accepted** follow
/// of — and a reblog row only while that follow shows reblogs — and then
/// [`push_exclusive_list_exclusion`] takes the members of the viewer's
/// `exclusive` lists back out. Without a viewer the source is already
/// answered empty by [`TimelineQuery::fetch`]; the `AND false` arm is the
/// belt to that brace, so the predicate can never widen to "everything"
/// when the caller forgot it.
fn push_home(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        let _nothing: &mut _ = builder.push(" AND false");
        return;
    };
    let _followed: &mut _ = builder
        .push(" AND (s.user_id = ")
        .push_bind(viewer_id)
        .push(" OR EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND f.followee_id = s.user_id")
        .push(ACCEPTED_FOLLOW_STATE)
        .push(" AND (s.reblog_of_id IS NULL OR f.show_reblogs)))");
    push_exclusive_list_exclusion(builder, viewer_id);
}

/// Appends the `exclusive` list exclusion (Mastodon's `FeedManager` rule,
/// which Pleroma reaches by addressing an exclusive list's posts away from
/// the home timeline): neither a row of a member of one of the viewer's
/// `exclusive` lists nor a reblog **of** a member's row may ride the home
/// timeline — the member's own boosts of other accounts go with them, the
/// way [`push_author_edge_exclusion`] sweeps blocked authors' whole
/// presence — while the viewer's own rows were never in question: an
/// account that lists itself keeps its place in its own home.
fn push_exclusive_list_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _exclusive: &mut _ = builder
        .push(" AND (s.user_id = ")
        .push_bind(viewer_id)
        .push(
            " OR NOT EXISTS (SELECT 1 FROM list_members lm \
               JOIN lists l ON l.id = lm.list_id \
               WHERE l.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" AND l.exclusive AND (lm.user_id = s.user_id OR lm.user_id = ")
        .push(REBLOG_AUTHOR_SQL)
        .push(")))");
}

/// Appends the `Public` predicate: `public` rows, and `local` rows as well
/// for an authenticated viewer this instance owns (`local` never federates,
/// so a remote mirror or an anonymous request never sees one; and
/// `unlisted`, though the visibility clause admits it to everyone, is not
/// part of *the public timeline*).
fn push_public(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
    let _public: &mut _ = builder.push(" AND (s.visibility = 'public'");
    if viewer.is_some_and(|user| user.local.get()) {
        let _local: &mut _ = builder.push(" OR s.visibility = 'local'");
    }
    let _close: &mut _ = builder.push(")");
}

/// Appends the `Hashtag` predicate: `public` rows only, joined through the
/// tag conditions — `any` as one array bind, `all` and `none` one bound
/// name per EXISTS arm (an empty list adds nothing).
fn push_hashtag(
    builder: &mut QueryBuilder<Postgres>,
    any: &[String],
    all: &[String],
    none: &[String],
) {
    let _public: &mut _ = builder.push(" AND s.visibility = 'public'");
    if !any.is_empty() {
        let _any: &mut _ = builder
            .push(" AND EXISTS (")
            .push(TAG_JOIN_SQL)
            .push(" AND h.name = ANY(")
            .push_bind(any.to_vec())
            .push("))");
    }
    for name in all {
        let _all: &mut _ = builder
            .push(" AND EXISTS (")
            .push(TAG_JOIN_SQL)
            .push(" AND h.name = ")
            .push_bind(name.clone())
            .push(")");
    }
    for name in none {
        let _none: &mut _ = builder
            .push(" AND NOT EXISTS (")
            .push(TAG_JOIN_SQL)
            .push(" AND h.name = ")
            .push_bind(name.clone())
            .push(")");
    }
}

/// Appends the `List` predicate: rows whose own author is a member of
/// `list_id`'s `list_members` — membership alone, no follow read — reblog
/// rows by members included (their author is the member who boosted; the
/// original's author needs no membership to ride a member's boost).
/// [`push_visibility_clause`] and [`crate::models::status`] then judge
/// every member row like any other read's, and [`TimelineQuery`]'s
/// moderation arms hide members the viewer put away.
///
/// Replies are left to [`push_list_replies_policy`], which reads the same
/// list's stored `replies_policy` in the statement — the one source for
/// every list, so no second round trip decides the page.
///
/// Reference: Pleroma's `Pleroma.Web.TimelineFilter.ListTimeline` behind
/// the list action of `timeline_controller.ex` (the ticket body's
/// `list`/`followed`/`none` spellings are authoritative).
fn push_list(builder: &mut QueryBuilder<Postgres>, list_id: Uuid, viewer: Option<&User>) {
    let _member: &mut _ = builder
        .push(" AND EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ")
        .push_bind(list_id)
        .push(" AND lm.user_id = s.user_id)");
    push_list_replies_policy(builder, list_id, viewer);
}

/// Opens the `replies_policy` group and runs its arms: a non-reply always
/// survives; under `none` no reply does; otherwise a reply survives when
/// [`push_list_member_replies`] answers it to a member, or — with a viewer
/// — [`push_list_self_arms`] answers it to the viewer and, under
/// `followed`, [`push_list_followed_replies`] answers it to a viewer's
/// accepted follow. Without a viewer (a page [`fetch`](TimelineQuery::fetch)
/// already answers empty) only the member arm reads.
fn push_list_replies_policy(
    builder: &mut QueryBuilder<Postgres>,
    list_id: Uuid,
    viewer: Option<&User>,
) {
    let _open: &mut _ = builder
        .push(" AND (s.in_reply_to_id IS NULL OR ((SELECT lp.replies_policy FROM lists lp WHERE lp.id = ")
        .push_bind(list_id)
        .push(") <> 'none' AND (");
    push_list_member_replies(builder, list_id);
    if let Some(viewer_id) = viewer.map(|user| user.id) {
        push_list_self_arms(builder, viewer_id);
        push_list_followed_replies(builder, list_id, viewer_id);
    }
    let _close: &mut _ = builder.push(")))");
}

/// The `list` arm — shared by every live policy: a reply addressed to a
/// member survives, whether it answers them (`in_reply_to_account_id`) or
/// @-mentions them (the same reading of "addressed to" as
/// [`push_reply_visibility`] gives).
fn push_list_member_replies(builder: &mut QueryBuilder<Postgres>, list_id: Uuid) {
    let _member: &mut _ = builder
        .push(" EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ")
        .push_bind(list_id)
        .push(" AND (lm.user_id = s.in_reply_to_account_id OR EXISTS (SELECT 1 FROM status_mentions sm WHERE sm.status_id = s.id AND sm.user_id = lm.user_id)))");
}

/// The viewer's own arm, live under both `list` and `followed`: a reply
/// answering or @-mentioning the viewer survives on the strength of being
/// addressed to the reader, whoever else the policy counts (Mastodon reads
/// the current account next to the members in `list` and next to the
/// followed accounts in `followed`).
fn push_list_self_arms(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _self_arm: &mut _ = builder
        .push(" OR (s.in_reply_to_account_id = ")
        .push_bind(viewer_id)
        .push(" OR EXISTS (SELECT 1 FROM status_mentions sv WHERE sv.status_id = s.id AND sv.user_id = ")
        .push_bind(viewer_id)
        .push("))");
}

/// The `followed` policy's extra arm: a reply answering or @-mentioning an
/// account the viewer has an **accepted** follow of survives too — the
/// same arms [`push_following_arms`] builds for [`ReplyVisibility::Following`],
/// hung behind a read of this list's own policy so the `list` policy never
/// sees them.
fn push_list_followed_replies(
    builder: &mut QueryBuilder<Postgres>,
    list_id: Uuid,
    viewer_id: Uuid,
) {
    let _followed: &mut _ = builder
        .push(" OR ((SELECT fp.replies_policy FROM lists fp WHERE fp.id = ")
        .push_bind(list_id)
        .push(") = 'followed' AND (EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND f.followee_id = s.in_reply_to_account_id")
        .push(ACCEPTED_FOLLOW_STATE)
        .push(") OR EXISTS (SELECT 1 FROM status_mentions sf JOIN follows mf ON mf.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND mf.followee_id = sf.user_id AND mf.state = 'accepted' WHERE sf.status_id = s.id)))");
}

/// Appends the `reply_visibility` group for `visibility`, judged by the
/// reply's stored target (`in_reply_to_account_id`) and @-mentions;
/// non-replies and the viewer's own replies always survive. With no viewer
/// there is nothing to reply *to* or *for*, so — as Pleroma's clause that
/// requires a `%User{}` — the filter appends nothing.
fn push_reply_visibility(
    builder: &mut QueryBuilder<Postgres>,
    visibility: ReplyVisibility,
    viewer: Option<&User>,
) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        return;
    };
    let _open: &mut _ = builder
        .push(" AND (s.in_reply_to_id IS NULL OR s.user_id = ")
        .push_bind(viewer_id);
    match visibility {
        ReplyVisibility::SelfOnly => push_self_arms(builder, viewer_id),
        ReplyVisibility::Following => push_following_arms(builder, viewer_id),
    }
    let _close: &mut _ = builder.push(")");
}

/// The `self` arms: the reply's author-side target is the viewer, or the
/// reply @-mentions them.
fn push_self_arms(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _target: &mut _ = builder
        .push(" OR s.in_reply_to_account_id = ")
        .push_bind(viewer_id)
        .push(
            " OR EXISTS (SELECT 1 FROM status_mentions sm \
               WHERE sm.status_id = s.id AND sm.user_id = ",
        )
        .push_bind(viewer_id)
        .push(")");
}

/// The `following` arms: the target is the viewer or one of their accepted
/// follows, and a @-mention of the viewer or of an accepted follow counts as
/// being addressed to friends (Pleroma's recipients ∩ (self ∪ friends)).
fn push_following_arms(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _target: &mut _ = builder
        .push(" OR s.in_reply_to_account_id = ")
        .push_bind(viewer_id)
        .push(" OR EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND f.followee_id = s.in_reply_to_account_id")
        .push(ACCEPTED_FOLLOW_STATE)
        .push(") OR EXISTS (SELECT 1 FROM status_mentions sm LEFT JOIN follows mf ON mf.follower_id = ")
        .push_bind(viewer_id)
        .push(
            " AND mf.followee_id = sm.user_id AND mf.state = 'accepted' \
               WHERE sm.status_id = s.id AND (sm.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" OR mf.id IS NOT NULL))");
}

/// Appends the exclusions the viewer lives by — statuses authored by
/// accounts the viewer blocked or still mutes (an unexpired mute), reblogs
/// **of** those accounts' statuses, statuses by authors on a domain-blocked
/// domain, and statuses inside a thread-muted conversation (the viewer's
/// own posts in a muted thread excepted). All four read the viewer, so an
/// anonymous request gains no condition; `with_muted` skips the whole
/// group, while [`push_visibility_clause`] — the author's side of a block —
/// keeps applying either way.
fn push_moderation_exclusions(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        return;
    };
    push_author_edge_exclusion(builder, viewer_id);
    push_muted_edge_exclusion(builder, viewer_id);
    let _domain: &mut _ = builder
        .push(
            " AND NOT EXISTS (SELECT 1 FROM user_domain_blocks udb \
               JOIN users d ON d.id = s.user_id \
               WHERE udb.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" AND udb.domain = d.domain)");
    let _thread: &mut _ = builder
        .push(
            " AND NOT EXISTS (SELECT 1 FROM conversation_mutes cm \
               WHERE cm.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" AND cm.conversation_id = s.conversation_id AND s.user_id <> ")
        .push_bind(viewer_id)
        .push(")");
}

/// The block arm: neither the row's author nor the author of the original
/// it reblogs may be an account the viewer blocks.
fn push_author_edge_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _blocked: &mut _ = builder
        .push(" AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.blocker_id = ")
        .push_bind(viewer_id)
        .push(" AND (b.blocked_id = s.user_id OR b.blocked_id = ")
        .push(REBLOG_AUTHOR_SQL)
        .push("))");
}

/// The mute arm, [`push_author_edge_exclusion`]'s shape with the
/// live-mutes test ([`MUTE_LIVE_ARMS`]) on: an expired mute hides nothing,
/// the same way it counts for nothing in every other read of
/// [`crate::models::mute`].
fn push_muted_edge_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _muted: &mut _ = builder
        .push(" AND NOT EXISTS (SELECT 1 FROM mutes m WHERE m.muter_id = ")
        .push_bind(viewer_id)
        .push(" AND (m.muted_id = s.user_id OR m.muted_id = ")
        .push(REBLOG_AUTHOR_SQL)
        .push(")")
        .push(MUTE_LIVE_ARMS)
        .push(")");
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            block, conversation_mute, domain_block, follow,
            list::{self, List, ListChanges, NewList},
            mute::{self, MuteOptions},
            pinned_status,
            status::{self, NewStatus, soft_delete},
            user::{self, NewUser, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;

    /// Inserts a local user named `nickname`.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a locked local user named `nickname`, so a follow of them
    /// stays `pending` and never joins a home timeline.
    async fn locked_local(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..NewUser::local(nickname, &local_ap_id(BASE, nickname))
            },
        )
        .await
        .expect("the locked fixture inserts")
    }

    /// Inserts a mirror of the remote actor `nickname` on `domain`.
    async fn remote(pool: &PgPool, nickname: &str, domain: &str) -> User {
        insert_user(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{domain}"),
                ap_id: format!("https://{domain}/users/{nickname}"),
                domain: Some(domain.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// Reads `viewer` back from the table: a [`TimelineQuery`] owns its
    /// viewer, so a test that asks twice hands out one row per ask.
    async fn as_viewer(pool: &PgPool, viewer: &User) -> User {
        user::get_by_id(pool, viewer.id)
            .await
            .expect("the viewer reads")
            .expect("the fixture viewer exists")
    }

    /// Inserts `text` as `author`'s live local status of `visibility`.
    async fn post(pool: &PgPool, author: &User, visibility: Visibility, text: &str) -> Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a fixture post inserts")
    }

    /// Inserts `author`'s `direct` status mentioning everyone in `mentioned`.
    async fn direct(pool: &PgPool, author: &User, mentioned: &[&User], text: &str) -> Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: mentioned.iter().map(|member| member.id).collect(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a fixture direct message inserts")
    }

    /// Inserts `author`'s public reply to `parent`, @-mentioning everyone in
    /// `mentioned` (empty for none).
    async fn reply(
        pool: &PgPool,
        author: &User,
        parent: &Status,
        mentioned: &[&User],
        text: &str,
    ) -> Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(parent.user_id),
                mentioned_user_ids: mentioned.iter().map(|member| member.id).collect(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a fixture reply inserts")
    }

    /// `reblogger`'s public reblog of `original`.
    async fn reblog(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
        status::create_reblog(pool, BASE, reblogger, original, Visibility::Public)
            .await
            .expect("a fixture reblog writes")
            .0
    }

    /// Inserts a mirrored (`local = false`) public status whose object uri
    /// and text are keyed by `uri_key`, carrying `hashtags` by stored name.
    async fn remote_post(pool: &PgPool, author: &User, uri_key: &str, hashtags: &[&str]) -> Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                uri: Some(format!("https://origin.test.example/objects/{uri_key}")),
                user_id: author.id,
                raw_text: uri_key.to_owned(),
                hashtags: hashtags.iter().map(|name| (*name).to_owned()).collect(),
                ..NewStatus::default()
            },
        )
        .await
        .expect("a fixture remote status inserts")
    }

    /// Attaches one media row to `status_id`.
    async fn attach_media(pool: &PgPool, status_id: Uuid) {
        let written = sqlx::query(
            "INSERT INTO media_attachments (id, status_id, url, content_type)
             VALUES ($1, $2, $3, 'image/png')",
        )
        .bind(Uuid::now_v7())
        .bind(status_id)
        .bind(format!("{BASE}/media/fixture.png"))
        .execute(pool)
        .await
        .expect("the media fixture inserts");
        assert_eq!(written.rows_affected(), 1, "one attachment lands");
    }

    /// Lapses `muter`'s mute of `muted` an hour into the past, the state
    /// every read in this repository already treats as no mute at all.
    async fn lapse_mute(pool: &PgPool, muter: &User, target: &User) {
        let lapsed = sqlx::query(
            "UPDATE mutes SET expires_at = now() - interval '1 hour'
              WHERE muter_id = $1 AND muted_id = $2",
        )
        .bind(muter.id)
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the mute lapses");
        assert_eq!(lapsed.rows_affected(), 1, "the fixture mute was there");
    }

    /// Creates `owner`'s list titled `circle` with `policy` as its replies
    /// policy and `exclusive` as its exclusiveness.
    async fn owned_list(pool: &PgPool, owner: &User, policy: &str, exclusive: bool) -> List {
        list::create(
            pool,
            owner.id,
            &NewList {
                title: "circle".to_owned(),
                replies_policy: policy.to_owned(),
                exclusive,
            },
        )
        .await
        .expect("the fixture list creates")
    }

    /// Adds `member` to `list` through the model's own write (the API's
    /// follow rule is [`crate::api::lists`]'s business, not membership's).
    async fn add_member(pool: &PgPool, list: &List, member: &User) {
        let added = list::add_members(pool, list.id, &[member.id])
            .await
            .expect("the fixture membership adds");
        assert_eq!(added, 1, "exactly one membership went in");
    }

    /// Rewrites the list's stored replies policy, the same write the API's
    /// `PUT` performs.
    async fn set_policy(pool: &PgPool, owner: &User, list: &List, policy: &str) {
        let rewritten = list::update(
            pool,
            owner.id,
            list.id,
            &ListChanges {
                title: None,
                replies_policy: Some(policy.to_owned()),
                exclusive: None,
            },
        )
        .await
        .expect("the policy update succeeds")
        .expect("the fixture list exists");
        assert_eq!(
            rewritten.replies_policy, policy,
            "the stored policy now reads `{policy}`"
        );
    }

    /// The whole-table page: no cursors and a limit above every fixture
    /// this module seeds (the cap of 40 is the endpoint extractor's, not
    /// [`TimelineQuery::fetch`]'s).
    const fn full_page() -> PageParams {
        PageParams {
            max_id: None,
            min_id: None,
            since_id: None,
            limit: 40,
            offset: None,
        }
    }

    /// A timeline `source` as `viewer`, unfiltered, one full page.
    fn timeline(viewer: Option<User>, source: TimelineSource) -> TimelineQuery {
        TimelineQuery {
            viewer,
            source,
            filters: TimelineFilters::default(),
            page: full_page(),
        }
    }

    /// A timeline `source` as `viewer` with `filters`, one full page.
    fn filtered(
        viewer: Option<User>,
        source: TimelineSource,
        filters: TimelineFilters,
    ) -> TimelineQuery {
        TimelineQuery {
            viewer,
            source,
            filters,
            page: full_page(),
        }
    }

    /// Runs `query` and answers its page as ids, in the order it came back.
    async fn page_ids(pool: &PgPool, query: &TimelineQuery) -> Vec<Uuid> {
        let statuses = query.fetch(pool).await.expect("the timeline query runs");
        statuses.iter().map(|status| status.id).collect()
    }

    /// A timeline `source` as `viewer`, unfiltered, on a custom `page`.
    fn paged(viewer: Option<User>, source: TimelineSource, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer,
            source,
            filters: TimelineFilters::default(),
            page,
        }
    }

    /// The anonymous `Account` page of `user_id`, on `page` — the read the
    /// keyset test walks.
    async fn all_pages(pool: &PgPool, user_id: Uuid, page: PageParams) -> Vec<Uuid> {
        page_ids(
            pool,
            &paged(None, TimelineSource::Account { user_id }, page),
        )
        .await
    }

    /// The ids of `statuses`, id descending — the order
    /// [`fetch`](TimelineQuery::fetch) answers in, since `UUIDv7` text sorts
    /// by creation (decision 4).
    fn newest_first(statuses: &[&Status]) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = statuses.iter().map(|status| status.id).collect();
        ids.sort_by(|a, b| b.cmp(a));
        ids
    }

    /// The ids of `statuses`, newest first, minus the ids in `without`:
    /// the expected page written as a difference from the seeded set.
    fn minus(statuses: &[&Status], without: &[&Status]) -> Vec<Uuid> {
        let skipped: Vec<Uuid> = without.iter().map(|status| status.id).collect();
        let mut ids: Vec<Uuid> = statuses
            .iter()
            .map(|status| status.id)
            .filter(|id| !skipped.contains(id))
            .collect();
        ids.sort_by(|a, b| b.cmp(a));
        ids
    }

    /// The `Public` timeline as `viewer` with only a `reply_visibility`
    /// filter, one full page.
    fn reply_page(viewer: Option<User>, reply_visibility: ReplyVisibility) -> TimelineQuery {
        filtered(
            viewer,
            TimelineSource::Public,
            TimelineFilters {
                reply_visibility: Some(reply_visibility),
                ..TimelineFilters::default()
            },
        )
    }

    /// A two-row page, the shape every cursor case of
    /// [`keyset_pages_walk_ids_and_min_id_flips`] asks for.
    const fn two_rows(cursor: PageCursor) -> PageParams {
        let (max_id, min_id, since_id) = match cursor {
            PageCursor::None => (None, None, None),
            PageCursor::Max(id) => (Some(id), None, None),
            PageCursor::Min(id) => (None, Some(id), None),
            PageCursor::Since(id) => (None, None, Some(id)),
        };
        PageParams {
            max_id,
            min_id,
            since_id,
            limit: 2,
            offset: None,
        }
    }

    /// Which cursor a [`two_rows`] page carries, beside no cursor at all.
    #[derive(Clone, Copy, Debug)]
    enum PageCursor {
        /// No cursor: the first page.
        None,
        /// `max_id`, reading strictly older rows.
        Max(Uuid),
        /// `min_id`, reading strictly newer rows, flipped.
        Min(Uuid),
        /// `since_id`, reading strictly newer rows, newest-first.
        Since(Uuid),
    }

    /// `Home` is the viewer's own rows and those of their **accepted**
    /// follows only — a stranger's public post and a locked account's
    /// pending-requested post both stay out — and an anonymous `Home` is the
    /// empty page without a query. A follower reaches the followed
    /// account's `private` and `unlisted` posts: that is the visibility
    /// clause deciding, not the source.
    #[tokio::test]
    async fn home_answers_own_posts_and_accepted_follows_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = locked_local(&pool, "dave").await;
        let accepted = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows open bob");
        assert_eq!(
            accepted.state,
            follow::FollowState::Accepted,
            "bob is unlocked, so the follow is granted"
        );
        let pending = follow::follow(&pool, BASE, &alice, &dave)
            .await
            .expect("alice requests locked dave");
        assert_eq!(
            pending.state,
            follow::FollowState::Pending,
            "dave is locked, so the request waits"
        );
        let own = post(&pool, &alice, Visibility::Public, "own post").await;
        let own_unlisted = post(&pool, &alice, Visibility::Unlisted, "own unlisted").await;
        let followed_public = post(&pool, &bob, Visibility::Public, "followed public").await;
        let followed_private = post(&pool, &bob, Visibility::Private, "followed private").await;
        let stranger = post(&pool, &carol, Visibility::Public, "stranger public").await;
        let pending_post = post(&pool, &dave, Visibility::Public, "pending public").await;

        let for_alice = page_ids(&pool, &timeline(Some(alice), TimelineSource::Home)).await;
        assert_eq!(
            for_alice,
            newest_first(&[&own, &own_unlisted, &followed_public, &followed_private]),
            "home is exactly the viewer's own posts and their accepted follows' visible posts"
        );
        assert!(
            !for_alice.contains(&stranger.id) && !for_alice.contains(&pending_post.id),
            "a stranger and a merely-requested locked account never join the home timeline"
        );
        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Home)).await;
        assert!(anonymous.is_empty(), "an anonymous home is the empty page");
    }

    /// A reblog row on `Home` rides its author's follow: with that follow's
    /// `show_reblogs` switched off the boost disappears, the author's plain
    /// posts stay, and the viewer's own reblogs were never governed by the
    /// toggle.
    #[tokio::test]
    async fn home_hides_reblogs_from_follows_that_switched_them_off() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let bob_follow = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows bob");
        assert!(bob_follow.show_reblogs, "a fresh follow shows reblogs");
        let carol_follow = follow::follow(&pool, BASE, &alice, &carol)
            .await
            .expect("alice follows carol");
        assert!(carol_follow.show_reblogs, "and so does this one");
        let original = post(&pool, &dave, Visibility::Public, "reblogged original").await;
        let bob_reblog = reblog(&pool, &bob, &original).await;
        let carol_reblog = reblog(&pool, &carol, &original).await;
        let carol_plain = post(&pool, &carol, Visibility::Public, "carol plain").await;
        let own_reblog = reblog(&pool, &alice, &original).await;

        let before = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Home),
        )
        .await;
        assert_eq!(
            before,
            newest_first(&[&bob_reblog, &carol_reblog, &carol_plain, &own_reblog]),
            "with the default toggle both follows' reblogs are on the home timeline"
        );

        let hidden = follow::set_toggles(
            &pool,
            alice.id,
            carol.id,
            follow::FollowToggles {
                show_reblogs: Some(false),
                notify: None,
            },
        )
        .await
        .expect("the toggle writes")
        .expect("alice's follow of carol is open");
        assert!(!hidden.show_reblogs, "the follow now hides reblogs");
        let after = page_ids(&pool, &timeline(Some(alice), TimelineSource::Home)).await;
        assert_eq!(
            after,
            newest_first(&[&bob_reblog, &carol_plain, &own_reblog]),
            "carol's boost is gone, her plain posts and the viewer's own boost stay"
        );
    }

    /// The `Public` timeline is `public` for everyone — never `unlisted`,
    /// `private` or `direct`, not even the viewer's own — while `local`
    /// posts appear exactly for an authenticated viewer this instance owns.
    #[tokio::test]
    async fn public_shows_public_to_everyone_and_local_only_to_local_viewers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mallory = remote(&pool, "mallory", "remote.example").await;
        let public = post(&pool, &alice, Visibility::Public, "public").await;
        let unlisted = post(&pool, &alice, Visibility::Unlisted, "unlisted").await;
        let instance_local = post(&pool, &alice, Visibility::Local, "local only").await;
        let private = post(&pool, &alice, Visibility::Private, "private").await;
        let dm = post(&pool, &alice, Visibility::Direct, "unaddressed dm").await;
        let remote_public = remote_post(&pool, &mallory, "remote-public", &[]).await;

        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Public)).await;
        assert_eq!(
            anonymous,
            newest_first(&[&public, &remote_public]),
            "the anonymous public timeline is public rows only"
        );
        let for_alice = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Public),
        )
        .await;
        assert_eq!(
            for_alice,
            newest_first(&[&public, &remote_public, &instance_local]),
            "a local viewer's public timeline gains the `local` row and nothing else"
        );
        assert!(
            !for_alice.contains(&unlisted.id)
                && !for_alice.contains(&private.id)
                && !for_alice.contains(&dm.id),
            "unlisted, private and direct are off the public timeline even for their author"
        );
        let for_mallory = page_ids(&pool, &timeline(Some(mallory), TimelineSource::Public)).await;
        assert_eq!(
            for_mallory,
            newest_first(&[&public, &remote_public]),
            "a remote viewer never sees a `local` row"
        );
    }

    /// The `Hashtag` source keeps `public` rows only and reads `any` as a
    /// union, `all` as an intersection and `none` as a subtraction over the
    /// stored (lowercase, `#`-less) tag names.
    #[tokio::test]
    async fn hashtag_tags_select_public_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mallory = remote(&pool, "mallory", "remote.example").await;
        let foo_one = post(&pool, &alice, Visibility::Public, "#foo first").await;
        let bar_one = post(&pool, &alice, Visibility::Public, "#bar only").await;
        let both = post(&pool, &alice, Visibility::Public, "#foo #bar both").await;
        let foo_remote = remote_post(&pool, &mallory, "tagged-remote", &["foo"]).await;
        let foo_unlisted = post(&pool, &alice, Visibility::Unlisted, "#foo quiet").await;
        let foo_local = post(&pool, &alice, Visibility::Local, "#foo instance").await;
        let tags = |any: &[&str], all: &[&str], none: &[&str]| TimelineSource::Hashtag {
            any: any.iter().map(|name| (*name).to_owned()).collect(),
            all: all.iter().map(|name| (*name).to_owned()).collect(),
            none: none.iter().map(|name| (*name).to_owned()).collect(),
        };

        let for_any_foo = page_ids(&pool, &timeline(None, tags(&["foo"], &[], &[]))).await;
        assert_eq!(
            for_any_foo,
            newest_first(&[&foo_one, &both, &foo_remote]),
            "`any: foo` is every public row tagged foo, no matter who tagged it"
        );
        assert!(
            !for_any_foo.contains(&foo_unlisted.id) && !for_any_foo.contains(&foo_local.id),
            "the tag timeline is public-only: unlisted and local rows stay out"
        );
        let for_either = page_ids(&pool, &timeline(None, tags(&["foo", "bar"], &[], &[]))).await;
        assert_eq!(
            for_either,
            newest_first(&[&foo_one, &bar_one, &both, &foo_remote]),
            "`any` unions"
        );
        let for_both_tags = page_ids(&pool, &timeline(None, tags(&[], &["foo", "bar"], &[]))).await;
        assert_eq!(for_both_tags, newest_first(&[&both]), "`all` intersects");
        let for_foo_no_bar =
            page_ids(&pool, &timeline(Some(alice), tags(&["foo"], &[], &["bar"]))).await;
        assert_eq!(
            for_foo_no_bar,
            newest_first(&[&foo_one, &foo_remote]),
            "`none` subtracts, for any viewer"
        );
        let for_nothing_foo = page_ids(&pool, &timeline(None, tags(&[], &[], &["foo"]))).await;
        assert_eq!(
            for_nothing_foo,
            newest_first(&[&bar_one]),
            "`none` alone narrows the tagged set too"
        );
    }

    /// `Direct` answers exactly the `direct` rows the viewer is a party to —
    /// sent, or received by mention — no other account's DMs and no public
    /// post, and nothing at all for an anonymous request.
    #[tokio::test]
    async fn direct_answers_the_conversations_the_viewer_is_in() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let a_to_b = direct(&pool, &alice, &[&bob], "alice to bob").await;
        let b_to_a = direct(&pool, &bob, &[&alice], "bob to alice").await;
        let a_to_c = direct(&pool, &alice, &[&carol], "alice to carol").await;
        let public = post(&pool, &alice, Visibility::Public, "not a dm").await;

        let for_alice = page_ids(&pool, &timeline(Some(alice), TimelineSource::Direct)).await;
        assert_eq!(
            for_alice,
            newest_first(&[&a_to_b, &b_to_a, &a_to_c]),
            "the sender sees everything they sent and everything sent to them"
        );
        assert!(!for_alice.contains(&public.id), "a public post is not a DM");
        let for_bob = page_ids(&pool, &timeline(Some(bob), TimelineSource::Direct)).await;
        assert_eq!(
            for_bob,
            newest_first(&[&a_to_b, &b_to_a]),
            "the recipient only sees the messages naming them"
        );
        assert!(
            !for_bob.contains(&a_to_c.id),
            "a DM between other accounts never leaks into someone else's direct timeline"
        );
        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Direct)).await;
        assert!(
            anonymous.is_empty(),
            "an anonymous direct timeline is the empty page"
        );
    }

    /// `Account` is everything the author wrote — posts, replies, their own
    /// reblogs — judged per row by the visibility clause: followers reach
    /// `private`, a mentioned party reaches `direct`, and a stranger (or the
    /// anonymous request) keeps to `public` and `unlisted`; someone else's
    /// reblog of the author's post is that reblogger's row, not the
    /// author's.
    #[tokio::test]
    async fn account_answers_the_authors_rows_with_per_row_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let alice_follow = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows bob");
        assert_eq!(
            alice_follow.state,
            follow::FollowState::Accepted,
            "bob is unlocked, so the follow is granted"
        );
        let bob_id = bob.id;
        let account = move || TimelineSource::Account { user_id: bob_id };
        let public = post(&pool, &bob, Visibility::Public, "bob public").await;
        let unlisted = post(&pool, &bob, Visibility::Unlisted, "bob unlisted").await;
        let private = post(&pool, &bob, Visibility::Private, "bob private").await;
        let dm = direct(&pool, &bob, &[&alice], "bob dm to alice").await;
        let root = post(&pool, &carol, Visibility::Public, "carol root").await;
        let bob_reply = reply(&pool, &bob, &root, &[], "bob replies to carol").await;
        let carol_reblogs = reblog(&pool, &carol, &public).await;

        let for_follower = page_ids(&pool, &timeline(Some(alice), account())).await;
        assert_eq!(
            for_follower,
            newest_first(&[&public, &unlisted, &private, &dm, &bob_reply]),
            "a follower sees the whole history"
        );
        assert!(
            !for_follower.contains(&carol_reblogs.id),
            "someone else's boost of the author is the reblogger's row"
        );
        let for_stranger = page_ids(&pool, &timeline(Some(carol), account())).await;
        assert_eq!(
            for_stranger,
            newest_first(&[&public, &unlisted, &bob_reply]),
            "a non-follower reaches public and unlisted — but neither private nor DM"
        );
        let anonymous = page_ids(&pool, &timeline(None, account())).await;
        assert_eq!(
            anonymous,
            newest_first(&[&public, &unlisted, &bob_reply]),
            "the anonymous request reads the stranger's page"
        );
        let for_author = page_ids(&pool, &timeline(Some(bob), account())).await;
        assert_eq!(
            for_author,
            newest_first(&[&public, &unlisted, &private, &dm, &bob_reply]),
            "the author sees all of their own rows"
        );
    }

    /// `List` answers rows authored by members — no follow read: a followed
    /// non-member stays out, a member's `private` post stays out only for
    /// the visibility clause's reason, a member's boost rides in (its
    /// author is the member), and the list's own owner is off the page
    /// unless they added themselves. An anonymous list fetches the empty
    /// page without a query.
    #[tokio::test]
    async fn list_answers_member_posts_without_follows_including_member_reblogs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let carol_follow = follow::follow(&pool, BASE, &alice, &carol)
            .await
            .expect("alice follows carol");
        assert_eq!(
            carol_follow.state,
            follow::FollowState::Accepted,
            "the fixture follow is granted"
        );
        let circle = owned_list(&pool, &alice, "list", false).await;
        add_member(&pool, &circle, &bob).await;
        let bob_public = post(&pool, &bob, Visibility::Public, "member public").await;
        let _bob_private = post(&pool, &bob, Visibility::Private, "member private").await;
        let carol_public = post(&pool, &carol, Visibility::Public, "followed non-member").await;
        let dave_public = post(&pool, &dave, Visibility::Public, "boosted original").await;
        let alice_own = post(&pool, &alice, Visibility::Public, "owner's own post").await;
        let bob_reblog = reblog(&pool, &bob, &dave_public).await;

        let page = page_ids(
            &pool,
            &timeline(Some(alice), TimelineSource::List { list_id: circle.id }),
        )
        .await;
        assert_eq!(
            page,
            newest_first(&[&bob_public, &bob_reblog]),
            "the member's post and the member's boost ride the list; the followed \
             non-member's post, the owner's own post and the un-followed member's private \
             row (by the visibility clause) do not"
        );
        assert!(
            !page.contains(&carol_public.id) && !page.contains(&alice_own.id),
            "membership, not follow or ownership, is what fills the list"
        );
        let anonymous = page_ids(
            &pool,
            &timeline(None, TimelineSource::List { list_id: circle.id }),
        )
        .await;
        assert!(
            anonymous.is_empty(),
            "an anonymous list page is the empty page"
        );
    }

    /// The list page lives by the standard filters and exclusions like
    /// every other timeline: the viewer's mute clears the member off it,
    /// `with_muted` brings the rows back, and `only_media` narrows the
    /// kept rows to the attached ones.
    #[tokio::test]
    async fn list_pages_honour_mutes_and_only_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let circle = owned_list(&pool, &alice, "list", false).await;
        add_member(&pool, &circle, &bob).await;
        let plain = post(&pool, &bob, Visibility::Public, "plain member").await;
        let pictured = post(&pool, &bob, Visibility::Public, "pictured member").await;
        attach_media(&pool, pictured.id).await;
        let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
            .await
            .expect("alice mutes bob");
        assert_eq!(muted.muted_id, bob.id, "the mute is live");

        let list_source = || TimelineSource::List { list_id: circle.id };
        let hidden = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), list_source()),
        )
        .await;
        assert!(hidden.is_empty(), "the muted member leaves the list page");
        let shown = page_ids(
            &pool,
            &filtered(
                Some(as_viewer(&pool, &alice).await),
                list_source(),
                TimelineFilters {
                    with_muted: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            shown,
            newest_first(&[&plain, &pictured]),
            "with_muted lifts the viewer's own exclusion"
        );
        let media = page_ids(
            &pool,
            &filtered(
                Some(alice),
                list_source(),
                TimelineFilters {
                    with_muted: Flag::new(true),
                    only_media: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            media,
            newest_first(&[&pictured]),
            "only_media keeps the attached row"
        );
    }

    /// The `replies_policy` matrix of one list: `list` keeps only replies
    /// answering or @-mentioning a member or the viewer, `followed` also
    /// those answering or @-mentioning an accepted follow, and `none`
    /// drops every reply — a root never falls to the policy, and the
    /// non-members' roots that the replies answer stay off the page
    /// because membership, not the thread, fills it.
    #[tokio::test]
    async fn list_replies_policy_selects_whom_a_reply_may_answer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let eve = local(&pool, "eve").await;
        let dave_follow = follow::follow(&pool, BASE, &alice, &dave)
            .await
            .expect("alice follows dave");
        assert_eq!(
            dave_follow.state,
            follow::FollowState::Accepted,
            "the fixture follow is granted"
        );
        let circle = owned_list(&pool, &alice, "list", false).await;
        add_member(&pool, &circle, &bob).await;
        add_member(&pool, &circle, &carol).await;
        let carol_root = post(&pool, &carol, Visibility::Public, "member root").await;
        let dave_root = post(&pool, &dave, Visibility::Public, "followed root").await;
        let eve_root = post(&pool, &eve, Visibility::Public, "stranger root").await;
        let alice_root = post(&pool, &alice, Visibility::Public, "viewer root").await;
        let bob_root = post(&pool, &bob, Visibility::Public, "member root post").await;
        let to_member = reply(&pool, &bob, &carol_root, &[], "bob to carol").await;
        let to_followed = reply(&pool, &bob, &dave_root, &[], "bob to dave").await;
        let _to_stranger = reply(&pool, &bob, &eve_root, &[], "bob to eve").await;
        let to_viewer = reply(&pool, &bob, &alice_root, &[], "bob to alice").await;
        let mentions_member = reply(&pool, &bob, &eve_root, &[&carol], "bob cc carol").await;
        let mentions_followed = reply(&pool, &bob, &eve_root, &[&dave], "bob cc dave").await;

        let circle_id = circle.id;
        let page = |viewer: User| {
            filtered(
                Some(viewer),
                TimelineSource::List { list_id: circle_id },
                TimelineFilters::default(),
            )
        };

        let under_list = page_ids(&pool, &page(as_viewer(&pool, &alice).await)).await;
        assert_eq!(
            under_list,
            newest_first(&[
                &bob_root,
                &carol_root,
                &to_member,
                &to_viewer,
                &mentions_member
            ]),
            "`list` keeps roots and the replies answering or mentioning a member or the viewer"
        );

        set_policy(&pool, &alice, &circle, "followed").await;
        let under_followed = page_ids(&pool, &page(as_viewer(&pool, &alice).await)).await;
        assert_eq!(
            under_followed,
            newest_first(&[
                &bob_root,
                &carol_root,
                &to_member,
                &to_viewer,
                &mentions_member,
                &to_followed,
                &mentions_followed
            ]),
            "`followed` adds the replies answering or mentioning an accepted follow"
        );

        set_policy(&pool, &alice, &circle, "none").await;
        let under_none = page_ids(&pool, &page(alice)).await;
        assert_eq!(
            under_none,
            newest_first(&[&bob_root, &carol_root]),
            "`none` is no replies at all"
        );
    }

    /// An `exclusive` list takes its members back out of the viewer's
    /// `Home`: the member's posts, the member's own boosts, and even a
    /// stranger's boost **of** the member's post go, while a member of a
    /// non-exclusive list stays and the viewer's own rows are never in
    /// question — an account that lists itself keeps its place in its own
    /// home.
    #[tokio::test]
    async fn an_exclusive_list_removes_its_members_from_home() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        for followed in [&bob, &carol, &dave] {
            let granted = follow::follow(&pool, BASE, &alice, followed)
                .await
                .expect("alice follows the fixture accounts");
            assert_eq!(
                granted.state,
                follow::FollowState::Accepted,
                "every fixture account is open"
            );
        }
        let open = owned_list(&pool, &alice, "list", false).await;
        add_member(&pool, &open, &carol).await;
        let exclusive = owned_list(&pool, &alice, "list", true).await;
        add_member(&pool, &exclusive, &bob).await;
        add_member(&pool, &exclusive, &alice).await;
        let alice_own = post(&pool, &alice, Visibility::Public, "own post").await;
        let bob_post = post(&pool, &bob, Visibility::Public, "exclusive member").await;
        let carol_post = post(&pool, &carol, Visibility::Public, "open list member").await;
        let dave_post = post(&pool, &dave, Visibility::Public, "plain follow").await;
        let bob_boost = reblog(&pool, &bob, &dave_post).await;
        let dave_boost = reblog(&pool, &dave, &bob_post).await;

        let home = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Home),
        )
        .await;
        assert_eq!(
            home,
            newest_first(&[&alice_own, &carol_post, &dave_post]),
            "the exclusive member's rows are gone, her boost with them, and the viewer's \
             self-membership exempts her own post"
        );
        assert!(
            !home.contains(&bob_boost.id) && !home.contains(&dave_boost.id),
            "the member's boost and even the stranger's boost of the member's post stay out"
        );

        let relaxed = list::update(
            &pool,
            alice.id,
            exclusive.id,
            &ListChanges {
                title: None,
                replies_policy: None,
                exclusive: Some(false),
            },
        )
        .await
        .expect("the flag update succeeds")
        .expect("the fixture list exists");
        assert!(!relaxed.exclusive, "the list is no longer exclusive");
        let open_home = page_ids(&pool, &timeline(Some(alice), TimelineSource::Home)).await;
        assert_eq!(
            open_home,
            newest_first(&[
                &alice_own,
                &bob_post,
                &carol_post,
                &dave_post,
                &bob_boost,
                &dave_boost
            ]),
            "with exclusivity off, every followed row rides home again"
        );
    }

    /// `local_only`, `remote_only` and `instance` split the page by who
    /// authored it: rows of this instance, mirrored rows, and the mirror of
    /// one named domain (a local author has no domain and never matches).
    #[tokio::test]
    async fn local_remote_and_instance_split_by_origin() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mallory = remote(&pool, "mallory", "remote.example").await;
        let pete = remote(&pool, "pete", "other.example").await;
        let ours = post(&pool, &alice, Visibility::Public, "ours").await;
        let theirs = remote_post(&pool, &mallory, "remote-1", &[]).await;
        let others = remote_post(&pool, &pete, "other-1", &[]).await;

        let local_page = page_ids(
            &pool,
            &filtered(
                None,
                TimelineSource::Public,
                TimelineFilters {
                    local_only: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            local_page,
            newest_first(&[&ours]),
            "local_only keeps this instance's rows"
        );
        let remote_page = page_ids(
            &pool,
            &filtered(
                None,
                TimelineSource::Public,
                TimelineFilters {
                    remote_only: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            remote_page,
            newest_first(&[&theirs, &others]),
            "remote_only keeps the mirrored rows"
        );
        let one_domain = page_ids(
            &pool,
            &filtered(
                None,
                TimelineSource::Public,
                TimelineFilters {
                    instance: Some("remote.example".to_owned()),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            one_domain,
            newest_first(&[&theirs]),
            "instance names one author domain"
        );
        let contradictory = page_ids(
            &pool,
            &filtered(
                None,
                TimelineSource::Public,
                TimelineFilters {
                    local_only: Flag::new(true),
                    remote_only: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert!(
            contradictory.is_empty(),
            "an author is from here or from away, never both"
        );
    }

    /// `only_media` keeps rows with an `media_attachments` child;
    /// `pinned_only` keeps rows their author pinned; the two compose with
    /// AND, and an unfiltered page is the control.
    #[tokio::test]
    async fn media_and_pin_filters_narrow_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let plain = post(&pool, &alice, Visibility::Public, "plain").await;
        let with_media = post(&pool, &alice, Visibility::Public, "with media").await;
        let pinned = post(&pool, &alice, Visibility::Public, "pinned").await;
        let both = post(&pool, &alice, Visibility::Public, "media and pinned").await;
        attach_media(&pool, with_media.id).await;
        attach_media(&pool, both.id).await;
        let first_pin = pinned_status::pin(&pool, alice.id, pinned.id)
            .await
            .expect("the pin writes");
        assert!(first_pin.is_some(), "alice pins her status");
        let second_pin = pinned_status::pin(&pool, alice.id, both.id)
            .await
            .expect("the second pin writes");
        assert!(second_pin.is_some(), "alice pins the combined status too");

        let control = page_ids(&pool, &timeline(None, TimelineSource::Public)).await;
        assert_eq!(
            control,
            newest_first(&[&plain, &with_media, &pinned, &both]),
            "unfiltered, the page is all four posts"
        );
        let media_page = page_ids(
            &pool,
            &filtered(
                None,
                TimelineSource::Public,
                TimelineFilters {
                    only_media: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            media_page,
            newest_first(&[&with_media, &both]),
            "only_media keeps attached rows"
        );
        let pin_page = page_ids(
            &pool,
            &filtered(
                None,
                TimelineSource::Public,
                TimelineFilters {
                    pinned_only: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            pin_page,
            newest_first(&[&pinned, &both]),
            "pinned_only keeps the author's pins"
        );
        let combined = page_ids(
            &pool,
            &filtered(
                None,
                TimelineSource::Public,
                TimelineFilters {
                    only_media: Flag::new(true),
                    pinned_only: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            combined,
            newest_first(&[&both]),
            "the two compose with AND: a pinned post with media answers both filters"
        );
    }

    /// `tagged` narrows whatever rows the source selected to those carrying
    /// one stored tag name and adds no visibility of its own (unlike
    /// [`TimelineSource::Hashtag`], which is `public` only): a follower
    /// still reaches the author's `private` post, an anonymous request does
    /// not, and a name no row carries answers the empty page.
    #[tokio::test]
    async fn tagged_narrows_the_source_without_adding_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let alice_follow = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows open bob");
        assert_eq!(
            alice_follow.state,
            follow::FollowState::Accepted,
            "bob is unlocked, so the follow is granted"
        );
        let bob_id = bob.id;
        let account = move || TimelineSource::Account { user_id: bob_id };
        let rust_public = post(&pool, &bob, Visibility::Public, "public #Rust").await;
        let rust_private = post(&pool, &bob, Visibility::Private, "private #rust").await;
        let untagged = post(&pool, &bob, Visibility::Public, "no tag at all").await;
        let other_tag = post(&pool, &bob, Visibility::Public, "public #elixir").await;
        let tagged = |name: &str| TimelineFilters {
            tagged: Some(name.to_owned()),
            ..TimelineFilters::default()
        };

        let for_alice = page_ids(
            &pool,
            &filtered(
                Some(as_viewer(&pool, &alice).await),
                account(),
                tagged("rust"),
            ),
        )
        .await;
        assert_eq!(
            for_alice,
            newest_first(&[&rust_public, &rust_private]),
            "a follower reaches both of the author's `#rust` rows, private included"
        );
        let anonymous = page_ids(&pool, &filtered(None, account(), tagged("rust"))).await;
        assert_eq!(
            anonymous,
            newest_first(&[&rust_public]),
            "the anonymous tagged page is the public row only"
        );
        let uncarried = page_ids(&pool, &filtered(None, account(), tagged("haskell"))).await;
        assert!(
            uncarried.is_empty(),
            "a tag no row carries answers the empty page"
        );
        let control = page_ids(&pool, &timeline(None, account())).await;
        assert_eq!(
            control,
            newest_first(&[&rust_public, &untagged, &other_tag]),
            "without the filter the author's other public rows are there"
        );
    }

    /// `exclude_replies` drops rows with a parent, `exclude_reblogs` drops
    /// boost rows, and the two together leave only a plain root.
    #[tokio::test]
    async fn exclude_replies_and_reblogs_drop_those_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let root = post(&pool, &alice, Visibility::Public, "thread root").await;
        let answer = reply(&pool, &bob, &root, &[], "a reply").await;
        let boost = reblog(&pool, &carol, &root).await;

        let shapes = |replies: bool, reblogs: bool| TimelineFilters {
            exclude_replies: Flag::new(replies),
            exclude_reblogs: Flag::new(reblogs),
            ..TimelineFilters::default()
        };
        let no_replies = page_ids(
            &pool,
            &filtered(None, TimelineSource::Public, shapes(true, false)),
        )
        .await;
        assert_eq!(
            no_replies,
            newest_first(&[&root, &boost]),
            "the reply is out, root and boost stay"
        );
        let no_reblogs = page_ids(
            &pool,
            &filtered(None, TimelineSource::Public, shapes(false, true)),
        )
        .await;
        assert_eq!(
            no_reblogs,
            newest_first(&[&root, &answer]),
            "the boost is out"
        );
        let plain_only = page_ids(
            &pool,
            &filtered(None, TimelineSource::Public, shapes(true, true)),
        )
        .await;
        assert_eq!(
            plain_only,
            newest_first(&[&root]),
            "both together leave only the root"
        );
    }

    /// `exclude_visibilities` drops the named visibilities from whatever
    /// the viewer would otherwise reach; the empty list drops nothing.
    #[tokio::test]
    async fn exclude_visibilities_drops_named_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let alice_id = alice.id;
        let account = move || TimelineSource::Account { user_id: alice_id };
        let public = post(&pool, &alice, Visibility::Public, "x public").await;
        let unlisted = post(&pool, &alice, Visibility::Unlisted, "x unlisted").await;
        let private = post(&pool, &alice, Visibility::Private, "x private").await;
        let instance_local = post(&pool, &alice, Visibility::Local, "x local").await;
        let dm = direct(&pool, &alice, &[], "x dm").await;

        let without = |names: &[Visibility]| TimelineFilters {
            exclude_visibilities: names.to_vec(),
            ..TimelineFilters::default()
        };
        let everything = page_ids(
            &pool,
            &filtered(
                Some(as_viewer(&pool, &alice).await),
                account(),
                without(&[]),
            ),
        )
        .await;
        assert_eq!(
            everything,
            newest_first(&[&public, &unlisted, &private, &instance_local, &dm]),
            "the author's own page holds everything before any exclusion"
        );
        let no_public = page_ids(
            &pool,
            &filtered(
                Some(alice),
                account(),
                without(&[Visibility::Public, Visibility::Unlisted]),
            ),
        )
        .await;
        assert_eq!(
            no_public,
            newest_first(&[&private, &instance_local, &dm]),
            "excluding public and unlisted leaves the rest"
        );
        let emptied_public_timeline = page_ids(
            &pool,
            &filtered(None, TimelineSource::Public, without(&[Visibility::Public])),
        )
        .await;
        assert!(
            emptied_public_timeline.is_empty(),
            "on the public timeline, excluding `public` excludes the whole timeline"
        );
    }

    /// `reply_visibility` judges each reply by its stored target and
    /// @-mentions: `self` keeps only replies addressed to the viewer
    /// (answering them or mentioning them), `following` keeps replies whose
    /// target or mention is the viewer or an account they follow; roots and
    /// the viewer's own replies always survive, and without a viewer the
    /// filter asks nothing of anyone.
    #[tokio::test]
    async fn reply_visibility_following_and_self_filter_replies() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let _friend = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows bob");
        let bob_root = post(&pool, &bob, Visibility::Public, "bob root").await;
        let alice_root = post(&pool, &alice, Visibility::Public, "alice root").await;
        let carol_root = post(&pool, &carol, Visibility::Public, "carol root").await;
        let to_followed = reply(&pool, &carol, &bob_root, &[], "carol to bob").await;
        let to_stranger = reply(&pool, &bob, &carol_root, &[], "bob to carol").await;
        let mention_alice = reply(&pool, &bob, &carol_root, &[&alice], "cc alice").await;
        let mention_bob = reply(&pool, &dave, &carol_root, &[&bob], "dave cc bob").await;
        let to_alice = reply(&pool, &dave, &alice_root, &[], "dave to alice").await;
        let alice_own = reply(&pool, &alice, &carol_root, &[], "alice to carol").await;
        let all = [
            &bob_root,
            &alice_root,
            &carol_root,
            &to_followed,
            &to_stranger,
            &mention_alice,
            &mention_bob,
            &to_alice,
            &alice_own,
        ];

        let following = page_ids(
            &pool,
            &reply_page(
                Some(as_viewer(&pool, &alice).await),
                ReplyVisibility::Following,
            ),
        )
        .await;
        assert_eq!(
            following,
            minus(&all, &[&to_stranger]),
            "`following` keeps every reply except the one to an unfollowed stranger — \
             replies to the viewer and mentions of them or of the followed bob all stay"
        );
        let selfed = page_ids(&pool, &reply_page(Some(alice), ReplyVisibility::SelfOnly)).await;
        assert_eq!(
            selfed,
            minus(&all, &[&to_followed, &to_stranger, &mention_bob]),
            "`self` keeps only replies answering or mentioning the viewer"
        );
        let anonymous = page_ids(&pool, &reply_page(None, ReplyVisibility::SelfOnly)).await;
        assert_eq!(
            anonymous,
            newest_first(&all),
            "with no viewer there is no one to reply to or for, so the filter appends nothing"
        );
    }

    /// Blocks, unexpired mutes and domain blocks pull an author's rows out
    /// of every timeline the viewer reads — and so do reblogs **of** those
    /// rows — while a lapsed mute counts as absent and `with_muted` waves
    /// the whole group back through. None of it touches an anonymous page.
    #[tokio::test]
    async fn blocked_muted_and_domain_blocked_authors_leave_the_page() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let eve = local(&pool, "eve").await;
        let mallory = remote(&pool, "mallory", "remote.example").await;
        let carol_block = block::block(&pool, BASE, &alice, &carol)
            .await
            .expect("alice blocks carol");
        assert_eq!(
            carol_block.blocked_id, carol.id,
            "the fixture block is on carol"
        );
        let dave_mute = mute::mute(&pool, &alice, &dave, MuteOptions::default())
            .await
            .expect("alice mutes dave");
        assert_eq!(dave_mute.muted_id, dave.id, "the fixture mute is on dave");
        let eve_mute = mute::mute(&pool, &alice, &eve, MuteOptions::default())
            .await
            .expect("alice mutes eve");
        assert_eq!(eve_mute.muted_id, eve.id, "the second mute is on eve");
        lapse_mute(&pool, &alice, &eve).await;
        domain_block::block_domain(&pool, alice.id, "remote.example")
            .await
            .expect("alice domain-blocks remote.example");
        let carol_post = post(&pool, &carol, Visibility::Public, "blocked carol").await;
        let dave_post = post(&pool, &dave, Visibility::Public, "muted dave").await;
        let eve_post = post(&pool, &eve, Visibility::Public, "lapsed eve").await;
        let mallory_post = remote_post(&pool, &mallory, "mal-public", &[]).await;
        let bob_post = post(&pool, &bob, Visibility::Public, "plain bob").await;
        let bob_reb_carol = reblog(&pool, &bob, &carol_post).await;
        let bob_reb_dave = reblog(&pool, &bob, &dave_post).await;

        let for_alice = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Public),
        )
        .await;
        assert_eq!(
            for_alice,
            newest_first(&[&bob_post, &eve_post]),
            "blocked and muted authors are out, boosts of their rows with them, a domain \
             block takes its remote rows, and a lapsed mute counts as absent"
        );
        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Public)).await;
        assert_eq!(
            anonymous,
            newest_first(&[
                &carol_post,
                &dave_post,
                &eve_post,
                &mallory_post,
                &bob_post,
                &bob_reb_carol,
                &bob_reb_dave,
            ]),
            "none of this is an anonymous request's problem"
        );
        let with_muted = page_ids(
            &pool,
            &filtered(
                Some(alice),
                TimelineSource::Public,
                TimelineFilters {
                    with_muted: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            with_muted, anonymous,
            "with_muted brings the whole group back, exactly the unfiltered page"
        );
    }

    /// A thread mute takes the muted conversation's foreign rows out of the
    /// page — the root and every reply in it, boosts of its rows included —
    /// but never the viewer's own posts in that thread; another account
    /// reads unaffected, and `with_muted` overrides.
    #[tokio::test]
    async fn thread_mutes_hide_the_thread_except_the_viewers_own_posts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let root = post(&pool, &bob, Visibility::Public, "muted thread root").await;
        let alice_in_thread = reply(&pool, &alice, &root, &[], "alice answers in it").await;
        let carol_in_thread = reply(&pool, &carol, &root, &[], "carol answers in it").await;
        let dave_elsewhere = post(&pool, &dave, Visibility::Public, "another thread").await;
        let boost_of_thread = reblog(&pool, &dave, &alice_in_thread).await;
        conversation_mute::mute(&pool, alice.id, root.conversation_id)
            .await
            .expect("alice mutes the thread");

        let whole_thread = newest_first(&[
            &root,
            &alice_in_thread,
            &carol_in_thread,
            &dave_elsewhere,
            &boost_of_thread,
        ]);
        let for_alice = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Public),
        )
        .await;
        assert_eq!(
            for_alice,
            newest_first(&[&alice_in_thread, &dave_elsewhere]),
            "the foreign root, the foreign reply and even a boost of the viewer's own row \
             are all out of the muted thread"
        );
        let for_bob = page_ids(&pool, &timeline(Some(bob), TimelineSource::Public)).await;
        assert_eq!(
            for_bob, whole_thread,
            "the mute is alice's, nobody else reads through it"
        );
        let for_alice_unmuted = page_ids(
            &pool,
            &filtered(
                Some(alice),
                TimelineSource::Public,
                TimelineFilters {
                    with_muted: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            for_alice_unmuted, whole_thread,
            "with_muted restores the muted thread too"
        );
        assert!(
            !for_alice.contains(&carol_in_thread.id) && !for_alice.contains(&boost_of_thread.id),
            "the muted thread keeps nothing of carol's or dave's for alice"
        );
    }

    /// A tombstone is off every timeline even for its author, while a status
    /// whose `expires_at` has passed stays readable until the sweep takes it.
    #[tokio::test]
    async fn tombstones_are_out_and_expired_statuses_stay() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let live = post(&pool, &alice, Visibility::Public, "live post").await;
        let expiring = status::insert(
            &pool,
            BASE,
            &NewStatus {
                expires_at: Some(Utc::now() - Duration::hours(1)),
                ..NewStatus::local(&alice, "due for deletion, not yet swept")
            },
        )
        .await
        .expect("the expiring post inserts");
        let doomed = post(&pool, &alice, Visibility::Public, "already deleted").await;
        let gone = soft_delete(&pool, doomed.id)
            .await
            .expect("the delete runs")
            .expect("the post was live");
        assert!(gone.deleted_at.is_some(), "the row is a tombstone now");

        let own_page = page_ids(
            &pool,
            &timeline(
                Some(alice),
                TimelineSource::Account {
                    user_id: doomed.user_id,
                },
            ),
        )
        .await;
        assert_eq!(
            own_page,
            newest_first(&[&live, &expiring]),
            "the tombstone is gone, the un-swept expired row is not"
        );
    }

    /// Keyset pagination walks `statuses.id` (decision 4): a plain page is
    /// newest-first, `max_id` continues strictly older, `since_id` reads the
    /// newest rows after the cursor still newest-first, and a `min_id` page
    /// is read ascending and answered flipped.
    #[tokio::test]
    async fn keyset_pages_walk_ids_and_min_id_flips() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        for n in 0..5 {
            let _status = post(
                &pool,
                &alice,
                Visibility::Public,
                &format!("paged post {n}"),
            )
            .await;
        }
        let alice_id = alice.id;
        let all = all_pages(&pool, alice_id, full_page()).await;
        assert_eq!(all.len(), 5, "the whole page is the fixture");
        let on = |n: usize| all[n];

        let first = all_pages(&pool, alice_id, two_rows(PageCursor::None)).await;
        assert_eq!(
            first,
            vec![on(0), on(1)],
            "a plain page takes the two newest, newest first"
        );
        let second = all_pages(&pool, alice_id, two_rows(PageCursor::Max(on(1)))).await;
        assert_eq!(
            second,
            vec![on(2), on(3)],
            "max_id continues strictly older"
        );
        let tail = all_pages(&pool, alice_id, two_rows(PageCursor::Max(on(3)))).await;
        assert_eq!(tail, vec![on(4)], "the walk ends on the oldest row");
        let polling = all_pages(&pool, alice_id, two_rows(PageCursor::Since(on(3)))).await;
        assert_eq!(
            polling,
            vec![on(0), on(1)],
            "since_id reads the newer rows, still answered newest-first"
        );
        let flipped = all_pages(&pool, alice_id, two_rows(PageCursor::Min(on(3)))).await;
        assert_eq!(
            flipped,
            vec![on(1), on(2)],
            "min_id takes the page just newer than the cursor — read ascending, answered flipped"
        );
        let nothing_newer = all_pages(&pool, alice_id, two_rows(PageCursor::Min(on(0)))).await;
        assert!(
            nothing_newer.is_empty(),
            "nothing is newer than the newest row"
        );
    }

    /// The filter-context map the filter ticket gives the timeline routes:
    /// `Home` is `home`, `Public` and `Hashtag` are `public`, `Account` is
    /// `account`, and the two sources no v1 context names — `Direct` and
    /// `List` — answer `None` (unfiltered pages).
    #[test]
    fn sources_map_to_the_filter_contexts_the_endpoints_render_in() {
        assert_eq!(
            TimelineSource::Home.filter_context(),
            Some(FilterContext::Home)
        );
        assert_eq!(
            TimelineSource::Public.filter_context(),
            Some(FilterContext::Public)
        );
        assert_eq!(
            TimelineSource::Hashtag {
                any: Vec::new(),
                all: Vec::new(),
                none: Vec::new(),
            }
            .filter_context(),
            Some(FilterContext::Public)
        );
        assert_eq!(
            TimelineSource::Account {
                user_id: Uuid::now_v7()
            }
            .filter_context(),
            Some(FilterContext::Account)
        );
        assert_eq!(TimelineSource::Direct.filter_context(), None);
        assert_eq!(
            TimelineSource::List {
                list_id: Uuid::now_v7()
            }
            .filter_context(),
            None
        );
    }
}
