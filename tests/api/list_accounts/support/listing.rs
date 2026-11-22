//! Reading a list's members: the empty list, the order and the page size.

use super::*;

/// A list with nobody on it answers the empty array, with no `Link` header:
/// there is no membership to key a page on.
#[tokio::test]
async fn an_empty_list_answers_the_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool);

    let response = members(&app, &scene.token, scene.list.id, "").await;
    assert_eq!(response.status, StatusCode::OK, "200 for its owner");
    assert_eq!(response.text(), "[]", "no members, the empty array");
    assert_eq!(response.header("link"), None, "and nothing to link to");
}

/// The page walk: newest membership first, `max_id` older, `min_id` newer,
/// `since_id` forward, the `Link` header keyed on the position ordinals of the
/// page it describes, and `limit=0` every member of the list.
#[tokio::test]
async fn members_are_answered_newest_membership_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool.clone());
    added(&app, &scene.token, scene.list.id, &scene.ids()).await;

    let every = listed(&app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(
        accts(&every),
        ["dave", "carol", "bob"],
        "the batch added bob, carol, dave, so dave's membership is the newest \
         and `limit=0` answers every member"
    );
    assert_eq!(
        ids(&every),
        [
            scene.dave.id.to_string(),
            scene.carol.id.to_string(),
            scene.bob.id.to_string()
        ],
        "the entities carry the members' own ids"
    );
    assert_account_keys(every.first().expect("three members are on the page"));
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "").await.len(),
        3,
        "the default page of 20 holds all three"
    );

    let page = members(&app, &scene.token, scene.list.id, "limit=2").await;
    assert_eq!(page.status, StatusCode::OK, "a page of two reads");
    assert_eq!(
        accts(&page.json::<Vec<Value>>()),
        ["dave", "carol"],
        "the two newest memberships"
    );
    let path = members_path(scene.list.id);
    let link = page
        .header("link")
        .expect("a page that leaves a member out links to the rest")
        .to_owned();
    let next = cursor(&link, "next", "max_id");
    let newest = cursor(&link, "prev", "min_id");
    assert_members_link(&link, &path, &next, &newest);

    assert_cursor_walk(&app, &scene.token, scene.list.id, &next).await;
    let empty = [
        members(
            &app,
            &scene.token,
            scene.list.id,
            &format!("since_id={newest}"),
        )
        .await,
        members(&app, &scene.token, scene.list.id, "limit=2&max_id=0").await,
    ];
    for past_end in &empty {
        common::assert_answer(
            past_end,
            StatusCode::OK,
            "a page past the newest membership",
        );
        assert_eq!(past_end.text(), "[]", "is the empty array");
    }
    assert_eq!(
        empty[0].header("link"),
        None,
        "and an empty page sends no Link header at all"
    );

    let second_list = seed(&pool, &scene.alice, "Work").await;
    assert!(
        listed(&app, &scene.token, second_list.id, "limit=0")
            .await
            .is_empty(),
        "memberships are per list: her other list holds nobody"
    );
}

/// The default page is 20 members, an explicit `limit` above the cap falls to
/// the cap, and `limit=0` answers all of them.
#[tokio::test]
async fn the_default_page_is_twenty_members() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    assert_eq!(
        CAP,
        usize::try_from(MAX_LIMIT).expect("the cap is a page size"),
        "the cap this test pins is the pagination layer's own"
    );
    let scene = Scene::new(&pool).await;
    let mut members = vec![scene.bob.id, scene.carol.id, scene.dave.id];
    for index in 0..42 {
        let followed = user(&pool, &format!("friend{index}")).await;
        follow_user(&pool, &scene.alice, &followed).await;
        members.push(followed.id);
    }
    let app = common::test_app(pool);
    added(&app, &scene.token, scene.list.id, &members).await;

    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "limit=0")
            .await
            .len(),
        members.len(),
        "45 members, and `limit=0` answers every one of them"
    );
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "").await.len(),
        20,
        "the default page is 20 of them"
    );
    assert_eq!(
        listed(&app, &scene.token, scene.list.id, "limit=100")
            .await
            .len(),
        CAP,
        "and an over-cap request falls to the cap"
    );
}
