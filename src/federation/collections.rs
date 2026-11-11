//! The `ActivityPub` actor collections: the documents served beside the
//! actor itself — its outbox, its followers and following lists, its
//! featured (pinned) notes, and the replies to one of its objects.
//!
//! The reference is Pleroma's `ActivityPubController` actions `outbox`,
//! `followers`, `following` and `pinned`
//! (`lib/pleroma/web/activity_pub/activity_pub_controller.ex`) rendering
//! through `UserView` (`lib/pleroma/web/activity_pub/views/user_view.ex`)
//! and `CollectionViewHelper`
//! (`lib/pleroma/web/activity_pub/views/collection_view_helper.ex`), and
//! the object action `object_replies` rendering through `ObjectView`. Two
//! shapes come out of every one of these routes:
//!
//! - the **root**: `{id, type, totalItems?, first}`, where `first` is the
//!   collection's own address with `?page=true` appended — outbox and the
//!   follow lists are `OrderedCollection`s, the replies collection a
//!   `Collection` — and
//! - one **page** of items: `{id, type, partOf, next?, prev?, items}`, with
//!   the keyset cursors of decision 4 spelled out in the URLs. The
//!   collection's own address echoes back as the page's `id`, Pleroma's
//!   `current_url`, and `partOf` names the root.
//!
//! The ticket fixes the page sizes — 20 outbox activities, 10 follow
//! entries, 20 replies — and the visibility rules: an outbox page lists
//! the account's `public` and `unlisted` statuses as the `Create`s (and,
//! for reblogs, `Announce`s) they federate as, a replies page lists the
//! publicly visible answers' object ids, and a private or direct row
//! appears in neither. The follow lists answer empty when the account
//! hides them, and hide their totals when it hides the counts.
//!
//! Like [`super::note`], this module keeps the document build separate
//! from the reads that feed it: the builders are pure over their
//! arguments, the `*_document` functions take a pool and read what the
//! route's request asks for.

use serde_json::{Value, json};
use sqlx::PgPool;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    federation::note,
    models::{
        follow::{self, FollowPageEntry},
        pinned_status,
        status::{self, Status},
        user::{User, get_by_id as user_by_id, outbox},
    },
    pagination::{MAX_LIMIT, PageParams},
};

/// The `type` of the outbox and the two follow lists: an ordered
/// collection, walked newest-first.
const ORDERED_COLLECTION: &str = "OrderedCollection";
/// The `type` of an object's replies: a plain (unordered by contract)
/// collection, oldest-first in practice.
const COLLECTION: &str = "Collection";
/// The `type` of one page of an ordered collection.
const ORDERED_COLLECTION_PAGE: &str = "OrderedCollectionPage";
/// The `type` of one replies page.
const COLLECTION_PAGE: &str = "CollectionPage";
/// Path appended to the actor id for the featured (pinned) collection.
///
/// It is the address [`super::actor::actor_json`] advertises and Pleroma's
/// `UserView` renders, distinct from [`crate::models::user::featured`]'s
/// column-default path.
pub const FEATURED_PATH: &str = "/collections/featured";
/// The value of the `page` parameter that asks for a page rather than the
/// collection root (Pleroma's outbox action matches `true` / `"true"`).
const PAGE_TRUE: &str = "true";
/// The keyset parameter that walks a page further from the newest end.
const MAX_ID: &str = "max_id";
/// The keyset parameter that walks a page back toward the newest end.
const MIN_ID: &str = "min_id";

/// How many activities one outbox page holds.
const OUTBOX_PAGE_SIZE: i64 = 20;
/// How many actor ids one followers/following page holds.
const FOLLOW_PAGE_SIZE: i64 = 10;
/// How many reply ids one replies page holds.
const REPLIES_PAGE_SIZE: i64 = 20;

/// Which side of the follow graph a route serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowSide {
    /// The accounts that follow the actor: `/followers`.
    Followers,
    /// The accounts the actor follows: `/following`.
    Following,
}

impl FollowSide {
    /// The path segment of this side's collection, under the actor id.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Followers => "followers",
            Self::Following => "following",
        }
    }

    /// The `totalItems` the root shows: the account's stored counter,
    /// `None` — no key at all — when the account hides this side's count
    /// (`hide_followers_count` / `hide_follows_count`).
    #[must_use]
    pub fn total_items(self, user: &User) -> Option<i64> {
        match self {
            Self::Followers if user.hide_followers_count.get() => None,
            Self::Followers => Some(i64::from(user.followers_count)),
            Self::Following if user.hide_follows_count.get() => None,
            Self::Following => Some(i64::from(user.following_count)),
        }
    }

    /// Whether this side's pages list nothing at all, because the account
    /// hides the list itself (`hide_followers` / `hide_follows`).
    #[must_use]
    pub const fn hidden(self, user: &User) -> bool {
        match self {
            Self::Followers => user.hide_followers.get(),
            Self::Following => user.hide_follows.get(),
        }
    }

    /// The accepted relationships of this side, newest relationship first,
    /// keyset-paged on the follow row id.
    async fn fetch(
        self,
        pool: &PgPool,
        user_id: Uuid,
        page: PageParams,
    ) -> AppResult<Vec<FollowPageEntry>> {
        match self {
            Self::Followers => follow::followers_page(pool, user_id, page).await,
            Self::Following => follow::following_page(pool, user_id, page).await,
        }
    }
}

/// The root document of a paged collection: its own address, the total
/// when the route has one to show, and the link to its first page.
#[must_use]
pub fn collection_root(
    id: &str,
    type_name: &'static str,
    total_items: Option<i64>,
    config: &Config,
) -> Value {
    let mut fields = vec![
        ("@context", json_ld_context(config)),
        ("id", json!(id)),
        ("type", json!(type_name)),
    ];
    if let Some(total) = total_items {
        fields.push(("totalItems", json!(total)));
    }
    fields.push(("first", json!(format!("{id}?page=true"))));
    json_object(fields)
}

/// The pagination envelope of one keyset page: the fields the page carries
/// beside its items, in Pleroma's `collection_page_keyset` shape.
#[derive(Debug)]
pub struct PageLinks {
    /// The page's own `id`: the address it was requested at, the
    /// collection with the request's query echoed back.
    pub id: String,
    /// The `partOf` of the root this page belongs to.
    pub part_of: String,
    /// The `next` page's URL, `None` when the walk ends here.
    pub next: Option<String>,
    /// The `prev` page's URL, `None` when there is nothing on that side.
    pub prev: Option<String>,
}

/// One `OrderedCollectionPage` of `user`'s outbox or follow list.
#[must_use]
pub fn ordered_collection_page(links: &PageLinks, items: Vec<Value>, config: &Config) -> Value {
    page_document(
        links,
        ORDERED_COLLECTION_PAGE,
        "orderedItems",
        items,
        config,
    )
}

/// One `CollectionPage` of an object's reply ids.
#[must_use]
pub fn collection_page(links: &PageLinks, items: Vec<Value>, config: &Config) -> Value {
    page_document(links, COLLECTION_PAGE, "items", items, config)
}

/// The featured (pinned) collection: the notes inline, the count beside
/// them — Pleroma renders this one in a single document, no paging.
#[must_use]
pub fn featured_collection(id: &str, objects: Vec<Value>, config: &Config) -> Value {
    let total = i64::try_from(objects.len()).unwrap_or(i64::MAX);
    json_object(vec![
        ("@context", json_ld_context(config)),
        ("id", json!(id)),
        ("type", json!(ORDERED_COLLECTION)),
        ("orderedItems", Value::Array(objects)),
        ("totalItems", json!(total)),
    ])
}

/// The shared body of the two page builders: the envelope plus the items
/// under the type's own key (`orderedItems` / `items`).
fn page_document(
    links: &PageLinks,
    type_name: &'static str,
    items_key: &'static str,
    items: Vec<Value>,
    config: &Config,
) -> Value {
    let mut fields = vec![
        ("@context", json_ld_context(config)),
        ("id", json!(&links.id)),
        ("type", json!(type_name)),
        ("partOf", json!(&links.part_of)),
    ];
    if let Some(next) = &links.next {
        fields.push(("next", json!(next)));
    }
    if let Some(prev) = &links.prev {
        fields.push(("prev", json!(prev)));
    }
    fields.push((items_key, Value::Array(items)));
    json_object(fields)
}

/// `(name, value)` pairs as a JSON object, in the order they are given —
/// the same assembly [`super::actor`] and [`super::note`] use.
fn json_object(fields: Vec<(&'static str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// The `@context` of every document in this module: the same
/// [`note`]-document header Pleroma's `make_json_ld_header` writes.
fn json_ld_context(config: &Config) -> Value {
    note::json_ld_context(config)
}

/// Whether `query` asks for a page of the collection rather than the root.
/// A repeated `page` counts by its last value, as a query-param map folds
/// duplicates; anything but `true` is the root.
#[must_use]
pub fn page_requested(query: Option<&str>) -> bool {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .filter(|(key, _value)| key == "page")
        .map(|(_key, value)| value.into_owned())
        .last()
        .is_some_and(|value| value == PAGE_TRUE)
}

/// The keyset page a collection-page request asks for: the cursors from
/// the query string (a malformed one is the shared 400 `Invalid id`),
/// with the collection's own page size — the size is the endpoint's, not
/// the client's.
fn keyset_page(query: Option<&str>, size: i64) -> AppResult<PageParams> {
    let parsed = PageParams::from_query(query, MAX_LIMIT)?;
    Ok(PageParams {
        limit: size,
        ..parsed
    })
}

/// The page's own address: the collection with the request's query echoed
/// back, Pleroma's `current_url`.
fn requested_url(collection: &str, query: Option<&str>) -> String {
    query.map_or_else(
        || collection.to_owned(),
        |query| format!("{collection}?{query}"),
    )
}

/// The URL of the page of `collection` keyed on cursor `name`.
fn cursor_url(collection: &str, name: &str, id: Uuid) -> String {
    format!("{collection}?page=true&{name}={id}")
}

/// Whether `counted` rows filled a page of `size`, so the walk may yet
/// have a further page.
fn full_page(counted: usize, size: i64) -> bool {
    i64::try_from(counted).unwrap_or_default() == size
}

/// The envelope of one newest-first page: `next` walks older from the
/// page's oldest id while the page came back full, `prev` walks back
/// toward the newest end from the page's newest id, and the page's `id`
/// echoes the request.
fn descending_page_links(
    collection: &str,
    query: Option<&str>,
    newest: Option<Uuid>,
    oldest: Option<Uuid>,
    full: bool,
) -> PageLinks {
    PageLinks {
        id: requested_url(collection, query),
        part_of: collection.to_owned(),
        next: full
            .then_some(oldest)
            .flatten()
            .map(|id| cursor_url(collection, MAX_ID, id)),
        prev: newest.map(|id| cursor_url(collection, MIN_ID, id)),
    }
}

/// The envelope of one oldest-first page (the replies collection): `next`
/// walks newer from the page's newest id while the page came back full,
/// `prev` walks back toward the oldest end from the page's oldest id.
fn ascending_page_links(
    collection: &str,
    query: Option<&str>,
    oldest: Option<Uuid>,
    newest: Option<Uuid>,
    full: bool,
) -> PageLinks {
    PageLinks {
        id: requested_url(collection, query),
        part_of: collection.to_owned(),
        next: full
            .then_some(newest)
            .flatten()
            .map(|id| cursor_url(collection, MIN_ID, id)),
        prev: oldest.map(|id| cursor_url(collection, MAX_ID, id)),
    }
}

/// `user`'s outbox document: the collection root, or a page of activities.
///
/// With `?page=true` the newest 20 (`OUTBOX_PAGE_SIZE`) of the account's
/// publicly visible activities; the root's `totalItems` is the account's
/// denormalised post count — the ticket's estimate for the public and
/// unlisted total, so the root answers without a `COUNT` of its own.
///
/// # Errors
///
/// [`AppError::BadRequest`] when a cursor is no id; any database failure
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn outbox_document(
    pool: &PgPool,
    config: &Config,
    user: &User,
    query: Option<&str>,
) -> AppResult<Value> {
    let id = outbox(&user.ap_id);
    if !page_requested(query) {
        let total = i64::from(user.statuses_count);
        return Ok(collection_root(
            &id,
            ORDERED_COLLECTION,
            Some(total),
            config,
        ));
    }
    let page = keyset_page(query, OUTBOX_PAGE_SIZE)?;
    let rows = status::outbox_page(pool, user.id, &page).await?;
    let items = outbox_activities(pool, config, user, &rows).await?;
    let links = descending_page_links(
        &id,
        query,
        rows.first().map(|row| row.id),
        rows.last().map(|row| row.id),
        full_page(rows.len(), page.limit),
    );
    Ok(ordered_collection_page(&links, items, config))
}

/// The page rows as the activities an outbox lists, in page order: a
/// `Create` around each status's note document, an `Announce` for each
/// reblog (decision 6). A row whose object vanished between the page read
/// and its render drops out rather than failing the page.
async fn outbox_activities(
    pool: &PgPool,
    config: &Config,
    author: &User,
    rows: &[Status],
) -> AppResult<Vec<Value>> {
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.extend(outbox_activity(pool, config, author, row).await?);
    }
    Ok(items)
}

/// The one activity `row` is listed as, `None` when its object is gone.
async fn outbox_activity(
    pool: &PgPool,
    config: &Config,
    author: &User,
    row: &Status,
) -> AppResult<Option<Value>> {
    match row.reblog_of_id {
        Some(original_id) => announce_of(pool, config, author, row, original_id).await,
        None => create_of(pool, config, row).await,
    }
}

/// A reblog row as its `Announce`: the original's object id and author
/// drive Pleroma's addressing. The cascade keeps a reblog and its
/// original alive together, and a tombstoned original keeps its reserved
/// uri, so the reads above only miss a row the database no longer holds.
async fn announce_of(
    pool: &PgPool,
    config: &Config,
    author: &User,
    row: &Status,
    original_id: Uuid,
) -> AppResult<Option<Value>> {
    let Some(original) = status::get_by_id_including_deleted(pool, original_id).await? else {
        return Ok(None);
    };
    let Some(original_author) = user_by_id(pool, original.user_id).await? else {
        return Ok(None);
    };
    Ok(note::announce_activity_json(
        row,
        author,
        &original.uri,
        &original_author.ap_id,
        config,
    ))
}

/// A plain status row as its `Create`.
async fn create_of(pool: &PgPool, config: &Config, row: &Status) -> AppResult<Option<Value>> {
    match note::load_note_context(pool, config, row.id).await {
        Ok(ctx) => Ok(
            note::note_json(&ctx).map(|object| note::create_activity_json(&object, row, config))
        ),
        // The row fell between the page read and its render.
        Err(AppError::NotFound) => Ok(None),
        Err(err) => Err(err),
    }
}

/// `user`'s followers or following document: the root, or one page of it.
///
/// With `?page=true` the newest 10 (`FOLLOW_PAGE_SIZE`) actor ids of the
/// accepted relationships, keyset-paged on the follow row id. When the
/// account hides this side its pages carry no items at all, and when it
/// hides the count the root carries none.
///
/// # Errors
///
/// [`AppError::BadRequest`] when a cursor is no id; any database failure
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn follows_document(
    pool: &PgPool,
    config: &Config,
    user: &User,
    side: FollowSide,
    query: Option<&str>,
) -> AppResult<Value> {
    let id = format!("{}/{}", user.ap_id, side.path());
    if !page_requested(query) {
        return Ok(collection_root(
            &id,
            ORDERED_COLLECTION,
            side.total_items(user),
            config,
        ));
    }
    let page = keyset_page(query, FOLLOW_PAGE_SIZE)?;
    let entries = if side.hidden(user) {
        Vec::new()
    } else {
        side.fetch(pool, user.id, page).await?
    };
    let links = descending_page_links(
        &id,
        query,
        entries.first().map(|entry| entry.follow_id),
        entries.last().map(|entry| entry.follow_id),
        full_page(entries.len(), FOLLOW_PAGE_SIZE),
    );
    let items = entries
        .iter()
        .map(|entry| json!(&entry.user.ap_id))
        .collect();
    Ok(ordered_collection_page(&links, items, config))
}

/// `user`'s featured collection: the whole pinned list in one document,
/// each pin's status as its full note object (Pleroma's
/// `render("featured.json", …)`).
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn featured_document(pool: &PgPool, config: &Config, user: &User) -> AppResult<Value> {
    let rows = pinned_status::list_for_user(pool, user.id).await?;
    let mut objects = Vec::with_capacity(rows.len());
    for row in &rows {
        objects.extend(featured_object(pool, config, row).await?);
    }
    Ok(featured_collection(
        &format!("{}{FEATURED_PATH}", user.ap_id),
        objects,
        config,
    ))
}

/// One pinned row as its note document; a row that vanished between the
/// pin list and its render, or that federates as nothing (a `local`
/// post), contributes no object.
async fn featured_object(pool: &PgPool, config: &Config, row: &Status) -> AppResult<Option<Value>> {
    match note::load_note_context(pool, config, row.id).await {
        Ok(ctx) => Ok(note::note_json(&ctx)),
        Err(AppError::NotFound) => Ok(None),
        Err(err) => Err(err),
    }
}

/// The replies collection of `object`: the root with the count of
/// publicly visible answers, or — when the query asks for a page — the
/// 20 (`REPLIES_PAGE_SIZE`) oldest ones as their object ids.
///
/// # Errors
///
/// [`AppError::BadRequest`] when a cursor is no id; any database failure
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn replies_document(
    pool: &PgPool,
    config: &Config,
    object: &Status,
    query: Option<&str>,
) -> AppResult<Value> {
    let id = format!("{}/replies", object.uri);
    if !page_requested(query) {
        let total = status::public_replies_count(pool, object.id).await?;
        return Ok(collection_root(&id, COLLECTION, Some(total), config));
    }
    let page = keyset_page(query, REPLIES_PAGE_SIZE)?;
    let rows =
        status::public_replies_page(pool, object.id, page.min_id, page.max_id, page.limit).await?;
    let links = ascending_page_links(
        &id,
        query,
        rows.first().map(|row| row.id),
        rows.last().map(|row| row.id),
        full_page(rows.len(), page.limit),
    );
    let items = rows.iter().map(|row| json!(&row.uri)).collect();
    Ok(collection_page(&links, items, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::user::{Flag, NewUser, insert};

    /// The collection address every pure builder is tested against.
    const ROOT: &str = "https://test.example/users/alice/outbox";
    /// A cursor id for the page-link tests.
    const NEWEST: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// An older cursor id for the page-link tests.
    const OLDER: &str = "01934567-89ab-7cde-f012-4456789abcde";
    /// The newest cursor, parsed.
    fn newest() -> Uuid {
        Uuid::parse_str(NEWEST).expect("the fixture cursor parses")
    }

    /// The oldest cursor, parsed.
    fn oldest() -> Uuid {
        Uuid::parse_str(OLDER).expect("the fixture cursor parses")
    }

    /// The instance the builders render against.
    fn config() -> Config {
        Config::for_tests()
    }

    /// The sorted key set of a document object.
    fn keys(document: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = document
            .as_object()
            .expect("a document is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn the_root_carries_the_total_and_the_first_link() {
        let document = collection_root(ROOT, ORDERED_COLLECTION, Some(4), &config());
        assert_eq!(
            keys(&document),
            ["@context", "first", "id", "totalItems", "type"],
            "the documented key set"
        );
        assert_eq!(document["id"], ROOT, "the collection's own address");
        assert_eq!(document["type"], ORDERED_COLLECTION, "the type");
        assert_eq!(document["totalItems"], 4, "the total");
        assert_eq!(
            document["first"],
            format!("{ROOT}?page=true"),
            "the first page is the collection with the page query"
        );
        let context = document["@context"].as_array().expect("the context");
        assert_eq!(
            context[0], "https://www.w3.org/ns/activitystreams",
            "the ActivityStreams vocabulary comes first, as on every document"
        );
        assert_eq!(
            context[1], "https://test.example/schemas/litepub-0.1.jsonld",
            "then this instance's schema"
        );
    }

    #[test]
    fn the_root_omits_the_total_when_the_caller_has_none() {
        let document = collection_root(ROOT, COLLECTION, None, &config());
        assert_eq!(
            keys(&document),
            ["@context", "first", "id", "type"],
            "no totalItems key at all"
        );
        assert_eq!(document["type"], COLLECTION, "the caller's type");
    }

    #[test]
    fn an_ordered_page_carries_the_envelope_and_ordered_items() {
        let links = PageLinks {
            id: format!("{ROOT}?page=true"),
            part_of: ROOT.to_owned(),
            next: Some(format!("{ROOT}?page=true&max_id={OLDER}")),
            prev: None,
        };
        let document = ordered_collection_page(&links, vec![json!({"a": 1})], &config());
        assert_eq!(
            keys(&document),
            ["@context", "id", "next", "orderedItems", "partOf", "type"],
            "no prev without a prev link"
        );
        assert_eq!(document["type"], ORDERED_COLLECTION_PAGE, "the type");
        assert_eq!(document["partOf"], ROOT, "the root the page belongs to");
        assert_eq!(document["orderedItems"], json!([{"a": 1}]), "the items");
    }

    #[test]
    fn an_unordered_page_lists_items_under_the_plain_key() {
        let links = PageLinks {
            id: format!("{ROOT}?page=true"),
            part_of: ROOT.to_owned(),
            next: None,
            prev: Some(format!("{ROOT}?page=true&max_id={NEWEST}")),
        };
        let document = collection_page(&links, vec![json!("x"), json!("y")], &config());
        assert_eq!(
            keys(&document),
            ["@context", "id", "items", "partOf", "prev", "type"],
            "items, prev; no next, no orderedItems"
        );
        assert_eq!(document["type"], COLLECTION_PAGE, "the type");
        assert_eq!(
            document["items"],
            json!(["x", "y"]),
            "the replies page's key"
        );
    }

    #[test]
    fn the_featured_document_inlines_the_objects_and_counts_them() {
        let document = featured_collection(
            "https://test.example/users/alice/collections/featured",
            vec![json!({"id": "a"}), json!({"id": "b"})],
            &config(),
        );
        assert_eq!(
            keys(&document),
            ["@context", "id", "orderedItems", "totalItems", "type"],
            "one document, items inline"
        );
        assert_eq!(document["totalItems"], 2, "the pinned count");
        assert_eq!(
            document["orderedItems"],
            json!([{"id": "a"}, {"id": "b"}]),
            "the full objects, in pin order"
        );
    }

    #[test]
    fn only_page_true_asks_for_a_page() {
        assert!(page_requested(Some("page=true")), "the documented ask");
        assert!(
            page_requested(Some("foo=1&page=true")),
            "among other parameters"
        );
        assert!(
            !page_requested(Some("page=true&page=false")),
            "the last value of a repeated key wins, so this asks for the root"
        );
        assert!(!page_requested(None), "no query is the root");
        assert!(!page_requested(Some("")), "an empty query is the root");
        for query in ["page=1", "page", "page=false", "PAGE=true", "page=TRUE"] {
            assert!(
                !page_requested(Some(query)),
                "{query} is not Pleroma's truthy page ask"
            );
        }
    }

    #[test]
    fn descending_pages_walk_older_and_back_to_newer() {
        let first = descending_page_links(
            ROOT,
            Some("page=true"),
            Some(newest()),
            Some(oldest()),
            true,
        );
        assert_eq!(first.id, format!("{ROOT}?page=true"), "the ask echoes back");
        assert_eq!(first.part_of, ROOT, "the root");
        assert_eq!(
            first.next.as_deref(),
            Some(format!("{ROOT}?page=true&max_id={OLDER}").as_str()),
            "a full page offers the older page keyed on its oldest id"
        );
        assert_eq!(
            first.prev.as_deref(),
            Some(format!("{ROOT}?page=true&min_id={NEWEST}").as_str()),
            "and a page back toward the newest end keyed on its newest"
        );

        let last = descending_page_links(
            ROOT,
            Some("page=true&max_id=x"),
            Some(newest()),
            None,
            false,
        );
        assert_eq!(last.next, None, "a short page ends the walk");
        assert!(last.prev.is_some(), "but it still points back");

        let empty = descending_page_links(ROOT, Some("page=true"), None, None, false);
        assert!(
            empty.next.is_none() && empty.prev.is_none(),
            "no items, no links"
        );
    }

    #[test]
    fn ascending_pages_walk_newer_and_back_to_older() {
        let links = ascending_page_links(
            ROOT,
            Some("page=true"),
            Some(oldest()),
            Some(newest()),
            true,
        );
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
    }

    #[test]
    fn a_page_is_full_only_at_exactly_its_size() {
        assert!(full_page(20, 20), "a full page");
        assert!(!full_page(19, 20), "one short is the last page");
        assert!(!full_page(0, 20), "empty");
    }

    #[test]
    fn follow_sides_name_their_paths() {
        assert_eq!(FollowSide::Followers.path(), "followers");
        assert_eq!(FollowSide::Following.path(), "following");
    }

    #[tokio::test]
    async fn hidden_lists_and_counts_come_from_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let visible = insert(
            pool,
            &NewUser::local("visible", "https://test.example/users/visible"),
        )
        .await
        .expect("the plain fixture inserts");
        let hidden = insert(
            pool,
            &NewUser {
                hide_followers: Flag::new(true),
                hide_follows: Flag::new(true),
                hide_followers_count: Flag::new(true),
                hide_follows_count: Flag::new(true),
                ..NewUser::local("hidden", "https://test.example/users/hidden")
            },
        )
        .await
        .expect("the hidden fixture inserts");
        assert!(!FollowSide::Followers.hidden(&visible), "a shown list");
        assert!(FollowSide::Followers.hidden(&hidden), "a hidden list");
        assert!(FollowSide::Following.hidden(&hidden), "both sides");
        assert_eq!(
            FollowSide::Followers.total_items(&visible),
            Some(0),
            "a shown count is the stored counter"
        );
        assert_eq!(
            FollowSide::Followers.total_items(&hidden),
            None,
            "a hidden count carries no number at all"
        );
        assert_eq!(
            FollowSide::Following.total_items(&hidden),
            None,
            "both sides"
        );
    }
}
