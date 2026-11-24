//! Loopback HTTP servers for tests that need a peer to talk to.
//!
//! A test binds an unused port with [`bind`], reads the address the peer will
//! answer on with [`origin`] (so the routes can embed it), and then serves its
//! [`Router`] with [`spawn`].

use axum::Router;
use tokio::{net::TcpListener, task::JoinHandle};

/// Binds an unused loopback port.
///
/// # Panics
///
/// Panics when no loopback port can be bound.
pub async fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// The `http://{addr}` origin a listener answers on.
///
/// # Panics
///
/// Panics when the listener has no local address.
pub fn origin(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

/// Serves `app` on `listener` until the test aborts the task.
pub fn spawn(listener: TcpListener, app: Router) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test server stopped: {err}");
        }
    })
}

#[cfg(test)]
mod tests {
    use axum::routing::get;

    use super::*;

    #[tokio::test]
    async fn origin_names_the_bound_loopback_port() {
        let listener = bind().await;
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0);
        assert_eq!(
            origin(&listener),
            format!("http://127.0.0.1:{}", addr.port())
        );
    }

    #[tokio::test]
    async fn spawn_serves_the_router_until_aborted() {
        let listener = bind().await;
        let base = origin(&listener);
        let server = spawn(
            listener,
            Router::new().route("/ping", get(|| async { "pong" })),
        );

        let body = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            reqwest::get(format!("{base}/ping"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(body, "pong");

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
