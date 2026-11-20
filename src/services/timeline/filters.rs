//! The optional narrowings of a timeline request: [`TimelineFilters`], the
//! Pleroma timeline parameters, and the conditions they append to the
//! statement.

use sqlx::{Postgres, QueryBuilder};

use crate::models::{
    status::Visibility,
    user::{Flag, User},
};

use super::{ReplyVisibility, TAG_JOIN_SQL, visibility_arms::push_reply_visibility};

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
    ///
    /// [`push_visibility_clause`]: crate::models::status_visibility::push_visibility_clause
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
    /// [`TimelineSource::Hashtag`](super::TimelineSource::Hashtag) this narrows whatever the source already
    /// selected and never adds a visibility of its own.
    pub tagged: Option<String>,
}

impl TimelineFilters {
    /// Appends the optional filters' conditions to `builder`, each in its
    /// own ` AND ` group; everything off / empty / `None` appends nothing.
    pub(super) fn push_clauses(&self, builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{follow, pinned_status},
        services::timeline::{
            TimelineSource,
            test_support::{
                BASE, as_viewer, attach_media, direct, filtered, local, newest_first, page_ids,
                post, reblog, remote, remote_post, reply, timeline,
            },
        },
    };

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
}
