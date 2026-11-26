//! The thread collections' tests: the page links and the database-backed
//! documents.

use super::*;
use crate::models::{
    status::{NewStatus, Visibility},
    user::{NewUser, User, insert},
};

/// The collection address the link tests use.
const ROOT: &str = "https://test.example/objects/root/replies";
/// A newer cursor id.
const NEWEST: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// An older cursor id.
const OLDER: &str = "01934567-89ab-7cde-f012-4456789abcde";

/// A cursor id from its text.
fn id(text: &str) -> Uuid {
    Uuid::parse_str(text).expect("the fixture cursor parses")
}

#[test]
fn ascending_pages_walk_newer_and_back_to_older() {
    let links = ascending_links(ROOT, Some("page=true"), "", &[id(OLDER), id(NEWEST)], 2);
    assert_eq!(
        links.next.as_deref(),
        Some(format!("{ROOT}?page=true&min_id={NEWEST}").as_str()),
        "a full oldest-first page offers the newer page keyed on its newest id"
    );
    assert_eq!(
        links.prev.as_deref(),
        Some(format!("{ROOT}?page=true&max_id={OLDER}").as_str()),
        "and a page back toward the oldest end keyed on its oldest"
    );
    assert_eq!(links.id, format!("{ROOT}?page=true"), "the ask echoes back");
    assert_eq!(links.part_of, ROOT, "the root");
}

#[test]
fn a_short_page_ends_the_walk_and_an_empty_one_has_no_links() {
    let short = ascending_links(ROOT, Some("page=true"), "", &[id(OLDER)], 2);
    assert_eq!(short.next, None, "a short page is the last");
    assert!(short.prev.is_some(), "but it still points back");
    let empty = ascending_links(ROOT, Some("page=true"), "", &[], 2);
    assert!(
        empty.next.is_none() && empty.prev.is_none(),
        "no items, no links"
    );
}

#[test]
fn the_narrowed_walk_keeps_its_flag_on_every_link() {
    let links = ascending_links(
        ROOT,
        Some("only_other_accounts=true&page=true"),
        OTHER_ACCOUNTS_PREFIX,
        &[id(OLDER), id(NEWEST)],
        2,
    );
    assert_eq!(
        links.next.as_deref(),
        Some(format!("{ROOT}?only_other_accounts=true&page=true&min_id={NEWEST}").as_str()),
        "the next page stays narrowed"
    );
    assert_eq!(
        links.prev.as_deref(),
        Some(format!("{ROOT}?only_other_accounts=true&page=true&max_id={OLDER}").as_str()),
        "and so does the way back"
    );
}

/// A local user named `nickname`.
async fn account(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
    )
    .await
    .expect("the fixture user inserts")
}

/// `author`'s status of `visibility`, answering `parent` when given.
async fn written(
    pool: &PgPool,
    author: &User,
    parent: Option<&Status>,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        "https://test.example",
        &NewStatus {
            in_reply_to_id: parent.map(|row| row.id),
            visibility,
            ..NewStatus::local(author, "text")
        },
    )
    .await
    .expect("the fixture status inserts")
}

#[tokio::test]
async fn the_other_accounts_page_leaves_out_the_authors_own_answers() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let config = Config::for_tests();
    let alice = account(pool, "alice").await;
    let bob = account(pool, "bob").await;
    let root = written(pool, &alice, None, Visibility::Public).await;
    let own = written(pool, &alice, Some(&root), Visibility::Public).await;
    let theirs = written(pool, &bob, Some(&root), Visibility::Public).await;

    let everyone = replies_document(pool, &config, &root, Some("page=true"))
        .await
        .expect("the unfiltered page renders");
    assert_eq!(
        everyone["items"],
        json!([own.uri, theirs.uri]),
        "a plain page lists every public answer"
    );
    let query = "only_other_accounts=true&page=true";
    let others = replies_document(pool, &config, &root, Some(query))
        .await
        .expect("the narrowed page renders");
    assert_eq!(
        others["items"],
        json!([theirs.uri]),
        "the narrowed page lists the other accounts' answers only"
    );
    assert_eq!(
        others["id"],
        format!("{}/replies?{query}", root.uri),
        "the id echoes the request"
    );
    assert_eq!(
        others["prev"],
        format!(
            "{}/replies?only_other_accounts=true&page=true&max_id={}",
            root.uri, theirs.id
        ),
        "the way back keeps the narrowing"
    );
}

#[tokio::test]
async fn a_context_of_only_private_posts_is_not_found() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let config = Config::for_tests();
    let alice = account(pool, "alice").await;
    let secret = written(pool, &alice, None, Visibility::Direct).await;

    let outcome = context_document(pool, &config, secret.conversation_id, None).await;
    assert!(
        matches!(outcome, Err(AppError::RouteNotFound)),
        "nothing public, nothing to list"
    );
    let unknown = context_document(pool, &config, Uuid::nil(), Some("page=true")).await;
    assert!(
        matches!(unknown, Err(AppError::RouteNotFound)),
        "an unknown conversation answers the same"
    );
}

#[tokio::test]
async fn a_context_lists_the_threads_public_objects() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let config = Config::for_tests();
    let alice = account(pool, "alice").await;
    let root = written(pool, &alice, None, Visibility::Public).await;
    let answer = written(pool, &alice, Some(&root), Visibility::Unlisted).await;
    let _dm = written(pool, &alice, Some(&answer), Visibility::Direct).await;

    let collection = context_document(pool, &config, root.conversation_id, None)
        .await
        .expect("the root renders");
    assert_eq!(collection["type"], "OrderedCollection", "FEP-7888's shape");
    assert_eq!(
        collection["id"],
        context_id(&config, root.conversation_id),
        "the id is the context the notes name"
    );
    assert_eq!(collection["totalItems"], 2, "the public objects");
    let page = context_document(pool, &config, root.conversation_id, Some("page=true"))
        .await
        .expect("the page renders");
    assert_eq!(page["type"], "OrderedCollectionPage", "a page of it");
    assert_eq!(
        page["orderedItems"],
        json!([root.uri, answer.uri]),
        "oldest first, the direct answer stays out"
    );
    assert_eq!(page["partOf"], collection["id"], "the root it belongs to");
}
