//! The create path's rendering tests.
//!
//! What `create_status` links, stores and answers for mentions and
//! hashtags, remote accounts, rich text formats and a bare post — the
//! render half of the create the file [`super::create`] implements.
//!
//! [`super::create`]: crate::services::create_status::create_status

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::{
        Router,
        extract::{Path, Query, State},
        http::header::CONTENT_TYPE,
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::{
        services::create_status::{
            CreateStatusParams,
            test_support::{BASE, created, posted, state, stored, user},
        },
        util::entities::{hashtag_html, mention_html},
    };
    use tokio::{net::TcpListener, task::JoinHandle};
    #[tokio::test]
    async fn mentions_and_hashtags_are_linked_stored_and_answered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "linker").await;
        let bob = user(pool, "bob").await;

        let entity = created(
            &app_state,
            &alice,
            posted("hi @BOB and @nobody, see #Rust #rust #123"),
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(
            entity.content,
            format!(
                "<p>hi {} and @nobody, see {} {} #123</p>",
                mention_html(&bob.ap_id, "BOB"),
                hashtag_html(BASE, "rust"),
                hashtag_html(BASE, "rust"),
            ),
            "the known mention becomes its h-card in the written case, the \
             unknown one stays text, the tag links in both spellings and the \
             digits-only one is no tag"
        );
        assert_eq!(
            row.raw_text, "hi @BOB and @nobody, see #Rust #rust #123",
            "the source text keeps what was typed"
        );

        let mentioned: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the mention links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(
            mentioned,
            vec![bob.id],
            "only the account this instance knows is linked"
        );

        let tagged: Vec<String> = sqlx::query_as::<_, (String,)>(
            "SELECT h.name FROM status_hashtags sh JOIN hashtags h ON h.id = sh.hashtag_id \
              WHERE sh.status_id = $1 ORDER BY h.name",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the tag links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(tagged, vec!["rust"], "the tag is stored once, folded");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the mentioned account answers in the entity"
        );
        assert_eq!(
            value["tags"],
            json!([{ "name": "rust", "url": format!("{BASE}/tags/rust") }]),
            "and so does the tag"
        );
    }

    /// The mock remote peer's `WebFinger` answer: any `acct:` handle gets a
    /// JRD whose self link points at this peer's own `/users/{nick}`.
    async fn mock_webfinger(
        State(base): State<String>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        let nick = params
            .get("resource")
            .and_then(|resource| resource.strip_prefix("acct:"))
            .and_then(|handle| handle.split_once('@').map(|(nick, _)| nick))
            .unwrap_or("alice");
        let jrd = json!({
            "subject": format!("acct:{nick}@mock.example"),
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{base}/users/{nick}"),
            }],
        });
        ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
    }

    /// The mock remote peer's actor document: self-identifying, named for
    /// the path segment.
    async fn mock_actor(State(base): State<String>, Path(nick): Path<String>) -> Response {
        let id = format!("{base}/users/{nick}");
        let document = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": nick,
            "name": "Mock",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
            },
        });
        (
            [(CONTENT_TYPE, "application/activity+json")],
            document.to_string(),
        )
            .into_response()
    }

    /// Serve the mock remote peer; answer with its origin and the task to
    /// abort when the test is done.
    async fn mock_peer() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/.well-known/webfinger", get(mock_webfinger))
            .route("/users/{nick}", get(mock_actor))
            .with_state(base.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        (base, server)
    }

    /// A remote mention fetched from the mock peer is linked into
    /// `status_mentions` and linkified in the HTML with its remote profile
    /// URL. The fetch itself (`WebFinger` + actor, budget and cap) is
    /// [`resolve_mentions`]'s tested behaviour; the loopback mock answers
    /// on `127.0.0.1`, so the posted spelling is the mirror's own handle
    /// (decision 7) and the mention resolves from the row the fetch stored.
    #[tokio::test]
    async fn a_resolved_remote_mention_is_stored_and_rendered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "remote-author").await;
        let (base, server) = mock_peer().await;

        let fetched = crate::services::mentions::resolve_mentions_with_base(
            &app_state,
            "hi @alice@mock.example",
            Some(&base),
        )
        .await
        .expect("the mock peer resolves")
        .pop()
        .expect("the remote mention resolved");
        server.abort();
        let mirror_acct = format!("alice@{}", base.trim_start_matches("http://"));
        assert_eq!(fetched.1.nickname, mirror_acct, "mirror handle");

        let entity = created(
            &app_state,
            &alice,
            posted(&format!("hi @{}!", fetched.1.nickname)),
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(
            entity.content,
            format!("<p>hi {}!</p>", mention_html(&fetched.1.ap_id, "alice")),
            "the remote mention is linkified to its actor URL"
        );
        let mentioned: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
        )
        .bind(row.id)
        .fetch_all(pool)
        .await
        .expect("the mention links read")
        .into_iter()
        .map(|link| link.0)
        .collect();
        assert_eq!(
            mentioned,
            vec![fetched.1.id],
            "the remote mirror is linked in status_mentions"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": fetched.1.id.to_string(), "username": "alice",
                     "url": fetched.1.ap_id, "acct": mirror_acct }]),
            "and answered in the entity"
        );
    }

    /// A mention of a domain that cannot be reached at all (`.invalid`
    /// never resolves) leaves the mention as plain text, resolves the
    /// known mentions beside it, and the status is created as always.
    #[tokio::test]
    async fn a_status_mentioning_an_unreachable_domain_still_creates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "patient-author").await;
        let bob = user(pool, "known-bob").await;

        let entity = created(
            &app_state,
            &alice,
            posted("hi @ghost@nowhere.invalid and @known-bob"),
        )
        .await;

        assert_eq!(
            entity.content,
            format!(
                "<p>hi @ghost@nowhere.invalid and {}</p>",
                mention_html(&bob.ap_id, "known-bob")
            ),
            "the unreachable mention stays exactly as written"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "known-bob",
                     "url": bob.ap_id, "acct": "known-bob" }]),
            "only the reachable account is answered"
        );
    }

    #[tokio::test]
    async fn a_bare_post_links_nothing_and_stores_no_entities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "plain").await;

        let entity = created(
            &app_state,
            &alice,
            posted("just text, with an email a@b.c and a #123"),
        )
        .await;
        let row = stored(pool, &entity).await;
        let value = serde_json::to_value(&entity).expect("the entity serializes");

        assert_eq!(
            value["content"], "<p>just text, with an email a@b.c and a #123</p>",
            "nothing in this text is an entity"
        );
        assert_eq!(value["mentions"], json!([]), "no mention, no entry");
        assert_eq!(value["tags"], json!([]), "no tag, no entry");
        let mentions: (i64,) =
            sqlx::query_as("SELECT count(*) FROM status_mentions WHERE status_id = $1")
                .bind(row.id)
                .fetch_one(pool)
                .await
                .expect("the mention links read");
        let tags: (i64,) =
            sqlx::query_as("SELECT count(*) FROM status_hashtags WHERE status_id = $1")
                .bind(row.id)
                .fetch_one(pool)
                .await
                .expect("the tag links read");
        assert_eq!(
            (mentions.0, tags.0),
            (0, 0),
            "and neither link table gained a row"
        );
    }

    #[tokio::test]
    async fn the_rich_formats_render_and_the_plain_one_escapes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "formatter").await;

        let markdown = created(
            &app_state,
            &alice,
            CreateStatusParams {
                content_type: Some("text/markdown".to_owned()),
                ..posted("a **bold** claim")
            },
        )
        .await;
        assert_eq!(
            markdown.content, "<p>a <strong>bold</strong> claim</p>",
            "markdown becomes the tag it means"
        );

        let html = created(
            &app_state,
            &alice,
            CreateStatusParams {
                content_type: Some("text/html".to_owned()),
                ..posted("<p>marked up</p><script>steal()</script>")
            },
        )
        .await;
        assert_eq!(
            html.content, "<p>marked up</p>",
            "submitted HTML keeps the allowed tags and loses the script \
             element with its text, as this instance's scrubber does"
        );

        let plain = created(
            &app_state,
            &alice,
            posted("<script>steal()</script> & friends"),
        )
        .await;
        assert_eq!(
            plain.content, "<p>&lt;script&gt;steal()&lt;/script&gt; &amp; friends</p>",
            "and plain text is escaped, so it can say those characters"
        );
    }
}
