//! The archive builder's tests: the four entries in order with the actor
//! verbatim and the temp file the value owns, the outbox's visibility
//! rules, the likes' and bookmarks' row order, the keyset pages crossing
//! their seam into one entry, and the empty account's three empty
//! collections.

use std::{collections::HashSet, io::Read as _};

use chrono::Utc;
use serde_json::Value;
use zip::ZipArchive;

use super::*;
use crate::{
    jobs::backups::test_support::{like, mark, post, user},
    models::{
        backup::file_name,
        status::{Visibility, soft_delete},
    },
    test_support::app_state,
};

/// How many statuses the page-seam test seeds: just over one page.
const OVER_PAGE: usize = PAGE_SIZE + 5;

/// Builds `owner`'s archive under the name
/// [`crate::models::backup::file_name`] mints for them right now.
async fn build_for(state: &AppState, owner: &User) -> BackupArchive {
    let name = file_name(&owner.nickname, Utc::now());
    let archive = build(state, owner, &name)
        .await
        .expect("the archive builds");
    assert_eq!(
        archive.file_name(),
        name,
        "the value echoes the name it was handed"
    );
    archive
}

/// The raw text of entry `name`, read back through `ZipArchive`.
fn entry_text(archive: &BackupArchive, name: &str) -> String {
    let file = File::open(archive.temp.path()).expect("the temp file exists");
    let mut zip = ZipArchive::new(file).expect("the archive reads back");
    let mut entry = zip.by_name(name).expect("the entry is there");
    let mut text = String::new();
    let _read: usize = entry.read_to_string(&mut text).expect("the entry decodes");
    text
}

/// The parsed JSON of entry `name`.
fn entry_json(archive: &BackupArchive, name: &str) -> Value {
    serde_json::from_str(&entry_text(archive, name))
        .unwrap_or_else(|err| panic!("{name} is not valid JSON: {err}"))
}

/// The entry names, in archive order.
fn entry_names(archive: &BackupArchive) -> Vec<String> {
    let file = File::open(archive.temp.path()).expect("the temp file exists");
    let zip = ZipArchive::new(file).expect("the archive reads back");
    zip.file_names()
        .map(|name| name.expect("entry names read").into_owned())
        .collect()
}

/// The `orderedItems` of one parsed collection entry, after asserting
/// the shared header and that `totalItems` counts its items.
fn collection(archive: &BackupArchive, name: &str) -> Vec<Value> {
    let doc = entry_json(archive, name);
    assert_eq!(
        doc.get("@context").and_then(Value::as_str),
        Some(AS_VOCABULARY),
        "{name} declares the plain ActivityStreams context"
    );
    assert_eq!(
        doc.get("id").and_then(Value::as_str),
        Some(name),
        "{name} is its own name's id"
    );
    assert_eq!(
        doc.get("type").and_then(Value::as_str),
        Some("OrderedCollection"),
        "{name} is an OrderedCollection"
    );
    let items = doc
        .get("orderedItems")
        .and_then(Value::as_array)
        .expect("the entry carries an orderedItems array");
    assert_eq!(
        doc.get("totalItems").and_then(Value::as_u64),
        Some(u64::try_from(items.len()).expect("an archive fits its item count into a u64")),
        "{name}'s totalItems matches its item count"
    );
    items.clone()
}

/// The `object.id` of every item, asserting each is a `Create`
/// carrying an embedded object.
fn create_object_ids(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            assert_eq!(
                item.get("type").and_then(Value::as_str),
                Some("Create"),
                "every outbox item is a Create"
            );
            item.get("object")
                .and_then(|object| object.get("id"))
                .and_then(Value::as_str)
                .expect("the Create embeds an object with an id")
                .to_owned()
        })
        .collect()
}

/// The uri strings of a parsed likes/bookmarks entry.
fn uri_items(items: Vec<Value>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| {
            item.as_str()
                .expect("a like/bookmark item is a uri string")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn the_archive_holds_the_four_entries_and_owns_its_temp_file() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let _first: Status = post(&pool, &alice, Visibility::Public, "hello").await;

    let archive = build_for(&state, &alice).await;
    assert_eq!(
        entry_names(&archive),
        [ACTOR_ENTRY, OUTBOX_ENTRY, LIKES_ENTRY, BOOKMARKS_ENTRY],
        "the four entries stand in this order"
    );
    for name in [ACTOR_ENTRY, OUTBOX_ENTRY, LIKES_ENTRY, BOOKMARKS_ENTRY] {
        assert!(
            entry_json(&archive, name).is_object(),
            "{name} parses as a JSON object"
        );
    }
    assert_eq!(
        entry_json(&archive, ACTOR_ENTRY),
        actor_json(&alice, &state.config, &[]),
        "actor.json is the federation actor verbatim"
    );

    // The size says what the file holds while the value owns it...
    let path = archive.temp.path().to_path_buf();
    assert_eq!(
        archive.size(),
        std::fs::metadata(&path)
            .expect("the temp file is there")
            .len(),
        "size equals the archive file's length"
    );
    // ...and the temp file cannot outlive the value.
    drop(archive);
    assert!(!path.exists(), "dropping the archive deleted its file");
}

#[tokio::test]
async fn outbox_lists_every_federated_status_once_as_a_create() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let public = post(&pool, &alice, Visibility::Public, "public").await;
    let unlisted = post(&pool, &alice, Visibility::Unlisted, "unlisted").await;
    let private = post(&pool, &alice, Visibility::Private, "followers").await;
    let direct = post(&pool, &alice, Visibility::Direct, "psst").await;
    let local = post(&pool, &alice, Visibility::Local, "instance only").await;
    let gone = post(&pool, &alice, Visibility::Public, "deleted").await;
    assert!(
        soft_delete(&pool, gone.id)
            .await
            .expect("the delete runs")
            .is_some(),
        "the fixture tombstone lands"
    );
    // Another account's post must never ride along.
    let _theirs: Status = post(&pool, &bob, Visibility::Public, "not yours").await;

    let archive = build_for(&state, &alice).await;
    let items = collection(&archive, OUTBOX_ENTRY);
    let got: HashSet<String> = create_object_ids(&items).into_iter().collect();
    let want: HashSet<String> = [
        public.uri.as_str(),
        unlisted.uri.as_str(),
        private.uri.as_str(),
        direct.uri.as_str(),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        got, want,
        "all four visibilities, private and direct included"
    );
    assert_eq!(items.len(), want.len(), "each status exactly once");
    assert!(
        !got.contains(&local.uri),
        "a local post has no document to export"
    );
    assert!(!got.contains(&gone.uri), "a soft-deleted status stays out");
}

#[tokio::test]
async fn likes_and_bookmarks_list_the_marked_uris_in_row_order() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let one = post(&pool, &alice, Visibility::Public, "one").await;
    let two = post(&pool, &alice, Visibility::Public, "two").await;
    let three = post(&pool, &alice, Visibility::Public, "three").await;
    like(&pool, &alice, two.id).await;
    like(&pool, &alice, one.id).await;
    mark(&pool, &alice, three.id).await;

    let archive = build_for(&state, &alice).await;
    assert_eq!(
        uri_items(collection(&archive, LIKES_ENTRY)),
        [two.uri.as_str(), one.uri.as_str()],
        "likes are exactly the favourites' uris, in favourite id order"
    );
    assert_eq!(
        uri_items(collection(&archive, BOOKMARKS_ENTRY)),
        [three.uri.as_str()],
        "bookmarks are exactly the bookmarks' uris, in bookmark id order"
    );
}

#[tokio::test]
async fn a_whole_page_beyond_the_seam_still_reaches_one_entry() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "paged").await;
    let mut uris = Vec::with_capacity(OVER_PAGE);
    let mut ids = Vec::with_capacity(OVER_PAGE);
    for n in 0..OVER_PAGE {
        let row = post(&pool, &alice, Visibility::Public, &format!("post {n}")).await;
        uris.push(row.uri);
        ids.push(row.id);
    }
    for id in &ids {
        like(&pool, &alice, *id).await;
        mark(&pool, &alice, *id).await;
    }

    let archive = build_for(&state, &alice).await;
    let items = collection(&archive, OUTBOX_ENTRY);
    assert_eq!(items.len(), OVER_PAGE, "every page reached the entry");
    let got: HashSet<String> = create_object_ids(&items).into_iter().collect();
    assert_eq!(
        got,
        uris.into_iter().collect::<HashSet<String>>(),
        "no status lost and none doubled across the page seam"
    );
    let likes: HashSet<String> = uri_items(collection(&archive, LIKES_ENTRY))
        .into_iter()
        .collect();
    assert_eq!(likes, got, "the likes crossed their seam too");
    let marks: HashSet<String> = uri_items(collection(&archive, BOOKMARKS_ENTRY))
        .into_iter()
        .collect();
    assert_eq!(marks, got, "and the bookmarks");
}

#[tokio::test]
async fn an_empty_account_yields_three_empty_collections() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let nobody = user(&pool, "nobody").await;

    let archive = build_for(&state, &nobody).await;
    for name in [OUTBOX_ENTRY, LIKES_ENTRY, BOOKMARKS_ENTRY] {
        assert!(
            entry_text(&archive, name).ends_with(r#""orderedItems": [],"totalItems": 0}"#),
            "{name} states the empty collection verbatim"
        );
        assert!(
            collection(&archive, name).is_empty(),
            "{name} parses as an empty collection"
        );
    }
}
