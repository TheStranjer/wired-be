//! The per-user and per-instance pages, the quote nesting and the scope gates.

use super::*;

// --- the two envelopes -----------------------------------------------------------------

/// One account's page: the `{total, activities}` envelope of that account's
/// statuses alone, the total the whole set the filters select and not the
/// window, the id naming an account as the handle does, and the unknown
/// nickname being the 404.
#[tokio::test]
async fn the_user_page_lists_one_account_with_its_total() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_user_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_user_alice").await;
    let bob = common::create_local_user(&pool, "as_user_bob").await;
    let mine = ladder(&pool, &alice).await;
    let theirs = post(&pool, &bob, Visibility::Public).await;
    let my_boost = boost(&pool, &alice, &theirs).await;

    let path = format!("{ADMIN}/users/as_user_alice/statuses");
    let quiet = listed(&app, &path, &token).await;
    assert_eq!(
        activity_ids(&quiet),
        newest(&mine[..2]),
        "this account's two public visibilities, newest first, and none of \
         bob's"
    );
    assert_eq!(quiet["total"], json!(2), "the total of that same set");

    let open = listed(&app, &format!("{path}?godmode=true&page_size=3"), &token).await;
    assert_eq!(
        activity_ids(&open),
        newest(&mine[2..]),
        "godmode admits the private visibilities too, in the three-row window"
    );
    assert_eq!(
        open["total"],
        json!(5),
        "the total is the whole set, not the window it is cut with"
    );

    let boosts = listed(&app, &format!("{path}?with_reblogs=true"), &token).await;
    assert!(
        activity_ids(&boosts).contains(&my_boost.id.to_string()),
        "alice's own boost is hers to list when reblogs are asked for"
    );

    let by_id = listed(&app, &format!("{ADMIN}/users/{}/statuses", bob.id), &token).await;
    assert_eq!(
        activity_ids(&by_id),
        vec![theirs.id.to_string()],
        "the id names an account as well as the handle does"
    );

    let missing = get_with(&app, &format!("{ADMIN}/users/nobody/statuses"), &token).await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "a nickname nobody carries");
    common::assert_error(&missing, NOT_FOUND, "a nickname nobody carries");
}

/// One domain's page: the mirrors of one host and not another's, and the
/// `local` literal for this instance's own authors, both in the same
/// envelope and under the same filters.
#[tokio::test]
async fn the_instance_page_lists_one_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_inst_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_inst_alice").await;
    let first = common::create_remote_user(&pool, "one", "first.example").await;
    let second = common::create_remote_user(&pool, "two", "second.example").await;
    let home = post(&pool, &alice, Visibility::Public).await;
    let of_first = mirror_of(&pool, &first, "first.example", "a").await;
    let of_second = mirror_of(&pool, &second, "second.example", "b").await;

    let hosts = listed(
        &app,
        &format!("{ADMIN}/instances/first.example/statuses"),
        &token,
    )
    .await;
    assert_eq!(
        activity_ids(&hosts),
        vec![of_first.id.to_string()],
        "one host's mirrors only"
    );
    assert_eq!(hosts["total"], json!(1), "and that host's total");

    let local = listed(&app, &format!("{ADMIN}/instances/local/statuses"), &token).await;
    assert_eq!(
        activity_ids(&local),
        vec![home.id.to_string()],
        "the `local` literal is this instance's own authors"
    );

    let other = listed(
        &app,
        &format!("{ADMIN}/instances/second.example/statuses?with_reblogs=false"),
        &token,
    )
    .await;
    assert_eq!(
        activity_ids(&other),
        vec![of_second.id.to_string()],
        "the filters ride the domain page as they do the ledger"
    );

    let nowhere = listed(
        &app,
        &format!("{ADMIN}/instances/absent.example/statuses"),
        &token,
    )
    .await;
    assert!(
        activity_ids(&nowhere).is_empty() && nowhere["total"] == json!(0),
        "a domain nobody came from is the empty page, not a miss"
    );
}

/// The admin view asks no visibility question of what a post quotes: a
/// public status quoting a private one nests the quoted original on the
/// staff read, where the client surface leaves it null.
#[tokio::test]
async fn the_admin_view_nests_a_quote_the_public_view_hides() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_quote_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_quote_alice").await;
    let secret = post(&pool, &alice, Visibility::Private).await;
    let quoting = quote(&pool, &alice, &secret).await;

    let answer = listed(&app, &format!("{LEDGER}/{}", quoting.id), &token).await;
    assert_eq!(
        answer["quote_id"],
        secret.id.to_string(),
        "the id names the quote either way"
    );
    assert_eq!(
        answer["quote"]["id"],
        secret.id.to_string(),
        "and the admin view renders the private quote itself"
    );
    assert_eq!(answer["pleroma"]["quote_visible"], json!(true));

    let client = anonymous(&app, &format!("/api/v1/statuses/{}", quoting.id)).await;
    common::assert_answer(&client, StatusCode::OK, "the anonymous client read");
    assert_eq!(
        client.json_value()["quote"],
        Value::Null,
        "the anonymous client view keeps the same quote hidden"
    );
}

// --- the gate --------------------------------------------------------------------------

/// A moderator holds Pleroma's `messages_delete` but not `messages_read`:
/// the staff extractor lets them re-scope a post and delete it, and the
/// entries they leave name them as the actor.
#[tokio::test]
async fn a_moderator_writes_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_mod, token) = moderator(&pool, "as_mod", BOTH).await;
    let alice = common::create_local_user(&pool, "as_mod_alice").await;
    let row = post(&pool, &alice, Visibility::Private).await;

    let moved = rescoped(&app, &token, row.id, json!({"visibility": "public"})).await;
    assert_eq!(moved["visibility"], "public", "the moderator re-scopes it");
    let gone = removed(&app, &token, row.id).await;
    common::assert_answer(&gone, StatusCode::OK, "and deletes it");

    let actors: Vec<String> = log_lines(&pool, "status_update")
        .await
        .into_iter()
        .map(|(actor, _message)| actor)
        .collect();
    assert_eq!(
        actors,
        vec!["as_mod".to_owned()],
        "the entry names the moderator, not the author"
    );
}

/// The four reads are `messages_read`, which only admins hold: a moderator
/// with the read scope is refused every one with the 403 `User is not an
/// admin.`, while an admin with the same scope reads the ledger, and a
/// direct post is visible to them under `godmode`.
#[tokio::test]
async fn a_moderator_cannot_read_the_ledger_but_an_admin_can() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_mod, mod_token) = moderator(&pool, "as_rd_mod", "admin:read:statuses").await;
    let (_root, admin_token) = admin(&pool, "as_rd_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_rd_alice").await;
    let dm = post(&pool, &alice, Visibility::Direct).await;

    for path in read_paths("as_rd_alice", dm.id) {
        let refused = get_with(&app, &path, &mod_token).await;
        common::assert_answer(&refused, StatusCode::FORBIDDEN, "a moderator read");
        common::assert_error(&refused, NOT_ADMIN, "a moderator read");
    }

    let godmode = "godmode=true";
    let ledger = listed(&app, &format!("{LEDGER}?{godmode}"), &admin_token).await;
    assert_eq!(
        common::ids_of(&ledger),
        vec![dm.id.to_string()],
        "the admin sees the DM"
    );
    let shown = listed(&app, &format!("{LEDGER}/{}", dm.id), &admin_token).await;
    assert_eq!(shown["id"], dm.id.to_string(), "and shows it by id");
    let user_page = format!("{ADMIN}/users/as_rd_alice/statuses?{godmode}");
    let by_user = listed(&app, &user_page, &admin_token).await;
    assert_eq!(activity_ids(&by_user), vec![dm.id.to_string()]);
    let instance_page = format!("{ADMIN}/instances/local/statuses?{godmode}");
    let by_instance = listed(&app, &instance_page, &admin_token).await;
    assert_eq!(activity_ids(&by_instance), vec![dm.id.to_string()]);
}

/// Every read refuses what it must, whatever its filters: no token at all is
/// the 401 `Invalid credentials`, an app-only token is the 403 that asks for
/// a user, and a valid regular account's token is the 403 that says it is no
/// admin.
#[tokio::test]
async fn the_reads_demand_a_token_of_an_admin_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, user_token) = regular(&pool, "as_gate_alice", "admin:read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "admin:read:statuses").await;
    let posted = post(&pool, &alice, Visibility::Public).await;

    for path in read_paths("as_gate_alice", posted.id) {
        let open = anonymous(&app, &path).await;
        common::assert_answer(&open, StatusCode::UNAUTHORIZED, "no token at all");
        common::assert_error(&open, NO_TOKEN, "no token at all");

        let client = get_with(&app, &path, &app_only).await;
        common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&client, USER_REQUIRED, "an app-only token");

        let outsider = get_with(&app, &path, &user_token).await;
        common::assert_answer(&outsider, StatusCode::FORBIDDEN, "a non-staff token");
        common::assert_error(&outsider, NOT_ADMIN, "a non-admin token");
    }
}

/// The scope half of the gate is Pleroma's and per area: another admin
/// area's grant and a bare `read` are both refused on every read, the class
/// root `admin:read` serves the whole class, and the narrow
/// `admin:read:statuses` opens the four reads and neither write.
#[tokio::test]
async fn the_reads_ask_their_own_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, narrow) = admin(&pool, "as_area_root", "admin:read:statuses").await;
    let (_wrong, wrong_area) = admin(&pool, "as_area_other", "admin:read:chats").await;
    let (_bare, no_admin) = admin(&pool, "as_area_bare", "read").await;
    let (_wide, all_reads) = admin(&pool, "as_area_wide", "admin:read").await;
    let alice = common::create_local_user(&pool, "as_area_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    for path in read_paths("as_area_alice", row.id) {
        for (token, who) in [
            (&wrong_area, "another admin area only"),
            (&no_admin, "no admin grant at all"),
        ] {
            let refused = get_with(&app, &path, token).await;
            common::assert_answer(&refused, StatusCode::FORBIDDEN, who);
            common::assert_error(&refused, NO_ADMIN_READ, who);
        }
        let _open = listed(&app, &path, &all_reads).await;
    }

    let ledger = listed(&app, LEDGER, &narrow).await;
    assert_eq!(
        common::ids_of(&ledger),
        vec![row.id.to_string()],
        "the narrow read scope reads the ledger"
    );
    let write = removed(&app, &narrow, row.id).await;
    common::assert_answer(&write, StatusCode::FORBIDDEN, "a read-scope DELETE");
    common::assert_error(&write, NO_ADMIN_WRITE, "a read-scope DELETE");
}

/// The writes ask the write class and no other: `admin:read:statuses` is
/// refused on both, the narrow `admin:write:statuses` writes both and is
/// refused on the reads, and the class root serves either.
#[tokio::test]
async fn the_writes_ask_their_own_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, read_only) = admin(&pool, "as_w_root", "admin:read:statuses").await;
    let (_other, write_only) = admin(&pool, "as_w_other", "admin:write:statuses").await;
    let (_wide, all_writes) = admin(&pool, "as_w_wide", "admin:write").await;
    let alice = common::create_local_user(&pool, "as_w_alice").await;
    let one = post(&pool, &alice, Visibility::Public).await;
    let two = post(&pool, &alice, Visibility::Public).await;
    let three = post(&pool, &alice, Visibility::Public).await;

    let refused = refused_put(&app, &read_only, one.id, json!({"sensitive": true})).await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "a read-scope PUT");
    common::assert_error(&refused, NO_ADMIN_WRITE, "a read-scope PUT");

    let shown = get_with(&app, &format!("{LEDGER}/{}", one.id), &write_only).await;
    common::assert_answer(&shown, StatusCode::FORBIDDEN, "a write-scope read");
    common::assert_error(&shown, NO_ADMIN_READ, "a write-scope read");

    let moved = rescoped(&app, &write_only, one.id, json!({"visibility": "direct"})).await;
    assert_eq!(
        moved["visibility"], "direct",
        "the narrow write scope writes"
    );
    let gone = removed(&app, &write_only, two.id).await;
    common::assert_answer(&gone, StatusCode::OK, "and deletes");
    let wide = rescoped(&app, &all_writes, three.id, json!({"sensitive": true})).await;
    assert_eq!(wide["sensitive"], json!(true), "and so does the class root");

    assert_eq!(
        log_lines(&pool, "status_update").await.len(),
        2,
        "one entry per accepted write, and none for the refusal"
    );
}

// --- helpers kept below the tests they serve -------------------------------------------

/// Every read route of this surface, for the gate walks: the ledger with and
/// without its filters, its `{id}` twin on the post `id`, and the two
/// envelope listings — the account one keyed on the nickname `author`.
fn read_paths(author: &str, id: Uuid) -> Vec<String> {
    vec![
        LEDGER.to_owned(),
        format!("{LEDGER}?godmode=true&with_reblogs=true&local_only=true"),
        format!("{LEDGER}/{id}"),
        format!("{ADMIN}/users/{author}/statuses"),
        format!("{ADMIN}/instances/local/statuses"),
    ]
}
