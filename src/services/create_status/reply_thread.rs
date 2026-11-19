//! The service-level tests of the reply arms in [`super::reply`].
//!
//! What a stored reply inherits from its parent — ids, thread, counted
//! replies, language, deduplicated auto-mentions, capped audience — and
//! the shared 404 for a parent that cannot be answered.

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::{
        models::{
            status::{self, NewStatus},
            user::User,
        },
        services::create_status::{
            CreateStatusParams, MAX_INHERITED_MENTIONS, create_status,
            test_support::{
                BASE, created, live_ids, mention_ids, missed, parent_row, plain_post, posted,
                replying, state, stored, user,
            },
        },
    };
    #[tokio::test]
    async fn a_reply_stores_its_parents_fields_counts_and_language() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "bob").await;
        let parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                language: Some("eo".to_owned()),
                ..posted("the parent")
            },
        )
        .await;
        let parent_id = Uuid::parse_str(&parent.id).expect("the entity carries its id");

        let answer = created(&app_state, &alice, replying("right back", parent_id)).await;
        let row = stored(pool, &answer).await;

        assert_eq!(
            row.in_reply_to_id,
            Some(parent_id),
            "the reply names its parent"
        );
        assert_eq!(
            row.in_reply_to_account_id,
            Some(bob.id),
            "and the parent's author"
        );
        assert_eq!(
            row.conversation_id, parent_id,
            "a reply joins the root's own thread"
        );
        assert_eq!(
            row.language.as_deref(),
            Some("eo"),
            "an absent language falls back to the parent's tag"
        );
        assert_eq!(row.raw_text, "right back", "the text is not modified");
        assert_eq!(
            row.content, "<p>right back</p>",
            "no mention span is inserted into the render"
        );
        assert_eq!(
            mention_ids(pool, row.id).await,
            vec![bob.id],
            "the parent's author joins status_mentions alone"
        );

        let counted = parent_row(pool, parent_id).await;
        assert_eq!(counted.replies_count, 1, "the parent counts the reply");

        let value = serde_json::to_value(&answer).expect("the entity serializes");
        assert_eq!(value["in_reply_to_id"], parent_id.to_string());
        assert_eq!(value["in_reply_to_account_id"], bob.id.to_string());
        assert_eq!(value["pleroma"]["in_reply_to_account_acct"], "bob");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the auto-mention answers in the entity"
        );
    }

    /// A submitted language beats the parent's; a parent with no tag
    /// inherits nothing.
    #[tokio::test]
    async fn a_submitted_language_beats_the_parent_s() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "tagged").await;
        let bob = user(pool, "bob").await;
        let tagged = created(
            &app_state,
            &bob,
            CreateStatusParams {
                language: Some("eo".to_owned()),
                ..posted("saluton")
            },
        )
        .await;
        let tagged_id = Uuid::parse_str(&tagged.id).expect("an id");
        let mut body = replying("danke", tagged_id);
        body.language = Some("de".to_owned());
        let overridden = created(&app_state, &alice, body).await;
        assert_eq!(
            overridden.language.as_deref(),
            Some("de"),
            "the submitted tag wins over the parent's"
        );

        let plain = plain_post(pool, &bob, "no tag here").await;
        let rootless = created(&app_state, &alice, replying("nor this", plain.id)).await;
        assert_eq!(
            rootless.language, None,
            "a parent with no tag leaves the reply tagless"
        );
    }

    /// The parent's mentions ride along after its author, deduplicated
    /// against the reply's own text mentions — and a self-reply mentions
    /// nobody.
    #[tokio::test]
    async fn a_reply_inherits_the_parent_s_mentions_deduplicated() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "bob").await;
        let carol = user(pool, "carol").await;
        let dave = user(pool, "dave").await;

        let parent = created(&app_state, &bob, posted("@carol and @dave, hi")).await;
        let parent_id = Uuid::parse_str(&parent.id).expect("an id");
        let answer = created(&app_state, &alice, replying("sure @CAROL", parent_id)).await;
        let row = stored(pool, &answer).await;

        assert_eq!(
            mention_ids(pool, row.id).await,
            vec![carol.id, bob.id, dave.id],
            "the text's own mention leads, then the parent's author, then \
             the parent's mentions — with the account both sides name \
             linked once"
        );

        let mine = created(&app_state, &alice, posted("my own @dave")).await;
        let mine_id = Uuid::parse_str(&mine.id).expect("an id");
        let self_reply = created(&app_state, &alice, replying("to myself", mine_id)).await;
        let self_row = stored(pool, &self_reply).await;
        assert_eq!(
            mention_ids(pool, self_row.id).await,
            vec![dave.id],
            "the replier never joins their own auto-mentions, only the \
             accounts the parent mentioned that are not them"
        );
        assert_eq!(
            self_row.in_reply_to_account_id,
            Some(alice.id),
            "while the reply link itself still names the author"
        );
    }

    /// Twenty-five accounts on the parent's side is more than the cap: the
    /// author leads, the first nineteen follow in order, and the tail is
    /// simply not linked. A crowd member the reply text mentions itself
    /// stays — the body's own mentions do not count against the cap.
    #[tokio::test]
    async fn the_inherited_mentions_are_capped_at_twenty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "bob").await;
        let mut crowd: Vec<User> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for n in 1..=25_u32 {
            let nick = format!("member{n:02}");
            names.push(format!("@{nick}"));
            crowd.push(user(pool, &nick).await);
        }
        let crowd_ids: Vec<Uuid> = crowd.iter().map(|found| found.id).collect();
        let parent = created(&app_state, &bob, posted(&names.join(" "))).await;
        let parent_id = Uuid::parse_str(&parent.id).expect("an id");
        assert_eq!(
            mention_ids(pool, parent_id).await,
            crowd_ids,
            "the fixture parent really mentions all twenty-five"
        );

        let last = crowd_ids[24];
        let answer = created(
            &app_state,
            &alice,
            replying(&format!("noted @{}", crowd[24].nickname), parent_id),
        )
        .await;
        let row = stored(pool, &answer).await;

        let links = mention_ids(pool, row.id).await;
        let mut expected: Vec<Uuid> = vec![last, bob.id];
        expected.extend(crowd_ids[..MAX_INHERITED_MENTIONS - 1].iter().copied());
        assert_eq!(
            links, expected,
            "the body's own mention leads, then the capped inheritage — \
             author first, its nineteen first mentions in order"
        );
        for dropped in &crowd_ids[MAX_INHERITED_MENTIONS - 1..24] {
            assert!(
                !links.contains(dropped),
                "the parent's tail past the cap is not linked"
            );
        }
    }

    /// The service-level legs of the visibility inheritance rules the pure
    /// matrix pins: a submitted `public` coming down under a `private`
    /// parent, an unsubmitted default capped the same way, a `direct`
    /// parent forcing `direct`, and a requested `public` standing
    /// untouched under an `unlisted` parent.
    #[tokio::test]
    async fn a_reply_s_audience_follows_its_parent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "wide").await;
        let bob = user(pool, "bob").await;

        let private_parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("private".to_owned()),
                ..posted("hush")
            },
        )
        .await;
        let private_id = Uuid::parse_str(&private_parent.id).expect("an id");
        let mut requested = replying("reply", private_id);
        requested.visibility = Some("public".to_owned());
        let downgraded = created(&app_state, &bob, requested).await;
        assert_eq!(
            downgraded.visibility, "private",
            "a submitted public under a private parent comes down"
        );

        let unsubmitted = created(&app_state, &bob, replying("reply", private_id)).await;
        assert_eq!(
            unsubmitted.visibility, "private",
            "a public default under a private parent is capped to private"
        );

        let direct_parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("direct".to_owned()),
                ..posted("secret")
            },
        )
        .await;
        let direct_id = Uuid::parse_str(&direct_parent.id).expect("an id");
        let mut asked_wide = replying("reply", direct_id);
        asked_wide.visibility = Some("public".to_owned());
        let forced = created(&app_state, &bob, asked_wide).await;
        assert_eq!(
            forced.visibility, "direct",
            "a direct parent forces direct whatever was asked for"
        );

        let unlisted_parent = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("unlisted".to_owned()),
                ..posted("quiet")
            },
        )
        .await;
        let unlisted_id = Uuid::parse_str(&unlisted_parent.id).expect("an id");
        let mut kept_wide = replying("reply", unlisted_id);
        kept_wide.visibility = Some("public".to_owned());
        let standing = created(&app_state, &alice, kept_wide).await;
        assert_eq!(
            standing.visibility, "public",
            "an unlisted parent keeps a requested public as requested"
        );
    }

    /// A reply to a reblog row answers the boosted original: the ids, the
    /// thread and the counted reply all land on the original, not on the
    /// boost row the id named.
    #[tokio::test]
    async fn a_reply_to_a_reblog_answers_the_original() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let bob = user(pool, "original-author").await;
        let carol = user(pool, "booster").await;
        let original = created(&app_state, &bob, posted("the original")).await;
        let original_id = Uuid::parse_str(&original.id).expect("an id");
        let reblog = status::insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original_id),
                ..NewStatus::local(&carol, "")
            },
        )
        .await
        .expect("a reblog row inserts");

        let answer = created(&app_state, &alice, replying("reacting", reblog.id)).await;
        let row = stored(pool, &answer).await;

        assert_eq!(
            row.in_reply_to_id,
            Some(original_id),
            "the reply targets the reblogged original"
        );
        assert_eq!(
            row.in_reply_to_account_id,
            Some(bob.id),
            "and the original's author, not the booster"
        );
        assert_eq!(row.conversation_id, original_id, "the original's thread");
        assert_eq!(
            mention_ids(pool, row.id).await,
            vec![bob.id],
            "the auto-mention names the original's author"
        );

        let reblog_after = parent_row(pool, reblog.id).await;
        assert_eq!(
            reblog_after.replies_count, 0,
            "the boost row does not count the reply"
        );
        let original_after = parent_row(pool, original_id).await;
        assert_eq!(
            original_after.replies_count, 1,
            "the original gains the reply"
        );
    }

    /// Everything that cannot answer — malformed text, an id that names no
    /// row, a tombstone, a post the poster may not see, and a reblog whose
    /// original went away — is the one shared 404, and none of it writes.
    /// The empty string, Pleroma's own no-reply spelling, is not among
    /// them: it posts a root status.
    #[tokio::test]
    async fn an_unanswerable_parent_is_the_shared_not_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "stranger").await;
        let bob = user(pool, "bob").await;

        let gone = plain_post(pool, &bob, "doomed").await;
        let tombstone = status::soft_delete(pool, gone.id)
            .await
            .expect("the delete runs")
            .expect("the row was live");
        assert!(tombstone.deleted_at.is_some(), "the parent is a tombstone");
        let hidden = created(
            &app_state,
            &bob,
            CreateStatusParams {
                visibility: Some("private".to_owned()),
                ..posted("not for you")
            },
        )
        .await;
        let hidden_id = Uuid::parse_str(&hidden.id).expect("an id");
        let stranded_reblog = status::insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(gone.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .expect("a reblog of a tombstone's id inserts");

        let misses: Vec<CreateStatusParams> = vec![
            {
                let mut body = posted("hi");
                body.in_reply_to_id = Some("not-an-id".to_owned());
                body
            },
            replying("hi", Uuid::now_v7()),
            replying("hi", gone.id),
            replying("hi", hidden_id),
            replying("hi", stranded_reblog.id),
        ];
        let before = live_ids(pool).await;
        for params in misses {
            let err = create_status(&app_state, &alice, None, params)
                .await
                .expect_err("the parent above cannot be answered");
            missed(err).await;
        }
        assert_eq!(
            live_ids(pool).await,
            before,
            "not one refused reply wrote a row"
        );

        let root = created(
            &app_state,
            &alice,
            CreateStatusParams {
                in_reply_to_id: Some("  ".to_owned()),
                ..posted("a root after all")
            },
        )
        .await;
        let row = stored(pool, &root).await;
        assert!(
            row.in_reply_to_id.is_none() && row.in_reply_to_account_id.is_none(),
            "a blank parent parameter is no reply at all"
        );
        assert_eq!(
            row.conversation_id, row.id,
            "and the post is its own thread"
        );
        assert_eq!(
            live_ids(pool).await.len(),
            before.len() + 1,
            "and the blank is the only row the whole test wrote"
        );
    }

    /// Three levels deep: every answer joins the root's conversation and
    /// steps the parent it answers, whatever else the thread holds.
    #[tokio::test]
    async fn a_thread_shares_one_conversation_across_three_levels() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "first").await;
        let bob = user(pool, "bob").await;
        let carol = user(pool, "carol").await;

        let root = created(&app_state, &alice, posted("root")).await;
        let root_id = Uuid::parse_str(&root.id).expect("an id");
        let second = created(&app_state, &bob, replying("second", root_id)).await;
        let second_id = Uuid::parse_str(&second.id).expect("an id");
        let third = created(&app_state, &carol, replying("third", second_id)).await;

        let second_row = stored(pool, &second).await;
        let third_row = stored(pool, &third).await;
        assert_eq!(second_row.conversation_id, root_id, "the first reply");
        assert_eq!(third_row.conversation_id, root_id, "the second reply");

        let root_row = parent_row(pool, root_id).await;
        assert_eq!(
            root_row.replies_count, 1,
            "the root counts its direct reply"
        );
        let second_after = parent_row(pool, second_id).await;
        assert_eq!(
            second_after.replies_count, 1,
            "the middle row counts the one under it"
        );
        assert_eq!(
            third_row.in_reply_to_id,
            Some(second_id),
            "and each level answers the row directly above it"
        );
    }
}
