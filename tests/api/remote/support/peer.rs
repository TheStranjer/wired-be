//! The [`MockPeer`] a test drives: starting it, shaping what it serves and
//! reading what it captured.

use super::*;

impl MockPeer {
    /// Binds a loopback port and serves the whole peer surface.
    ///
    /// The returned peer is the test's handle on all of it — the
    /// [`origin`](Self::origin) to point a handle at, the [`Delivery`]
    /// capture to read the deliveries off, and the task serving both:
    /// [`Drop`] aborts that task, so a test must hold the peer alive for as
    /// long as the instance is expected to reach it (which is why no test
    /// binds it to `_`).
    ///
    /// # Panics
    ///
    /// If no loopback port can be bound, which tests never provoke.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let address = listener.local_addr().expect("a bound address");
        let peer = Peer {
            base: format!("http://{address}"),
            posts: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(VecDeque::new())),
            key: Arc::new(Mutex::new(Arc::new(peer_keys().clone()))),
            notes: Arc::new(Mutex::new(HashMap::new())),
            outbox: Arc::new(Mutex::new(HashMap::new())),
        };
        let app = router(peer.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        Self { peer, server }
    }

    /// The peer's `host:port` authority: the domain half of every handle it
    /// serves, since decision 7 spells a remote account `user@host`.
    #[must_use]
    pub fn host(&self) -> String {
        self.peer
            .base
            .split_once("://")
            .map_or_else(String::new, |(_scheme, rest)| rest.to_owned())
    }

    /// The peer's full `http://{addr}` origin: the base every id it serves
    /// hangs off, which a test needs when it builds a URL the peer does
    /// *not* serve — a key id whose actor no lookup can settle.
    #[must_use]
    pub fn origin(&self) -> String {
        self.peer.base.clone()
    }

    /// The actor id of this peer's account `nick`.
    #[must_use]
    pub fn actor(&self, nick: &str) -> String {
        format!("{}/users/{nick}", self.peer.base)
    }

    /// The `acct:` handle of this peer's account `nick`, as a client of this
    /// instance names it.
    #[must_use]
    pub fn handle(&self, nick: &str) -> String {
        format!("{nick}@{}", self.host())
    }

    /// The request path of the inbox belonging to the peer actor `actor`.
    ///
    /// The capture records where a delivery arrived, so a test compares
    /// against this rather than against the absolute URL the actor document
    /// names.
    #[must_use]
    pub fn inbox_path(&self, actor: &str) -> String {
        actor
            .strip_prefix(&self.peer.base)
            .map_or_else(|| INBOX.to_owned(), |tail| format!("{tail}/inbox"))
    }

    /// The peer-wide shared inbox, which is also the relay's inbox.
    #[must_use]
    pub fn shared_inbox(&self) -> String {
        format!("{}{INBOX}", self.peer.base)
    }

    /// The relay actor's id: a `Service` served at `/relay`.
    #[must_use]
    pub fn relay(&self) -> String {
        format!("{}/relay", self.peer.base)
    }

    /// The object id of the public `Note` this peer serves as `name`.
    #[must_use]
    pub fn note(&self, name: &str) -> String {
        format!("{}/objects/{name}", self.peer.base)
    }

    /// An activity id on this peer's host, which is what the dispatcher's
    /// containment check asks of a peer's own activity.
    #[must_use]
    pub fn activity_id(&self, name: &str) -> String {
        format!("{}/activities/{name}", self.peer.base)
    }

    /// An activity of `kind` by the peer's actor `actor`, named `name` on this
    /// peer's host, whose `object` is `object` — an id, or an embedded
    /// document.
    #[must_use]
    pub fn activity(&self, kind: &str, name: &str, actor: &str, object: &Value) -> Value {
        json!({
            "id": self.activity_id(name),
            "type": kind,
            "actor": actor,
            "object": object,
        })
    }

    /// A `Follow` by the peer's actor `follower` of the account `target`.
    #[must_use]
    pub fn follow_of(&self, name: &str, follower: &str, target: &str) -> Value {
        self.activity("Follow", name, follower, &json!(target))
    }

    /// The `OrderedCollection` address of the peer actor `nick`'s outbox, as
    /// its actor document names it.
    #[must_use]
    pub fn outbox_url(&self, nick: &str) -> String {
        format!("{}/outbox", self.actor(nick))
    }

    /// The address of the standalone `Key` mirror this peer serves — the
    /// document a `keyId` that names the key, and not an actor, resolves to.
    #[must_use]
    pub fn key_url(&self) -> String {
        format!("{}/key", self.peer.base)
    }

    /// Lists `activity_id` at the end of the outbox of the peer's actor
    /// `nick`, which is how a test decides what `GET /users/{nick}/outbox`
    /// pages through. The ids of the activities [`activity`] built with
    /// [`activity_id`](Self::activity_id) are what belongs here: the outbox
    /// carries activity *references*, not documents.
    ///
    /// [`activity`]: Self::activity
    ///
    /// # Panics
    ///
    /// If the outbox lock is poisoned, which only a panicking handler leaves
    /// behind.
    pub fn publish(&self, nick: &str, activity_id: &str) {
        let mut listing = self.peer.outbox.lock().expect("the outbox lock");
        listing
            .entry(nick.to_owned())
            .or_default()
            .push(activity_id.to_owned());
    }

    /// A public `Note` on this peer's origin named `name` — id
    /// [`note`](Self::note) — authored by `actor` and carrying `content`.
    /// Every field of `extra` merges over those defaults, which is how a
    /// scenario shapes one note: its `to` / `cc` addressing, `attachment`,
    /// `tag` (mentions, hashtags, custom emojis), `sensitive` and `summary`
    /// for a content warning, `inReplyTo`, or a `Question`'s `oneOf` and
    /// `closed`.
    ///
    /// [`note`]: Self::note
    #[must_use]
    pub fn note_document(&self, name: &str, actor: &str, content: &str, extra: Value) -> Value {
        let mut note = json!({
            "@context": AS_CONTEXT,
            "id": self.note(name),
            "type": "Note",
            "attributedTo": actor,
            "to": [PUBLIC],
            "cc": [],
            "content": content,
            "published": DEFAULT_PUBLISHED,
        });
        merge(&mut note, extra);
        note
    }

    /// Asks `GET /objects/{name}` to answer `document` from now on — the
    /// shaped note a remote reader dereferences when this instance fetches
    /// the object by id (a thread backfill, a `resolve=true` status read).
    /// A name nothing was served for keeps answering the canned public note.
    ///
    /// # Panics
    ///
    /// If the notes lock is poisoned, which only a panicking handler leaves
    /// behind.
    pub fn serve_note(&self, name: &str, document: Value) {
        let mut served = self.peer.notes.lock().expect("the notes lock");
        let _previous: Option<Value> = served.insert(name.to_owned(), document);
    }

    /// Script the next `times` inbox `POST`s — on either endpoint, in
    /// arrival order — to be recorded as always but answered `status`
    /// instead of `202`, with a `Retry-After: {secs}` header when
    /// `retry_after` names a delay. This is how a scenario points the
    /// mock's inbox at a `500` a delivery must ride out, a `503` whose
    /// `Retry-After` the worker must honour, or a `401` that reads like a
    /// signature the peer can no longer verify.
    ///
    /// # Panics
    ///
    /// If the script lock is poisoned, which only a panicking handler
    /// leaves behind.
    pub fn script_inbox(&self, status: StatusCode, retry_after: Option<Duration>, times: usize) {
        let retry_after = retry_after.map(|delay| delay.as_secs());
        self.peer
            .script
            .lock()
            .expect("the script lock")
            .push_back(Scripted {
                status,
                retry_after,
                remaining: times,
            });
    }

    /// Script every later inbox `POST` to answer `status` (with
    /// `Retry-After` when asked): the closed address that never reopens,
    /// the peer that stays convinced a signature is bad.
    pub fn script_inbox_permanently(&self, status: StatusCode, retry_after: Option<Duration>) {
        self.script_inbox(status, retry_after, usize::MAX);
    }

    /// Generate and serve a fresh key pair from every document the peer
    /// publishes from now on — the peer mid-rotation. The pair [`deliver`]
    /// and the signing helpers use does not change: what this instance
    /// already mirrored of a mock actor still vouches for the old key, which
    /// is exactly the skew a rotation scenario reads back.
    ///
    /// # Panics
    ///
    /// If the new key pair cannot be generated, which it always can here.
    ///
    /// [`deliver`]: Self::deliver
    pub fn rotate_keys(&self) {
        let next = Arc::new(generate_keypair().expect("the mock peer rotates its key pair"));
        let mut served = self.peer.key.lock().expect("the key lock");
        *served = next;
    }

    /// The `publicKeyPem` the peer's actor documents carry right now — the
    /// rotated one after [`rotate_keys`](Self::rotate_keys), the process
    /// peer key before it. A test compares this against what a stored
    /// mirror still holds to show which side moved.
    ///
    /// # Panics
    ///
    /// If the key lock is poisoned, which only a panicking handler leaves
    /// behind.
    #[must_use]
    pub fn served_public_key(&self) -> String {
        self.peer.public_pem()
    }

    /// The four auth headers of a signed `POST` of `body` to this instance at
    /// `path`, presented as the peer's actor `actor` under [`main_key`].
    ///
    /// The peer signs every one of its actors with its single process-wide
    /// [key pair](peer_keys), so this needs no instance.
    ///
    /// # Panics
    ///
    /// If the URL cannot be signed, which the test instance's own origin never
    /// is.
    #[must_use]
    pub fn signed_headers(actor: &str, path: &str, body: &[u8]) -> SignedHeaders {
        let key_id = main_key(actor);
        sign_request(
            "POST",
            &format!("{}{path}", common::test_config().base_url),
            Some(body),
            &SigningKey {
                key_id: &key_id,
                private_pem: &peer_keys().private_pem,
            },
            Utc::now(),
        )
        .expect("the mock peer signs its own delivery")
    }

    /// The four auth headers of a signed `GET` of this instance at `path`,
    /// presented as the peer's actor `actor` under [`main_key`] — the
    /// proof an `AUTHORIZED_FETCH` gate reads, over the same origin
    /// [`signed_headers`] signs deliveries to.
    ///
    /// # Panics
    ///
    /// If the URL cannot be signed, which the test instance's own origin
    /// never is.
    #[must_use]
    pub fn signed_get(actor: &str, path: &str) -> SignedHeaders {
        let key_id = main_key(actor);
        sign_request(
            "GET",
            &format!("{}{path}", common::test_config().base_url),
            None,
            &SigningKey {
                key_id: &key_id,
                private_pem: &peer_keys().private_pem,
            },
            Utc::now(),
        )
        .expect("the mock peer signs its own fetch")
    }

    /// The public half of the peer's key pair: the `publicKeyPem` every
    /// mock actor publishes. A test that must let the peer deliver while
    /// the instance may not fetch its origin — a seeded mirror behind a
    /// closed SSRF guard — stores exactly this PEM.
    #[must_use]
    pub fn public_key_pem() -> &'static str {
        &peer_keys().public_pem
    }

    /// `POST`s `activity` to `path` on this instance as the peer's actor
    /// `actor`, signed.
    ///
    /// The answer comes back for the test to read: `202 Accepted` is the
    /// inbox's yes, and every refusal of the auth gate arrives as its own
    /// status.
    ///
    /// # Panics
    ///
    /// If `activity` does not serialize, which the fixtures never do.
    pub async fn deliver(
        &self,
        app: &Router,
        actor: &str,
        path: &str,
        activity: &Value,
    ) -> common::TestResponse {
        let body = serde_json::to_vec(activity).expect("the mock activity serializes");
        let wire = Self::signed_headers(actor, path, &body);
        let digest = wire.digest.as_deref().expect("a signed post digests");
        common::TestRequest::post(path)
            .json(activity)
            .header("host", &wire.host)
            .header("date", &wire.date)
            .header("digest", digest)
            .header("signature", &wire.signature)
            .send(app)
            .await
    }

    /// Every delivery this peer's inboxes recorded, oldest first.
    ///
    /// # Panics
    ///
    /// If the capture lock is poisoned, which only a panicking handler
    /// leaves behind.
    #[must_use]
    pub fn received(&self) -> Vec<Delivery> {
        self.peer.posts.lock().expect("the capture lock").clone()
    }

    /// The recorded deliveries whose activity names `kind` as its `type`,
    /// oldest first: the capture narrowed to the one activity a scenario
    /// asserts on.
    #[must_use]
    pub fn delivered(&self, kind: &str) -> Vec<Delivery> {
        self.received()
            .into_iter()
            .filter(|post| post.activity_type() == Some(kind))
            .collect()
    }

    /// How many deliveries this peer has recorded, which is how a test says
    /// "and nothing else left this instance".
    #[must_use]
    pub fn delivery_count(&self) -> usize {
        self.received().len()
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Merges `extra`'s fields over `target`, dropping what they displace: the
/// shaping every mock document builder reads its caller's `extra` through.
///
/// # Panics
///
/// If either document is not a JSON `Object`, which only a caller that built
/// the wrong shape does.
fn merge(target: &mut Value, extra: Value) {
    let (Some(map), Value::Object(additions)) = (target.as_object_mut(), extra) else {
        panic!("both documents are JSON objects");
    };
    for (key, value) in additions {
        let _previous = map.insert(key, value);
    }
}
