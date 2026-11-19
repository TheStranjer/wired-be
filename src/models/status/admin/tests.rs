//! The admin API's tests: which rows each filter keeps, the window the
//! page picks, and the scope write's two fields.

use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus},
        user::{self, NewUser, User},
    },
};

/// The window every filter test reads: the whole first page with every
/// arm open, so the assertions are about the filters and nothing else.
fn open() -> ListParams {
    ListParams {
        page: 1,
        page_size: 100,
        godmode: Flag::new(true),
        local_only: Flag::new(false),
        with_reblogs: Flag::new(true),
        user_id: None,
        domain: None,
    }
}

/// A local account named `nickname`.
async fn author(pool: &PgPool, nickname: &str) -> User {
    let ap_id = user::local_ap_id(TEST_BASE_URL, nickname);
    user::insert(pool, &NewUser::local(nickname, &ap_id))
        .await
        .expect("a local author inserts")
}

/// `poster`'s public status, so a test can name its id.
async fn post(pool: &PgPool, poster: &User, text: &str, visibility: Visibility) -> Status {
    let new = NewStatus {
        visibility,
        ..NewStatus::local(poster, text)
    };
    status::insert(pool, TEST_BASE_URL, &new)
        .await
        .expect("a local status inserts")
}

/// A mirror of a remote post: the row an inbound `Create` would store,
/// authored by `remote` and owned by nobody here.
async fn mirror(pool: &PgPool, remote: &User, slug: &str) -> Status {
    let new = NewStatus {
        uri: Some(format!("https://remote.example/objects/{slug}")),
        user_id: remote.id,
        content: format!("a remote post {slug}"),
        raw_text: format!("a remote post {slug}"),
        ..NewStatus::default()
    };
    status::insert(pool, TEST_BASE_URL, &new)
        .await
        .expect("a mirrored status inserts")
}

/// The account this instance does not own, with its host.
async fn remote_author(pool: &PgPool, nickname: &str, host: &str) -> User {
    user::insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id: format!("https://{host}/users/{nickname}"),
            domain: Some(host.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("a remote mirror inserts")
}

/// The ids of `rows` in the order they arrived, for order assertions.
fn ids(rows: &[Status]) -> Vec<Uuid> {
    rows.iter().map(|row| row.id).collect()
}

#[tokio::test]
async fn godmode_decides_which_visibilities_are_listed() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let alice = author(pool, "alice").await;
    let public = post(pool, &alice, "public one", Visibility::Public).await;
    let unlisted = post(pool, &alice, "unlisted one", Visibility::Unlisted).await;
    let private = post(pool, &alice, "private one", Visibility::Private).await;
    let direct = post(pool, &alice, "direct one", Visibility::Direct).await;
    let local = post(pool, &alice, "local one", Visibility::Local).await;

    let every = list(pool, &open()).await.expect("the ledger reads");
    assert_eq!(
        ids(&every),
        vec![local.id, direct.id, private.id, unlisted.id, public.id],
        "godmode sees every visibility, newest id first, which is the \
             UUIDv7 creation order"
    );
    assert_eq!(
        count(pool, &open()).await.expect("the ledger counts"),
        5,
        "and the total is the same set"
    );

    let quiet = list(
        pool,
        &ListParams {
            godmode: Flag::new(false),
            ..open()
        },
    )
    .await
    .expect("the ledger reads");
    assert_eq!(
        ids(&quiet),
        vec![unlisted.id, public.id],
        "without godmode only the two public visibilities are listed"
    );
    assert_eq!(
        count(
            pool,
            &ListParams {
                godmode: Flag::new(false),
                ..open()
            }
        )
        .await
        .expect("the ledger counts"),
        2,
        "and the total weighs the same filter the page does"
    );
}

#[tokio::test]
async fn reblogs_local_authors_and_domains_narrow_the_ledger() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let alice = author(pool, "alice").await;
    let root = post(pool, &alice, "a public post", Visibility::Public).await;
    let boost = status::create_reblog(pool, TEST_BASE_URL, &alice, &root, Visibility::Public)
        .await
        .expect("a reblog writes")
        .0;
    let stranger = remote_author(pool, "bob", "remote.example").await;
    let mirrored = mirror(pool, &stranger, "one").await;

    let all = list(pool, &open()).await.expect("the ledger reads");
    assert_eq!(
        ids(&all),
        vec![mirrored.id, boost.id, root.id],
        "every live row is listed, whoever authored it"
    );

    let posts = list(
        pool,
        &ListParams {
            with_reblogs: Flag::new(false),
            ..open()
        },
    )
    .await
    .expect("the ledger reads");
    assert_eq!(
        ids(&posts),
        vec![mirrored.id, root.id],
        "the boost row is Pleroma's excluded reblog"
    );

    let own = list(
        pool,
        &ListParams {
            local_only: Flag::new(true),
            ..open()
        },
    )
    .await
    .expect("the ledger reads");
    assert_eq!(
        ids(&own),
        vec![boost.id, root.id],
        "local_only is the local authors' set, boost included"
    );

    let hosts = list(
        pool,
        &ListParams {
            domain: Some("remote.example".to_owned()),
            ..open()
        },
    )
    .await
    .expect("the ledger reads");
    assert_eq!(
        ids(&hosts),
        vec![mirrored.id],
        "the domain filter is the author's stored host"
    );
}

#[tokio::test]
async fn a_domain_nobody_came_from_selects_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let alice = author(pool, "alice").await;
    let kept = post(pool, &alice, "a local post", Visibility::Public).await;
    assert!(
        ids(&list(pool, &open()).await.expect("the ledger reads")).contains(&kept.id),
        "the fixture post is on the open ledger, so the empty answer \
             below is the filter's doing and not an empty table"
    );

    let absent = ListParams {
        domain: Some("absent.example".to_owned()),
        ..open()
    };
    assert!(
        list(pool, &absent)
            .await
            .expect("the ledger reads")
            .is_empty(),
        "an unknown domain lists nothing"
    );
    assert_eq!(
        count(pool, &absent).await.expect("the ledger counts"),
        0,
        "and its total says so too"
    );
}

#[tokio::test]
async fn one_author_and_one_window_of_the_ledger() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let alice = author(pool, "alice").await;
    let bob = author(pool, "bob").await;
    let mut alices = Vec::new();
    for n in 0..3 {
        alices.push(post(pool, &alice, &format!("alice {n}"), Visibility::Public).await);
    }
    let bobs = post(pool, &bob, "bob's one", Visibility::Public).await;

    let scoped = ListParams {
        user_id: Some(alice.id),
        ..open()
    };
    let page = list(pool, &scoped)
        .await
        .expect("the author's ledger reads");
    assert_eq!(
        ids(&page),
        vec![alices[2].id, alices[1].id, alices[0].id],
        "the author filter is that account's whole ledger, newest first"
    );
    assert_eq!(
        count(pool, &scoped)
            .await
            .expect("the author's ledger counts"),
        3,
        "and its total is that account's count"
    );
    assert!(
        ids(&list(pool, &open()).await.expect("the ledger reads")).contains(&bobs.id),
        "the unscoped ledger still holds the other account's post"
    );

    let tail = list(
        pool,
        &ListParams {
            page: 2,
            page_size: 2,
            ..scoped
        },
    )
    .await
    .expect("the second page reads");
    assert_eq!(
        ids(&tail),
        vec![alices[0].id],
        "page 2 of a two-row window is what the first window left"
    );
}

#[tokio::test]
async fn set_scope_moves_the_two_fields_and_nothing_else() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let alice = author(pool, "alice").await;
    let row = post(pool, &alice, "text", Visibility::Public).await;

    let both = set_scope(pool, row.id, Some(true), Some(Visibility::Private))
        .await
        .expect("the write runs")
        .expect("the live row answers");
    assert!(both.sensitive, "the sensitive flag moved");
    assert_eq!(both.visibility, Visibility::Private, "and so did it");
    assert_eq!(both.content, row.content, "the text is where it was");
    assert_eq!(
        both.edited_at, row.edited_at,
        "a scope change is no content edit: no revision stamp"
    );

    let untouched = set_scope(pool, row.id, None, None)
        .await
        .expect("the write runs")
        .expect("the live row answers");
    assert!(untouched.sensitive, "an absent sensitive keeps the value");
    assert_eq!(
        untouched.visibility,
        Visibility::Private,
        "an absent visibility keeps the value"
    );

    let folded = set_scope(pool, row.id, Some(false), None)
        .await
        .expect("the write runs")
        .expect("the live row answers");
    assert!(
        !folded.sensitive,
        "one field moves while the other holds its ground"
    );
    assert_eq!(
        folded.visibility,
        Visibility::Private,
        "the visibility the first write moved still stands"
    );
}

#[tokio::test]
async fn set_scope_answers_nothing_for_a_row_that_is_not_there() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let alice = author(pool, "alice").await;
    let row = post(pool, &alice, "text", Visibility::Public).await;

    assert!(
        set_scope(pool, row.id, Some(true), None)
            .await
            .expect("the write runs")
            .is_some(),
        "the live row answers with its own new state"
    );
    let deleted: Option<Status> = status::soft_delete(pool, row.id)
        .await
        .expect("the fixture delete runs");
    assert!(deleted.is_some(), "the fixture deleted a live row");
    assert!(
        set_scope(pool, row.id, Some(true), None)
            .await
            .expect("the write runs")
            .is_none(),
        "a tombstone takes no scope change"
    );
    assert!(
        set_scope(pool, Uuid::now_v7(), Some(true), None)
            .await
            .expect("the write runs")
            .is_none(),
        "an id that names no row answers nothing"
    );
}
