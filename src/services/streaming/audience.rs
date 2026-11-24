//! The per-visibility audience answers: for one stored status, the stream
//! names, [`Audience`] and locality each visibility resolves to. The names
//! and id rosters they assemble come from [`streams`], the stored-row facts
//! from [`super::facts`] and the list rule from [`super::lists`].

use crate::{
    error::AppResult,
    models::{status::Status, user::User},
    services::streaming::{
        facts::{has_attachments, sorted_names, status_tag_names, users_audience},
        lists::list_targets,
    },
    state::AppState,
    streaming::hub::Audience,
};

use self::streams::{
    followers_of, hashtag_streams, home_ids, local_hashtag_streams, mentioned_local_ids,
    public_timeline_streams, user_streams,
};

mod streams;

/// The `public` visibility's answer: the public timeline set, the hashtag
/// streams, and the home and list deliveries, gated by nobody.
pub(super) async fn public_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let has_media = has_attachments(&state.db, row.id).await?;
    let mut streams = public_timeline_streams(row, author, has_media);
    let tags = status_tag_names(&state.db, row.id).await?;
    streams.extend(hashtag_streams(&tags, row.local));
    let home = home_ids(&state.db, row, author).await?;
    streams.extend(user_streams(&home));
    let (list_streams, _owners_gated_by_public_anyway) =
        list_targets(&state.db, row, author).await?;
    streams.extend(list_streams);
    Ok((sorted_names(streams), Audience::Public, false))
}

/// The `unlisted` visibility's answer: hashtag, list and home streams —
/// no `public*` timeline ever — gated by `Audience::Users` over the home
/// circle plus the list owners, the event's only viewers besides itself.
pub(super) async fn unlisted_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let tags = status_tag_names(&state.db, row.id).await?;
    let mut streams = hashtag_streams(&tags, row.local);
    let home = home_ids(&state.db, row, author).await?;
    streams.extend(user_streams(&home));
    let (list_streams, owners) = list_targets(&state.db, row, author).await?;
    streams.extend(list_streams);
    Ok((sorted_names(streams), users_audience(home, &owners), false))
}

/// The `private` visibility's answer: the `user:{id}` streams of the author,
/// the accepted local followers and the mentioned local users, matching
/// `status_visibility` rule 5 (a remote mention holds no socket here).
pub(super) async fn private_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let mut ids = vec![author.id];
    ids.extend(followers_of(&state.db, row, author).await?);
    ids.extend(mentioned_local_ids(&state.db, row.id).await?);
    ids.sort_unstable();
    ids.dedup();
    let streams = user_streams(&ids);
    Ok((streams, Audience::Users(ids), false))
}

/// The `direct` visibility's answer: the author and every mentioned LOCAL
/// user, each on their own `user:{id}` and `direct:{id}` streams, and no
/// public, hashtag or list stream anywhere.
pub(super) async fn direct_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let mut recipients = vec![author.id];
    recipients.extend(mentioned_local_ids(&state.db, row.id).await?);
    recipients.sort_unstable();
    recipients.dedup();
    let mut streams = Vec::with_capacity(recipients.len() * 2);
    for id in &recipients {
        streams.push(format!("direct:{id}"));
        streams.push(format!("user:{id}"));
    }
    Ok((sorted_names(streams), Audience::Users(recipients), false))
}

/// The `local` visibility's answer: the local half of the public set —
/// `public:local`, its media variant, and the local hashtag streams —
/// with nobody specific to gate: every authenticated local user may see
/// the post, so the audience is `Everyone` and `local_only` is set, which
/// is what drops it on every socket without a reader.
pub(super) async fn local_targets(
    state: &AppState,
    row: &Status,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let mut streams = vec!["public:local".to_owned()];
    if has_attachments(&state.db, row.id).await? {
        streams.push("public:local:media".to_owned());
    }
    let tags = status_tag_names(&state.db, row.id).await?;
    streams.extend(local_hashtag_streams(&tags));
    Ok((sorted_names(streams), Audience::Everyone, true))
}

#[cfg(test)]
mod tests {
    use crate::{
        models::status::Visibility,
        services::streaming::test_support::{
            app_state, attach_media, boost, canonical, direct_stream, follow_user, hide_boosts,
            inactive_user, list_of, list_stream, local_user, post, post_vis, published,
            remote_post, remote_user, sorted_ids, user_stream, users_ids,
        },
    };

    use super::*;

    #[tokio::test]
    async fn public_local_post_streams_the_public_timeline_and_home() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let erin = local_user(&pool, "erin").await;
        let row = post(&pool, &alice, "hello world").await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                user_stream(alice.id),
                user_stream(bob.id),
            ]),
            "the public timeline plus the author's and the follower's home streams"
        );
        assert!(
            !streams.contains(&user_stream(erin.id)),
            "and not an unrelated account's home"
        );
        assert_eq!(audience, Audience::Public, "a public post gates nobody");
        assert!(!local_only, "a public post is not a local-only event");
    }

    #[tokio::test]
    async fn public_remote_post_streams_the_remote_timelines_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let carol = remote_user(&pool, "carol").await;

        let tagged = remote_post(&pool, &carol, "over there", &["Rust"]).await;
        let (streams, audience, local_only) = published(&state, &tagged, &carol).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public".to_owned(),
                "public:remote".to_owned(),
                "public:remote:remote.example".to_owned(),
                "hashtag:rust".to_owned(),
                user_stream(carol.id),
            ]),
            "the remote variants stand in for the local ones, and a remote post rides \
             the global tag timeline but no local one"
        );
        assert_eq!(audience, Audience::Public, "a public post gates nobody");
        assert!(!local_only, "a remote post is not local-only either");

        let with_media = remote_post(&pool, &carol, "over there again", &[]).await;
        attach_media(&pool, &carol, &with_media).await;
        let (media_streams, _, _) = published(&state, &with_media, &carol).await;
        assert_eq!(
            media_streams,
            canonical(vec![
                "public".to_owned(),
                "public:remote".to_owned(),
                "public:remote:remote.example".to_owned(),
                "public:media".to_owned(),
                "public:remote:media".to_owned(),
                user_stream(carol.id),
            ]),
            "attachments ride the media timelines, remote-narrowed"
        );
    }

    #[tokio::test]
    async fn public_tagged_and_media_posts_add_their_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;

        let tagged = post(&pool, &alice, "hello #Rust world").await;
        let (tag_streams, _, _) = published(&state, &tagged, &alice).await;
        assert_eq!(
            tag_streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                "hashtag:rust".to_owned(),
                "hashtag:local:rust".to_owned(),
                user_stream(alice.id),
            ]),
            "a local tagged post rides both hashtag timelines, the stored name lowercased"
        );

        let media = post(&pool, &alice, "look at this").await;
        attach_media(&pool, &alice, &media).await;
        let (media_streams, _, _) = published(&state, &media, &alice).await;
        assert_eq!(
            media_streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                "public:media".to_owned(),
                "public:local:media".to_owned(),
                user_stream(alice.id),
            ]),
            "a local media post rides both media timelines"
        );
    }

    #[tokio::test]
    async fn unlisted_post_streams_hashtags_lists_and_home_but_no_public_timeline() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let alice_list = list_of(&pool, &dana, &alice, "followed").await;
        let erin = local_user(&pool, "erin").await;

        let row = post_vis(
            &pool,
            &alice,
            "quietly #Rust",
            Visibility::Unlisted,
            &[erin.id],
        )
        .await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                "hashtag:rust".to_owned(),
                "hashtag:local:rust".to_owned(),
                user_stream(alice.id),
                user_stream(bob.id),
                user_stream(dana.id),
                user_stream(erin.id),
                list_stream(alice_list.id),
            ]),
            "hashtag, home (author, followers, the mentioned non-follower) and list — \
             and no public timeline stream at all"
        );
        assert_eq!(
            users_ids(&audience),
            sorted_ids(vec![alice.id, bob.id, erin.id, dana.id]),
            "the audience is the home circle plus the list owner"
        );
        assert!(!local_only, "unlisted is still a federated post");
    }

    #[tokio::test]
    async fn private_post_streams_author_followers_and_mentioned_local_users() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let carol = remote_user(&pool, "carol").await;
        follow_user(&pool, &carol, &alice).await;
        let dave = inactive_user(&pool, "dave").await;
        follow_user(&pool, &dave, &alice).await;
        let erin = local_user(&pool, "erin").await;

        let row = post_vis(
            &pool,
            &alice,
            "followers only",
            Visibility::Private,
            &[erin.id],
        )
        .await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                user_stream(alice.id),
                user_stream(bob.id),
                user_stream(erin.id),
            ]),
            "the author, the one local active accepted follower and the mentioned local \
             non-follower — not the remote mirror, not the deactivated account"
        );
        assert_eq!(
            users_ids(&audience),
            sorted_ids(vec![alice.id, bob.id, erin.id]),
            "and the audience is exactly those three"
        );
        assert!(!local_only, "private is still a federated post");
    }

    #[tokio::test]
    async fn private_post_does_not_stream_to_a_mentioned_remote_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let carol = remote_user(&pool, "carol").await;

        let row = post_vis(
            &pool,
            &alice,
            "followers only",
            Visibility::Private,
            &[carol.id],
        )
        .await;

        let (streams, audience, _) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![user_stream(alice.id)]),
            "a remote mention holds no socket here, so only the author is streamed to"
        );
        assert_eq!(
            users_ids(&audience),
            sorted_ids(vec![alice.id]),
            "and the audience holds local ids only"
        );
    }

    #[tokio::test]
    async fn direct_post_streams_recipients_user_and_direct_names() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = remote_user(&pool, "carol").await;

        let row = post_vis(
            &pool,
            &alice,
            "for a few eyes only",
            Visibility::Direct,
            &[bob.id, carol.id],
        )
        .await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                user_stream(alice.id),
                direct_stream(alice.id),
                user_stream(bob.id),
                direct_stream(bob.id),
            ]),
            "author and every mentioned LOCAL user on both their streams — the remote \
             mention gets nothing, and no public or hashtag stream ever does"
        );
        assert_eq!(
            users_ids(&audience),
            sorted_ids(vec![alice.id, bob.id]),
            "the audience is exactly the recipients"
        );
        assert!(!local_only, "a direct post is not the local visibility");
    }

    #[tokio::test]
    async fn local_visibility_post_is_everyone_local_only_on_the_local_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;

        let row = post_vis(&pool, &alice, "instance only #Rust", Visibility::Local, &[]).await;
        attach_media(&pool, &alice, &row).await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public:local".to_owned(),
                "public:local:media".to_owned(),
                "hashtag:local:rust".to_owned(),
            ]),
            "the local half of the public set — no global streams, no home or list delivery"
        );
        assert_eq!(
            audience,
            Audience::Everyone,
            "every authenticated local user may see it"
        );
        assert!(
            local_only,
            "and no socket without a reader may see it at all"
        );
    }

    #[tokio::test]
    async fn a_reblog_skips_the_followers_who_hidden_boosts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let dave = local_user(&pool, "dave").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &dave).await;
        hide_boosts(&pool, &bob, &dave).await;
        let carol = local_user(&pool, "carol").await;
        follow_user(&pool, &carol, &dave).await;

        let own_post = post(&pool, &dave, "a plain post").await;
        let (own_streams, _, _) = published(&state, &own_post, &dave).await;
        assert!(
            own_streams.contains(&user_stream(bob.id)),
            "dave's own post reaches the follower who hides boosts"
        );

        let original = post(&pool, &alice, "worth boosting").await;
        let row = boost(&pool, &dave, &original).await;

        let (streams, audience, local_only) = published(&state, &row, &dave).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                user_stream(dave.id),
                user_stream(carol.id),
            ]),
            "the boost lands everywhere dave's own posts do, minus the home of the \
             follower who turned reblogs off"
        );
        assert_eq!(
            audience,
            Audience::Public,
            "a boost of a public post keeps the public audience"
        );
        assert!(!local_only, "a boost of a public post is not local-only");
    }
}
