//! Where a timeline's rows come from: [`TimelineSource`], the `from` half of
//! the query, and [`ReplyVisibility`], the one filter value that judges a
//! reply by who it addresses.

use uuid::Uuid;

use crate::services::filter::FilterContext;

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
/// [`push_visibility_clause`]: crate::models::status_visibility::push_visibility_clause
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
    /// [`fetch`](super::TimelineQuery::fetch) answers it the empty
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
    /// [`exclude_reblogs`](super::TimelineFilters::exclude_reblogs) for Mastodon's
    /// default listing). Each row is still judged by
    /// [`push_visibility_clause`](crate::models::status_visibility::push_visibility_clause), so followers see `private` and the
    /// mentioned parties see `direct` where a stranger sees only `public`
    /// and `unlisted`.
    Account {
        /// The author every row of this timeline is written by.
        user_id: Uuid,
    },
    /// One list's timeline: every row authored by an account of the list's
    /// `list_members` — membership alone is enough, a follow of the member
    /// is not required — reblog rows by members included, each row still
    /// judged by [`push_visibility_clause`](crate::models::status_visibility::push_visibility_clause) (a member's `private` post
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
    /// [`fetch`](super::TimelineQuery::fetch) answers it the empty page without
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
    /// where the v1 filter contexts do not reach: [`Self::Home`] and
    /// [`Self::List`] are the `home` context (Mastodon's "Home and lists",
    /// where `FeedManager#filter` runs `filter_from_home` for `:list` feeds
    /// too), [`Self::Public`] and [`Self::Hashtag`] the `public` one and
    /// [`Self::Account`] the `account` one. [`Self::Direct`] names no v1
    /// context, and the Pleroma extension built on it is an unfiltered
    /// page.
    #[must_use]
    pub const fn filter_context(&self) -> Option<FilterContext> {
        match self {
            Self::Home | Self::List { .. } => Some(FilterContext::Home),
            Self::Public | Self::Hashtag { .. } => Some(FilterContext::Public),
            Self::Account { .. } => Some(FilterContext::Account),
            Self::Direct => None,
        }
    }
}

/// Pleroma's `reply_visibility` timeline parameter
/// (`lib/pleroma/web/mastodon_api/controllers/timeline_controller.ex`), the
/// [`TimelineFilters::reply_visibility`] values.
///
/// Only a reply's stored target decides: its [`in_reply_to_account_id`](crate::models::status::Status::in_reply_to_account_id)
/// and its @-mentions (the [`status_mentions`] rows), which are this schema's
/// relational reading of Pleroma's `recipients` list; a status that replies
/// to nothing is never dropped. The filter applies only when there is a
/// viewer to reply *to* — an anonymous request ignores it, as Pleroma's
/// `restrict_replies` clause that never matches does.
///
/// [`Status::in_reply_to_account_id`]: crate::models::status::Status::in_reply_to_account_id
/// [`status_mentions`]: crate::models::mention
/// [`TimelineFilters::reply_visibility`]: super::TimelineFilters::reply_visibility
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The filter-context map the filter ticket gives the timeline routes:
    /// `Home` and `List` are `home`, `Public` and `Hashtag` are `public`,
    /// `Account` is `account`, and `Direct`, which no v1 context names,
    /// answers `None` (an unfiltered page).
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
            Some(FilterContext::Home)
        );
    }
}
