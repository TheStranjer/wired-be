//! The signed wire: the delivery tests that run against the verifying mock
//! inbox — what [`handle`](crate::federation::delivery::handle) carries
//! (the signature, the stripped body, the header set) and what a peer that
//! authenticates the delivery decides. Split from the
//! [`send`](crate::federation::delivery::send) tests so no file outgrows
//! its length budget.

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use serde_json::Value;

    use crate::federation::{
        delivery::{
            ACTIVITY_JSON,
            test_support::{
                ACTOR_KEY_ID, SIGNED_LINES, actor, attempt_to, first_attempt, header_text,
                signature_param, test_state, verifying_inbox,
            },
        },
        signature::digest_header,
    };

    #[tokio::test]
    async fn a_signed_delivery_verifies_at_the_peer_and_completes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, observable, server) = verifying_inbox().await;

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        assert_eq!(
            outcome,
            Ok(()),
            "a delivery the peer authenticates completes"
        );
        assert_eq!(
            observable.hits.load(Ordering::SeqCst),
            1,
            "the peer was asked exactly once"
        );
        let headers = observable.headers.lock().unwrap().clone().unwrap();
        let signature = header_text(&headers, "signature");
        assert_eq!(
            signature_param(&signature, "keyId"),
            ACTOR_KEY_ID,
            "the signature names the actor's own key"
        );
        assert_eq!(
            signature_param(&signature, "headers"),
            SIGNED_LINES,
            "a delivery signs the four canonical lines"
        );
        let raw = BASE64
            .decode(signature_param(&signature, "signature"))
            .unwrap();
        assert_eq!(raw.len(), 128, "an RSA-1024 signature is 128 bytes");
    }

    #[tokio::test]
    async fn blind_recipients_never_reach_the_peer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, observable, server) = verifying_inbox().await;

        attempt_to(&state, &inbox, alice.id, first_attempt())
            .await
            .unwrap();
        server.abort();

        let body = observable.body.lock().unwrap().clone().unwrap();
        let sent: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            sent.get("bto").is_none() && sent.get("bcc").is_none(),
            "the blind recipient lists were stripped: {sent}"
        );
        assert!(sent.get("to").is_some(), "the public addressees ride along");
        let headers = observable.headers.lock().unwrap().clone().unwrap();
        assert_eq!(
            header_text(&headers, "digest"),
            digest_header(&body),
            "the Digest header describes exactly the bytes that were sent"
        );
    }

    #[tokio::test]
    async fn the_delivery_carries_every_inbox_header() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, observable, server) = verifying_inbox().await;

        attempt_to(&state, &inbox, alice.id, first_attempt())
            .await
            .unwrap();
        server.abort();

        let headers = observable.headers.lock().unwrap().clone().unwrap();
        assert_eq!(
            header_text(&headers, "content-type"),
            ACTIVITY_JSON,
            "the body is labelled an activity"
        );
        assert_eq!(
            header_text(&headers, "accept"),
            ACTIVITY_JSON,
            "and the answer is asked for in the same media type"
        );
        assert!(
            header_text(&headers, "date").ends_with(" GMT"),
            "a signed Date header"
        );
        assert!(
            header_text(&headers, "digest").starts_with("SHA-256="),
            "a SHA-256 Digest header"
        );
        assert!(
            !header_text(&headers, "signature").is_empty(),
            "a Signature header"
        );
        assert!(
            !header_text(&headers, "host").is_empty(),
            "the client fills Host, which is why it is not sent by hand"
        );
    }
}
