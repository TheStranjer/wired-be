//! Tests of [`HttpClient`](super::HttpClient)'s request-sending methods:
//! the verbs, the body cap, timeouts, redirects, and the SSRF guard's
//! reach into every call. The methods themselves live in the parent's
//! one inherent `impl HttpClient` block — this crate allows a type only
//! a single implementation — so their coverage is this child's content.

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        routing::{any, get, post},
    };
    use bytes::Bytes;

    use crate::http_client::test_support::*;
    use crate::http_client::*;

    #[tokio::test]
    async fn get_sends_the_configured_user_agent() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client.get_limited(&url, &[], 1024).await.unwrap();

        assert_eq!(response.status, 200, "status");
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            TEST_UA,
            "the builder's User-Agent reaches the peer"
        );
        assert_eq!(
            response
                .headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; charset=utf-8"),
            "response headers are kept"
        );
        server.abort();
    }

    #[tokio::test]
    async fn request_headers_are_sent() {
        let listener = bind().await;
        let url = url_for(&listener, "/marker");
        let server = spawn_server(listener, Router::new().route("/marker", get(echo_marker)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .get_limited(&url, &[("x-marker", "yes")], 1024)
            .await
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "yes",
            "the extra header reaches the peer"
        );
        server.abort();
    }

    #[tokio::test]
    async fn body_over_the_limit_is_too_large_and_at_it_fits() {
        let listener = bind().await;
        let url = url_for(&listener, "/big");
        let server = spawn_server(listener, Router::new().route("/big", get(big)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let tight = client.get_limited(&url, &[], BIG_BODY.len() - 1).await;
        assert_eq!(
            tight.unwrap_err(),
            HttpError::TooLarge,
            "one byte over the cap fails"
        );

        let exact = client.get_limited(&url, &[], BIG_BODY.len()).await.unwrap();
        assert_eq!(exact.body.len(), BIG_BODY.len(), "exactly max_bytes fits");
        server.abort();
    }

    #[tokio::test]
    async fn a_stalled_response_times_out() {
        let listener = bind().await;
        let url = url_for(&listener, "/slow");
        let server = spawn_server(listener, Router::new().route("/slow", get(slow)));
        let client = HttpClient::with_timeout(TEST_UA, SHORT_TIMEOUT);

        let result = client.get_limited(&url, &[], 1024).await;

        assert_eq!(
            result.unwrap_err(),
            HttpError::Timeout,
            "the total timeout fires while the peer stalls"
        );
        server.abort();
    }

    #[tokio::test]
    async fn non_http_urls_are_rejected_before_any_connection() {
        let client = HttpClient::new_for_tests(TEST_UA);
        for url in [
            "ftp://example.com/pub",
            "file:///etc/passwd",
            "not a url",
            "https://",
            "",
        ] {
            let result = client.get_limited(url, &[], 1024).await;
            assert_eq!(
                result.unwrap_err(),
                HttpError::InvalidUrl,
                "{url} must not be fetched"
            );
        }
    }

    #[tokio::test]
    async fn loopback_literal_is_blocked_by_default_and_allowed_with_the_flag() {
        let listener = bind().await;
        let url = url_for(&listener, "/ua");
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));

        let guarded = HttpClient::new(TEST_UA);
        let blocked = guarded.get_limited(&url, &[], 1024).await;
        match blocked {
            Err(HttpError::Blocked(reason)) => {
                assert!(
                    reason.contains("127.0.0.1"),
                    "the reason names the refused address: {reason}"
                );
            }
            other => panic!("a loopback literal must be Blocked, got {other:?}"),
        }

        // The same client with `HTTP_ALLOW_PRIVATE_HOSTS` semantics.
        let open = HttpClient::new_for_tests(TEST_UA);
        let allowed = open.get_limited(&url, &[], 1024).await.unwrap();
        assert_eq!(allowed.status, 200, "the flag reaches the local server");
        server.abort();
    }

    #[tokio::test]
    async fn localhost_hostname_is_blocked_before_connecting() {
        let listener = bind().await;
        let port = listener.local_addr().unwrap().port();
        let server = spawn_server(listener, Router::new().route("/ua", get(echo_user_agent)));
        let client = HttpClient::new(TEST_UA);

        let result = client
            .get_limited(&format!("http://localhost:{port}/ua"), &[], 1024)
            .await;

        match result {
            Err(HttpError::Blocked(reason)) => {
                assert!(
                    reason.contains("127.0.0.1") || reason.contains("::1"),
                    "the reason names the resolved loopback address: {reason}"
                );
            }
            other => panic!("`localhost` must resolve to a blocked address, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn post_bytes_is_blocked_by_the_guard_too() {
        let client = HttpClient::new(TEST_UA);
        let result = client
            .post_bytes("http://127.0.0.1:9/ingest", &[], b"x".to_vec())
            .await;
        assert!(
            matches!(result, Err(HttpError::Blocked(_))),
            "delivery POSTs pass the same guard, got {result:?}"
        );
    }

    #[tokio::test]
    async fn redirect_is_followed() {
        let listener = bind().await;
        let hop_url = url_for(&listener, "/hop");
        let target = url_for(&listener, "/final");
        let app = Router::new()
            .route("/hop", get(hop).with_state(target))
            .route("/final", get(redirect_target));
        let server = spawn_server(listener, app);
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .get_limited(&hop_url, &[], 1024)
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        assert_eq!(response.status, 200, "final status after the hop");
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "followed",
            "the client ended up at the target"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_redirect_chain_past_five_fails_as_connect() {
        let listener = bind().await;
        let loop_url = url_for(&listener, "/hop");
        let app = Router::new().route("/hop", get(hop).with_state(loop_url.clone()));
        let server = spawn_server(listener, app);
        let client = HttpClient::new_for_tests(TEST_UA);

        let Err(err) = client.get_limited(&loop_url, &[], 1024).await else {
            panic!("a client looping past its redirect limit must not get a response");
        };
        assert!(
            matches!(err, HttpError::Connect(_)),
            "exceeding the redirect limit is a transport failure"
        );
        server.abort();
    }

    #[tokio::test]
    async fn post_bytes_sends_the_body() {
        let listener = bind().await;
        let url = url_for(&listener, "/echo");
        let server = spawn_server(listener, Router::new().route("/echo", post(echo_body)));
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .post_bytes(&url, &[("x-marker", "pong")], b"activity".to_vec())
            .await
            .unwrap();

        assert_eq!(response.status, 200, "echo status");
        assert_eq!(
            response.body,
            Bytes::from_static(b"activity"),
            "the body round-trips"
        );
        server.abort();
    }

    #[tokio::test]
    async fn request_sends_an_extension_method_verbatim() {
        let listener = bind().await;
        let url = url_for(&listener, "/verb");
        let server = spawn_server(listener, Router::new().route("/verb", any(echo_method)));
        let client = HttpClient::new_for_tests(TEST_UA);
        // The exact construction the media cache purge path uses.
        let purge = Method::from_bytes(b"PURGE").unwrap();

        let response = client
            .request(purge, &url, &[("x-marker", "yes")])
            .await
            .unwrap();

        assert_eq!(response.status, 200, "peer answered");
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "PURGE",
            "the uncommon verb arrives as sent, headers included"
        );
        server.abort();
    }

    #[tokio::test]
    async fn request_passes_the_guard_to_uncommon_verbs_too() {
        let client = HttpClient::new(TEST_UA);
        let result = client
            .request(
                Method::from_bytes(b"PURGE").unwrap(),
                "http://127.0.0.1:9/purge",
                &[],
            )
            .await;
        assert!(
            matches!(result, Err(HttpError::Blocked(_))),
            "a PURGE to loopback must be blocked like any other verb, got {result:?}"
        );
    }
}
