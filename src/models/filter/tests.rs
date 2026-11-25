//! Tests of the v2 filter model, its keyword and status rows and the v1
//! phrase view over them.

use chrono::{Duration, Timelike as _};

use super::{
    phrase::{self, NewPhrase},
    *,
};
use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{self as status_model, NewStatus},
        user::User,
    },
    test_fixtures::local_user as user,
};

/// An instant `days` from now, negative ones for the past, with the
/// sub-microsecond part dropped so it survives PostgreSQL's microsecond
/// resolution unchanged.
fn offset(days: i64) -> DateTime<Utc> {
    (Utc::now() + Duration::days(days))
        .with_nanosecond(0)
        .expect("zero nanoseconds is always in range")
}

/// A filter titled `title` applying in `home`, with the defaults otherwise.
fn titled(title: &str) -> NewFilter {
    NewFilter {
        title: title.to_owned(),
        context: vec!["home".to_owned()],
        ..NewFilter::default()
    }
}

/// Creates a filter titled `title` for `owner` with one keyword `word`.
async fn seed(pool: &PgPool, owner: &User, title: &str, word: &str) -> Filter {
    create(pool, owner.id, &titled(title), &[NewKeyword::new(word)])
        .await
        .expect("a fixture filter creates")
}

/// The titles of `rows`, in row order.
fn titles(rows: &[Filter]) -> Vec<&str> {
    rows.iter().map(|row| row.title.as_str()).collect()
}

#[tokio::test]
async fn create_round_trips_every_column_and_its_keywords() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let expires = offset(2);
    let stored = create(
        &pool,
        alice.id,
        &NewFilter {
            title: "Spoilers".to_owned(),
            context: vec!["home".to_owned(), "public".to_owned()],
            action: FilterAction::Blur,
            expires_at: Some(expires),
        },
        &[
            NewKeyword::new("one"),
            NewKeyword {
                keyword: "two".to_owned(),
                whole_word: false,
            },
        ],
    )
    .await
    .expect("a filter creates");

    assert_eq!(stored.id.get_version_num(), 7, "id is a UUIDv7");
    assert_eq!(stored.user_id, alice.id, "the owner is the argument");
    assert_eq!(stored.title, "Spoilers", "the title is stored verbatim");
    assert_eq!(stored.context, ["home", "public"], "contexts keep order");
    assert_eq!(stored.action, FilterAction::Blur, "the action round trips");
    assert_eq!(stored.expires_at, Some(expires), "and so does the expiry");
    let words: Vec<(&str, bool)> = stored
        .keywords
        .iter()
        .map(|found| (found.phrase.as_str(), found.whole_word))
        .collect();
    assert_eq!(words, [("one", true), ("two", false)], "both keywords");
    assert!(stored.statuses.is_empty(), "no status entries yet");

    let fetched = get_owned(&pool, stored.id, alice.id)
        .await
        .expect("the owner's read succeeds");
    assert_eq!(fetched, Some(stored), "a read returns what create did");
}

#[test]
fn actions_are_named_and_parsed_symmetrically() {
    for action in [FilterAction::Warn, FilterAction::Hide, FilterAction::Blur] {
        assert_eq!(
            FilterAction::from_name(action.as_str()),
            Some(action),
            "`{}` reads back",
            action.as_str()
        );
    }
    assert_eq!(FilterAction::from_name("mute"), None, "unknown is unknown");
    assert_eq!(
        FilterAction::default(),
        FilterAction::Warn,
        "warn is default"
    );
}

#[tokio::test]
async fn lists_are_newest_first_owned_and_expiry_aware() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let _first = seed(&pool, &alice, "first", "a").await;
    let _theirs = seed(&pool, &bob, "theirs", "b").await;
    let _past = create(
        &pool,
        alice.id,
        &NewFilter {
            expires_at: Some(offset(-2)),
            ..titled("past")
        },
        &[NewKeyword::new("c")],
    )
    .await
    .expect("an expired filter creates");
    let _future = create(
        &pool,
        alice.id,
        &NewFilter {
            expires_at: Some(offset(2)),
            ..titled("future")
        },
        &[],
    )
    .await
    .expect("a future filter creates");

    let active = list_active(&pool, alice.id).await.expect("active reads");
    assert_eq!(
        titles(&active),
        ["future", "first"],
        "the expired row leaves the active list; bob's stays his"
    );
    let all = list_all(&pool, alice.id).await.expect("all reads");
    assert_eq!(
        titles(&all),
        ["future", "past", "first"],
        "the full list keeps the expired row"
    );
    assert!(
        list_all(&pool, Uuid::now_v7())
            .await
            .expect("an unknown owner reads")
            .is_empty(),
        "nobody's filters are none"
    );
}

#[tokio::test]
async fn get_owned_serves_an_expired_filter_but_not_a_foreign_one() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let expired = create(
        &pool,
        alice.id,
        &NewFilter {
            expires_at: Some(offset(-1)),
            ..titled("expired")
        },
        &[],
    )
    .await
    .expect("the fixture creates");

    assert!(
        get_owned(&pool, expired.id, alice.id)
            .await
            .expect("the owner reads")
            .is_some(),
        "an expired filter is still a row its owner can read"
    );
    assert!(
        get_owned(&pool, expired.id, bob.id)
            .await
            .expect("a foreign read succeeds")
            .is_none(),
        "a foreign id reads as absent"
    );
    assert!(
        get_owned(&pool, Uuid::now_v7(), alice.id)
            .await
            .expect("an unknown read succeeds")
            .is_none(),
        "and so does an unknown one"
    );
}

#[tokio::test]
async fn update_changes_only_the_named_columns() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let stored = create(
        &pool,
        alice.id,
        &NewFilter {
            expires_at: Some(offset(1)),
            ..titled("before")
        },
        &[NewKeyword::new("kept")],
    )
    .await
    .expect("the fixture creates");

    let renamed = update(
        &pool,
        stored.id,
        alice.id,
        &FilterPatch {
            title: Some("after".to_owned()),
            action: Some(FilterAction::Hide),
            ..FilterPatch::default()
        },
        &[],
    )
    .await
    .expect("the update runs")
    .expect("the fixture exists");
    assert_eq!(renamed.title, "after", "the title changes");
    assert_eq!(renamed.action, FilterAction::Hide, "and the action");
    assert_eq!(renamed.context, stored.context, "the contexts stay");
    assert_eq!(renamed.expires_at, stored.expires_at, "and so does expiry");
    assert_eq!(renamed.keywords, stored.keywords, "and the keywords");
    assert_eq!(renamed.inserted_at, stored.inserted_at, "creation stays");

    let cleared = update(
        &pool,
        stored.id,
        alice.id,
        &FilterPatch {
            expires_at: ExpiryChange::Clear,
            ..FilterPatch::default()
        },
        &[],
    )
    .await
    .expect("the update runs")
    .expect("the fixture exists");
    assert_eq!(cleared.expires_at, None, "an explicit clear stops expiry");

    let later = offset(5);
    let set = update(
        &pool,
        stored.id,
        alice.id,
        &FilterPatch {
            expires_at: ExpiryChange::Set(later),
            context: Some(vec!["thread".to_owned()]),
            ..FilterPatch::default()
        },
        &[],
    )
    .await
    .expect("the update runs")
    .expect("the fixture exists");
    assert_eq!(set.expires_at, Some(later), "a set replaces the expiry");
    assert_eq!(set.context, ["thread"], "the contexts replace too");
}

#[tokio::test]
async fn update_applies_keyword_changes_atomically() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let stored = create(
        &pool,
        alice.id,
        &titled("words"),
        &[NewKeyword::new("old"), NewKeyword::new("doomed")],
    )
    .await
    .expect("the fixture creates");
    let [old, doomed] = stored.keywords.as_slice() else {
        panic!("two keywords were created");
    };

    let changed = update(
        &pool,
        stored.id,
        alice.id,
        &FilterPatch::default(),
        &[
            KeywordChange {
                id: Some(old.id),
                keyword: Some("new".to_owned()),
                whole_word: Some(false),
                destroy: false,
            },
            KeywordChange {
                id: Some(doomed.id),
                destroy: true,
                ..KeywordChange::default()
            },
            KeywordChange {
                keyword: Some("added".to_owned()),
                ..KeywordChange::default()
            },
        ],
    )
    .await
    .expect("the update runs")
    .expect("the fixture exists");
    let words: Vec<(&str, bool)> = changed
        .keywords
        .iter()
        .map(|found| (found.phrase.as_str(), found.whole_word))
        .collect();
    assert_eq!(
        words,
        [("new", false), ("added", true)],
        "one edited, one removed, one added"
    );

    let rejected = update(
        &pool,
        stored.id,
        alice.id,
        &FilterPatch {
            title: Some("never".to_owned()),
            ..FilterPatch::default()
        },
        &[KeywordChange {
            id: Some(Uuid::now_v7()),
            destroy: true,
            ..KeywordChange::default()
        }],
    )
    .await
    .expect("the update runs");
    assert!(rejected.is_none(), "a foreign keyword id fails the update");
    let after = get_owned(&pool, stored.id, alice.id)
        .await
        .expect("the read runs")
        .expect("the filter is there");
    assert_eq!(after.title, "words", "and nothing of it was applied");
}

#[tokio::test]
async fn update_and_delete_of_a_foreign_or_missing_id_do_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let hers = seed(&pool, &alice, "mine", "x").await;

    let patch = FilterPatch {
        title: Some("stolen".to_owned()),
        ..FilterPatch::default()
    };
    assert!(
        update(&pool, hers.id, bob.id, &patch, &[])
            .await
            .expect("a foreign update is not a failure")
            .is_none(),
        "bob rewrites nothing of alice's"
    );
    assert!(
        update(&pool, Uuid::now_v7(), alice.id, &patch, &[])
            .await
            .expect("an unknown update is not a failure")
            .is_none(),
        "no row, no rewrite"
    );
    assert!(
        !delete(&pool, hers.id, bob.id)
            .await
            .expect("a foreign delete is not a failure"),
        "bob deletes nothing of alice's"
    );
    assert!(
        get_owned(&pool, hers.id, alice.id)
            .await
            .expect("the row reads")
            .is_some_and(|row| row.title == "mine"),
        "alice's filter survived every attempt"
    );
}

#[tokio::test]
async fn delete_takes_the_filter_with_its_keywords_and_statuses() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let doomed = seed(&pool, &alice, "doomed", "x").await;
    let post = status_model::insert(&pool, TEST_BASE_URL, &NewStatus::local(&alice, "hello"))
        .await
        .expect("the fixture post inserts");
    let entry = status::insert(&pool, doomed.id, post.id)
        .await
        .expect("the entry inserts")
        .expect("it is new");

    assert!(
        delete(&pool, doomed.id, alice.id)
            .await
            .expect("the filter deletes"),
        "the first delete finds it"
    );
    assert!(
        !delete(&pool, doomed.id, alice.id)
            .await
            .expect("deleting nothing is not a failure"),
        "the second finds nothing"
    );
    let keywords = keyword::for_filters(&pool, &[doomed.id])
        .await
        .expect("keywords read");
    assert!(keywords.is_empty(), "its keywords went with it");
    assert!(
        status::get_owned(&pool, entry.id, alice.id)
            .await
            .expect("the entry reads")
            .is_none(),
        "and so did its status entry"
    );
}

#[tokio::test]
async fn deleting_a_user_takes_their_filters() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let _hers = seed(&pool, &alice, "hers", "x").await;

    let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .expect("the fixture user deletes");
    assert_eq!(deleted.rows_affected(), 1, "one account went");

    let left = sqlx::query_scalar::<_, i64>(
        "SELECT (SELECT count(*) FROM custom_filters)
              + (SELECT count(*) FROM custom_filter_keywords)",
    )
    .fetch_one(&pool)
    .await
    .expect("the rows count");
    assert_eq!(left, 0, "and their filters and keywords with it");
}

#[tokio::test]
async fn keyword_rows_are_owned_through_their_filter() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let stored = seed(&pool, &alice, "mine", "word").await;
    let word = stored.keywords.first().expect("one keyword").clone();

    assert!(
        keyword::get_owned(&pool, word.id, bob.id)
            .await
            .expect("a foreign read succeeds")
            .is_none(),
        "bob cannot read alice's keyword"
    );
    let mut conn = pool.acquire().await.expect("a connection");
    let patch = KeywordPatch {
        keyword: Some("renamed".to_owned()),
        whole_word: None,
    };
    assert!(
        keyword::update_owned(&mut conn, word.id, bob.id, &patch)
            .await
            .expect("a foreign update succeeds")
            .is_none(),
        "bob cannot change it"
    );
    assert!(
        !keyword::delete_owned(&mut conn, word.id, bob.id)
            .await
            .expect("a foreign delete succeeds"),
        "bob cannot delete it"
    );
    let renamed = keyword::update_owned(&mut conn, word.id, alice.id, &patch)
        .await
        .expect("the owner updates")
        .expect("the keyword exists");
    assert_eq!(renamed.phrase, "renamed", "the owner renames it");
    assert!(renamed.whole_word, "an absent flag keeps the stored one");
    assert!(
        keyword::delete_owned(&mut conn, word.id, alice.id)
            .await
            .expect("the owner deletes"),
        "and removes it"
    );
}

#[tokio::test]
async fn status_entries_are_unique_per_filter_and_owned_through_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let stored = seed(&pool, &alice, "mine", "word").await;
    let post = status_model::insert(&pool, TEST_BASE_URL, &NewStatus::local(&bob, "hello"))
        .await
        .expect("the fixture post inserts");

    assert!(
        status::status_exists(&pool, post.id)
            .await
            .expect("the check runs"),
        "a real status exists"
    );
    assert!(
        !status::status_exists(&pool, Uuid::now_v7())
            .await
            .expect("the check runs"),
        "a made-up one does not"
    );
    let entry = status::insert(&pool, stored.id, post.id)
        .await
        .expect("the entry inserts")
        .expect("it is new");
    assert!(
        status::insert(&pool, stored.id, post.id)
            .await
            .expect("a repeat is not a failure")
            .is_none(),
        "a status joins a filter once"
    );
    let loaded = get_owned(&pool, stored.id, alice.id)
        .await
        .expect("the filter reads")
        .expect("it exists");
    assert_eq!(
        loaded.statuses,
        std::slice::from_ref(&entry),
        "the filter carries it"
    );
    assert!(
        status::get_owned(&pool, entry.id, bob.id)
            .await
            .expect("a foreign read succeeds")
            .is_none(),
        "bob cannot read alice's entry"
    );
    assert!(
        !status::delete_owned(&pool, entry.id, bob.id)
            .await
            .expect("a foreign delete succeeds"),
        "or delete it"
    );
    assert!(
        status::delete_owned(&pool, entry.id, alice.id)
            .await
            .expect("the owner deletes"),
        "but alice can"
    );
}

#[tokio::test]
async fn a_phrase_filter_is_one_filter_with_one_keyword_under_one_id() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let made = phrase::create_phrase(
        &pool,
        alice.id,
        &NewPhrase {
            phrase: "spam".to_owned(),
            context: vec!["home".to_owned()],
            irreversible: true,
            whole_word: false,
            ..NewPhrase::default()
        },
    )
    .await
    .expect("a phrase filter creates");

    assert_eq!(made.title, "spam", "the phrase titles the filter");
    assert_eq!(made.action, FilterAction::Hide, "irreversible is hide");
    let word = made.keywords.first().expect("the one keyword");
    assert_eq!(word.id, made.id, "filter and keyword share the id");
    assert!(!word.whole_word, "the flag rides on the keyword");

    let shown = phrase::get_phrase(&pool, made.id, alice.id)
        .await
        .expect("the phrase reads")
        .expect("it exists");
    assert!(shown.irreversible(), "hide reads back as irreversible");
    assert_eq!(shown.keyword.phrase, "spam", "the phrase is the keyword");
}

#[tokio::test]
async fn phrase_lists_hold_one_entry_per_keyword_of_active_filters() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let _plain = phrase::create_phrase(
        &pool,
        alice.id,
        &NewPhrase {
            phrase: "plain".to_owned(),
            context: vec!["home".to_owned()],
            ..NewPhrase::default()
        },
    )
    .await
    .expect("a phrase filter creates");
    let _grouped = create(
        &pool,
        alice.id,
        &titled("group"),
        &[NewKeyword::new("g1"), NewKeyword::new("g2")],
    )
    .await
    .expect("a grouped filter creates");
    let _gone = create(
        &pool,
        alice.id,
        &NewFilter {
            expires_at: Some(offset(-1)),
            ..titled("gone")
        },
        &[NewKeyword::new("old")],
    )
    .await
    .expect("an expired filter creates");

    let listed = phrase::list_active_phrases(&pool, alice.id)
        .await
        .expect("the list reads");
    let words: Vec<&str> = listed
        .iter()
        .map(|found| found.keyword.phrase.as_str())
        .collect();
    assert_eq!(
        words,
        ["g2", "g1", "plain"],
        "every keyword of every live filter, newest keyword first"
    );
}

#[tokio::test]
async fn updating_a_phrase_rewrites_the_keyword_and_the_parent() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let made = phrase::create_phrase(
        &pool,
        alice.id,
        &NewPhrase {
            phrase: "before".to_owned(),
            context: vec!["home".to_owned()],
            expires_at: Some(offset(1)),
            ..NewPhrase::default()
        },
    )
    .await
    .expect("a phrase filter creates");
    let rewrite = NewPhrase {
        phrase: "after".to_owned(),
        context: vec!["public".to_owned()],
        whole_word: false,
        ..NewPhrase::default()
    };

    assert!(
        phrase::update_phrase(&pool, made.id, bob.id, &rewrite)
            .await
            .expect("a foreign update is not a failure")
            .is_none(),
        "bob rewrites nothing"
    );
    let updated = phrase::update_phrase(&pool, made.id, alice.id, &rewrite)
        .await
        .expect("the update runs")
        .expect("the phrase exists");
    assert_eq!(updated.keyword.phrase, "after", "the phrase changes");
    assert!(!updated.keyword.whole_word, "and the flag");
    assert_eq!(updated.context, ["public"], "the contexts change");
    assert_eq!(updated.expires_at, None, "an absent expiry clears it");
    let parent = get_owned(&pool, made.id, alice.id)
        .await
        .expect("the parent reads")
        .expect("it exists");
    assert_eq!(parent.title, "after", "a sole keyword retitles the filter");
}

#[tokio::test]
async fn deleting_a_phrase_keeps_a_parent_that_still_holds_something() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let made = phrase::create_phrase(
        &pool,
        alice.id,
        &NewPhrase {
            phrase: "solo".to_owned(),
            context: vec!["home".to_owned()],
            ..NewPhrase::default()
        },
    )
    .await
    .expect("a phrase filter creates");
    let grouped = create(
        &pool,
        alice.id,
        &titled("group"),
        &[NewKeyword::new("g1"), NewKeyword::new("g2")],
    )
    .await
    .expect("a grouped filter creates");
    let [first, _second] = grouped.keywords.as_slice() else {
        panic!("two keywords were created");
    };

    assert!(
        !phrase::delete_phrase(&pool, made.id, bob.id)
            .await
            .expect("a foreign delete is not a failure"),
        "bob deletes nothing"
    );
    assert!(
        phrase::delete_phrase(&pool, made.id, alice.id)
            .await
            .expect("the delete runs"),
        "a sole phrase goes"
    );
    assert!(
        get_owned(&pool, made.id, alice.id)
            .await
            .expect("the read runs")
            .is_none(),
        "and its emptied parent with it"
    );
    assert!(
        phrase::delete_phrase(&pool, first.id, alice.id)
            .await
            .expect("the delete runs"),
        "one keyword of a group goes"
    );
    let survivor = get_owned(&pool, grouped.id, alice.id)
        .await
        .expect("the read runs")
        .expect("the group stays while it holds a keyword");
    assert_eq!(survivor.keywords.len(), 1, "with the other keyword");
}

/// The v1 `filters` table as the first migration made it.
const OLD_FILTERS_DDL: &str = "CREATE TABLE filters (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    phrase text NOT NULL,
    context text[] NOT NULL,
    whole_word boolean NOT NULL DEFAULT true,
    irreversible boolean NOT NULL DEFAULT false,
    expires_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
)";

#[tokio::test]
async fn the_migration_turns_v1_filters_into_v2_filters_with_one_keyword() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    // Everything runs in one transaction that is rolled back, so the shared
    // slot's schema is untouched: the v2 tables are dropped, the v1 table is
    // recreated and filled, and the migration file is replayed over them.
    let mut tx = pool.begin().await.expect("a transaction opens");
    let _dropped =
        sqlx::query("DROP TABLE custom_filter_statuses, custom_filter_keywords, custom_filters")
            .execute(&mut *tx)
            .await
            .expect("the v2 tables drop");
    let _created = sqlx::query(OLD_FILTERS_DDL)
        .execute(&mut *tx)
        .await
        .expect("the v1 table is recreated");
    let hider = Uuid::now_v7();
    let warner = Uuid::now_v7();
    for (id, phrase, whole_word, irreversible) in
        [(hider, "spam", false, true), (warner, "news", true, false)]
    {
        let _inserted = sqlx::query(
            "INSERT INTO filters (id, user_id, phrase, context, whole_word, irreversible)
             VALUES ($1, $2, $3, ARRAY['home', 'public'], $4, $5)",
        )
        .bind(id)
        .bind(alice.id)
        .bind(phrase)
        .bind(whole_word)
        .bind(irreversible)
        .execute(&mut *tx)
        .await
        .expect("a v1 filter inserts");
    }
    let _migrated = sqlx::raw_sql(include_str!(
        "../../../migrations/20261125071035_custom_filters.sql"
    ))
    .execute(&mut *tx)
    .await
    .expect("the migration replays");

    let filters = sqlx::query_as::<_, (Uuid, String, Vec<String>, String)>(
        "SELECT id, title, context, filter_action FROM custom_filters ORDER BY title",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("the v2 filters read");
    assert_eq!(
        filters,
        [
            (
                warner,
                "news".to_owned(),
                vec!["home".to_owned(), "public".to_owned()],
                "warn".to_owned()
            ),
            (
                hider,
                "spam".to_owned(),
                vec!["home".to_owned(), "public".to_owned()],
                "hide".to_owned()
            ),
        ],
        "each v1 filter is a v2 filter under its own id; irreversible is hide"
    );
    let keywords = sqlx::query_as::<_, (Uuid, Uuid, String, bool)>(
        "SELECT id, filter_id, keyword, whole_word FROM custom_filter_keywords ORDER BY keyword",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("the keywords read");
    assert_eq!(
        keywords,
        [
            (warner, warner, "news".to_owned(), true),
            (hider, hider, "spam".to_owned(), false),
        ],
        "each holds its phrase as the one keyword, under the same id"
    );
    tx.rollback().await.expect("the transaction rolls back");
}
