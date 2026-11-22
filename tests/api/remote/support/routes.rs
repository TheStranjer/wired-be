//! The mock peer's HTTP surface: the router and its handlers.

use super::*;

/// The router serving `peer`'s whole surface: the actors, the key mirror, the
/// notes, the outboxes, `WebFinger` and the capturing inboxes.
pub(super) fn router(peer: Peer) -> Router {
    Router::new()
        .route("/.well-known/webfinger", get(webfinger))
        .route("/key", get(key_mirror))
        .route("/relay", get(relay_actor))
        .route("/objects/{name}", get(note))
        .route("/users/{nick}", get(person_actor))
        .route("/users/{nick}/outbox", get(outbox))
        .route("/users/{nick}/inbox", post(capture))
        .route(INBOX, post(capture))
        .with_state(peer)
}

/// The actor document every mock account wears: self-identifying, its inbox
/// under its own id, the peer-wide inbox as its `endpoints.sharedInbox`, and
/// the real public key its signatures verify against — the key pair
/// [`rotate_keys`] has moved the peer to, whichever that is now.
///
/// [`rotate_keys`]: MockPeer::rotate_keys
fn actor_doc(peer: &Peer, id: &str, kind: &str, preferred: &str) -> Value {
    let public_pem = peer.public_pem();
    json!({
        "@context": AS_CONTEXT,
        "type": kind,
        "id": id,
        "preferredUsername": preferred,
        "name": preferred,
        "summary": "an account on the mock federation peer",
        "inbox": format!("{id}/inbox"),
        "outbox": format!("{id}/outbox"),
        "followers": format!("{id}/followers"),
        "following": format!("{id}/following"),
        "endpoints": { "sharedInbox": format!("{}{INBOX}", peer.base) },
        "manuallyApprovesFollowers": false,
        "discoverable": true,
        "publicKey": {
            "id": main_key(id),
            "owner": id,
            "publicKeyPem": public_pem,
        },
    })
}

/// Answers `document` with the media type `mime`.
fn document_response(document: &Value, mime: &'static str) -> Response {
    (
        [(CONTENT_TYPE, HeaderValue::from_static(mime))],
        document.to_string(),
    )
        .into_response()
}

/// `GET /users/{nick}`: one of the peer's `Person` accounts.
async fn person_actor(State(peer): State<Peer>, Path(nick): Path<String>) -> Response {
    let id = format!("{}/users/{nick}", peer.base);
    document_response(&actor_doc(&peer, &id, "Person", &nick), AP_CONTENT_TYPE)
}

/// `GET /relay`: the peer's relay, a `Service` whose inbox is the shared one.
async fn relay_actor(State(peer): State<Peer>) -> Response {
    let id = format!("{}/relay", peer.base);
    let mut document = actor_doc(&peer, &id, "Service", "relay");
    document["inbox"] = json!(format!("{}{INBOX}", peer.base));
    document_response(&document, AP_CONTENT_TYPE)
}

/// `GET /objects/{name}`: whatever shaped document [`MockPeer::serve_note`]
/// put at that name, and otherwise the peer's own public `Note` by `author`.
async fn note(State(peer): State<Peer>, Path(name): Path<String>) -> Response {
    let served = {
        let shelf = peer.notes.lock().expect("the notes lock");
        shelf
            .get(&name)
            .cloned()
            .unwrap_or_else(|| canned_note(&peer, &name))
    };
    document_response(&served, AP_CONTENT_TYPE)
}

/// The `Note` a name nothing was shaped for answers with: public, authored by
/// the peer's own `author` account, naming `name` in its body so a test can
/// tell two of them apart.
fn canned_note(peer: &Peer, name: &str) -> Value {
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{}/objects/{name}", peer.base),
        "type": "Note",
        "attributedTo": format!("{}/users/author", peer.base),
        "to": [PUBLIC],
        "cc": [],
        "content": format!("<p>a note relayed as {name}</p>"),
        "published": DEFAULT_PUBLISHED,
    })
}

/// `GET /key`: the peer's current public key as a standalone `Key` object —
/// the `security` vocabulary's shape, `owner` naming an actor this peer also
/// serves — the mirror a fetcher reads when a `keyId` names the key instead
/// of dereferencing an actor document.
async fn key_mirror(State(peer): State<Peer>) -> Response {
    document_response(
        &json!({
            "@context": SECURITY_CONTEXT,
            "id": format!("{}/key", peer.base),
            "type": "Key",
            "owner": format!("{}/users/{KEY_OWNER}", peer.base),
            "publicKeyPem": peer.public_pem(),
        }),
        AP_CONTENT_TYPE,
    )
}

/// `GET /users/{nick}/outbox`: the actor's `OrderedCollection` root, and —
/// under `?page=true`, the query this instance's own collections are paged
/// with — one [`OUTBOX_PAGE`]-sized page of the activity ids
/// [`MockPeer::publish`] listed for that actor.
async fn outbox(
    State(peer): State<Peer>,
    Path(nick): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let root = format!("{}/users/{nick}/outbox", peer.base);
    let ids = peer.listed(&nick);
    if !page_requested(&params) {
        return document_response(
            &json!({
                "@context": AS_CONTEXT,
                "id": root,
                "type": "OrderedCollection",
                "totalItems": ids.len(),
                "first": format!("{root}?page=true"),
            }),
            AP_CONTENT_TYPE,
        );
    }
    let offset = params
        .get("offset")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    document_response(&outbox_page(&root, &ids, offset), AP_CONTENT_TYPE)
}

/// One page of a mock outbox: the activity ids `offset` windows onto, as
/// `orderedItems` of an `OrderedCollectionPage`, with the `next` and `prev`
/// links the window leaves standing on the peer's own `offset` cursor.
fn outbox_page(root: &str, ids: &[String], offset: usize) -> Value {
    let page: Vec<Value> = ids
        .iter()
        .skip(offset)
        .take(OUTBOX_PAGE)
        .map(|id| json!(id))
        .collect();
    let mut document = json!({
        "@context": AS_CONTEXT,
        "id": format!("{root}?page=true&offset={offset}"),
        "type": "OrderedCollectionPage",
        "partOf": root,
        "orderedItems": page,
    });
    if offset + OUTBOX_PAGE < ids.len() {
        let next = offset.saturating_add(OUTBOX_PAGE);
        document["next"] = json!(format!("{root}?page=true&offset={next}"));
    }
    if offset > 0 {
        let prev = offset.saturating_sub(OUTBOX_PAGE);
        document["prev"] = json!(format!("{root}?page=true&offset={prev}"));
    }
    document
}

/// Whether `params` ask for a page of a collection rather than its root:
/// `page=true`, the spelling this instance's own `OrderedCollection`s answer.
fn page_requested(params: &HashMap<String, String>) -> bool {
    params.get("page").is_some_and(|value| value == "true")
}
/// `GET /.well-known/webfinger`: a JRD echoing the asked `acct:` resource and
/// naming that nickname's actor on this peer.
async fn webfinger(
    State(peer): State<Peer>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(resource) = params
        .get("resource")
        .and_then(|value| value.strip_prefix("acct:"))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let nickname = resource.split('@').next().unwrap_or(resource);
    document_response(
        &json!({
            "subject": format!("acct:{resource}"),
            "links": [{
                "rel": "self",
                "type": AP_CONTENT_TYPE,
                "href": format!("{}/users/{nickname}", peer.base),
            }],
        }),
        JRD_CONTENT_TYPE,
    )
}

/// Both inboxes: record the path, headers and bytes as they arrived, and
/// answer the next scripted status when the test queued one (with its
/// `Retry-After` when the script carries one), the plain `202 Accepted`
/// when nothing was scripted.
async fn capture(State(peer): State<Peer>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let delivery = Delivery {
        path: uri.path().to_owned(),
        activity: serde_json::from_slice(&body).unwrap_or(Value::Null),
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        body,
    };
    peer.posts.lock().expect("the capture lock").push(delivery);
    next_scripted(&peer.script).map_or_else(
        || StatusCode::ACCEPTED.into_response(),
        |scripted| answer_with(scripted.status, scripted.retry_after),
    )
}

/// The response for a scripted answer: its status, with a `Retry-After`
/// delta-seconds header when the script carries one.
fn answer_with(status: StatusCode, retry_after: Option<u64>) -> Response {
    let header = retry_after.and_then(|seconds| HeaderValue::try_from(seconds.to_string()).ok());
    header.map_or_else(
        || status.into_response(),
        |value| (status, [(RETRY_AFTER, value)]).into_response(),
    )
}

/// The next scripted answer, spent down: the queue's head answers this
/// `POST` and loses one of its remaining hits, and an exhausted entry is
/// off the queue for the next one. `None` — the plain `202` — when nothing
/// stands scripted.
///
/// # Panics
///
/// If the script lock is poisoned, which only a panicking handler leaves
/// behind.
fn next_scripted(script: &Arc<Mutex<VecDeque<Scripted>>>) -> Option<Scripted> {
    let scripted = {
        let mut queue = script.lock().expect("the script lock");
        let mut next = queue.front().cloned()?;
        next.remaining = next.remaining.saturating_sub(1);
        if next.remaining == 0 {
            let _spent: Option<Scripted> = queue.pop_front();
        }
        next
    };
    Some(scripted)
}
