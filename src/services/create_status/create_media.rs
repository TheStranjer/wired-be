//! The create path's attachment tests: uploads claimed in the submitted
//! order, the write's `Media not found` gate and the attachments every
//! visibility may carry — the media half of [`create_status`].
//!
//! [`create_status`]: crate::services::create_status::create_status

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        models::media_attachment::{self, NewMediaAttachment},
        services::create_status::{
            BAD_MEDIA_ID, CreateStatusParams, create_status,
            test_support::{
                BASE, attachment_ids, created, live_ids, media_row, posted, refused, state, stored,
                upload, user, wire, with_media,
            },
        },
    };
    #[tokio::test]
    async fn a_media_only_post_stores_the_row_and_claims_the_uploads_in_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;
        let first_upload = upload(pool, &alice).await;
        let second_upload = upload(pool, &alice).await;

        // Posted newest first: the order the client asks for, not the order
        // the rows were minted in.
        let entity = created(
            &app_state,
            &alice,
            with_media(
                "",
                &[&second_upload.id.to_string(), &first_upload.id.to_string()],
            ),
        )
        .await;
        let row = stored(pool, &entity).await;
        assert_eq!(row.raw_text, "", "the post says nothing");
        assert_eq!(
            attachment_ids(&entity),
            vec![second_upload.id.to_string(), first_upload.id.to_string()],
            "the entity answers the attachments in the requested order"
        );

        let second = media_row(pool, second_upload.id).await;
        assert_eq!(
            second.status_id,
            Some(row.id),
            "the newest upload went first"
        );
        assert_eq!(second.position, 0, "at position zero");
        let first = media_row(pool, first_upload.id).await;
        assert_eq!(first.status_id, Some(row.id), "and the oldest follows it");
        assert_eq!(first.position, 1, "at the position the list gave it");
    }

    #[tokio::test]
    async fn media_the_poster_cannot_claim_refuses_the_whole_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let foreign = upload(pool, &bob).await;
        let first_post = created(&app_state, &alice, posted("mine already stands")).await;
        let first_id = Uuid::parse_str(&first_post.id).expect("an id");
        let claimed = media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(alice.id),
                status_id: Some(first_id),
                url: format!("{BASE}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("a fixture attachment inserts")
        .id;

        for (what, id) in [
            ("somebody else's upload", foreign.id),
            ("media a status already claims", claimed),
            ("an id no row holds", Uuid::now_v7()),
        ] {
            let mine = upload(pool, &alice).await;
            let body = with_media("hi", &[&mine.id.to_string(), &id.to_string()]);
            let err = create_status(&app_state, &alice, None, body)
                .await
                .expect_err("the unclaimable id above refuses the post");
            refused(&wire(err).await, BAD_MEDIA_ID);
            let mut live = live_ids(pool).await;
            assert_eq!(live.len(), 1, "{what}: the refused post wrote no row");
            assert_eq!(live.pop().expect("the one live id"), first_id);
            assert_eq!(
                media_row(pool, mine.id).await.status_id,
                None,
                "{what}: and the claimable upload rode the rollback out with it"
            );
        }
    }

    #[tokio::test]
    async fn every_visibility_can_carry_attachments() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;

        for name in ["public", "unlisted", "private", "direct", "local"] {
            let mine = upload(pool, &alice).await;
            let entity = create_status(
                &app_state,
                &alice,
                None,
                CreateStatusParams {
                    visibility: Some(name.to_owned()),
                    ..with_media("a picture with words", &[&mine.id.to_string()])
                },
            )
            .await
            .unwrap_or_else(|err| panic!("`{name}` posts with media: {err}"));
            assert_eq!(
                entity.visibility, name,
                "`{name}` stands as asked, attachments and all"
            );
            assert_eq!(
                attachment_ids(&entity),
                vec![mine.id.to_string()],
                "`{name}` answers its attachment"
            );
            assert_eq!(
                media_row(pool, mine.id).await.status_id,
                Uuid::parse_str(&entity.id).ok(),
                "`{name}` claimed the row"
            );
        }
    }
}
