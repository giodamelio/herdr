//! Browser client for the herdr TUI.
//!
//! The server renders to already-diffed terminal ANSI for network clients, so
//! a browser only needs a terminal emulator and a socket. This module binds an
//! HTTP listener, gates it behind single-use connect links, and bridges each
//! WebSocket to a normal herdr client connection.
//!
//! Off unless `[web] enabled` is set. Tokens live in memory only, so every
//! session ends with the server process.

mod assets;
mod auth;
mod bridge;
mod http;
mod wire;

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::api::schema::WebSessionInfo;
use crate::config::WebConfig;
use auth::TokenStore;

/// Name of the session cookie. Browsers cannot set headers on a `WebSocket`
/// constructor, so the cookie is what authenticates the upgrade.
const COOKIE_NAME: &str = "herdr_web";

type SharedState = Arc<WebState>;

pub(crate) struct WebState {
    tokens: Mutex<TokenStore>,
    /// Origin the connect link is built from, without a trailing slash.
    public_url: String,
    /// Whether that origin is https, and so whether cookies may be `Secure`.
    secure: bool,
}

impl WebState {
    /// Locks the token store.
    ///
    /// The lock is only ever held for a map lookup, never across an await, so a
    /// blocking mutex is both correct and cheaper than an async one.
    fn tokens(&self) -> std::sync::MutexGuard<'_, TokenStore> {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Builds the session cookie for a freshly redeemed invite.
    ///
    /// No `Max-Age`, so it survives a reload and a tab close but dies when the
    /// browser quits. `Secure` is only set over https, because a browser
    /// silently drops a `Secure` cookie sent over plain http and the login
    /// would fail with nothing to show for it.
    fn session_cookie(&self, token: &str) -> String {
        let mut cookie = format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict");
        if self.secure {
            cookie.push_str("; Secure");
        }
        cookie
    }
}

/// A bound listener and the sessions it serves.
pub(crate) struct WebRuntime {
    state: SharedState,
    bind: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl WebRuntime {
    /// Binds the listener and starts serving.
    ///
    /// Once bound it stays bound for the life of the server: a browser holding
    /// a valid cookie needs something to present it to, and nothing on the
    /// browser side could wake a listener that had gone away.
    ///
    /// Must be called from inside the server's tokio runtime, which is where
    /// the listener is registered and the serve task is spawned.
    pub(crate) fn start(config: &WebConfig) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind(&config.bind).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to bind the web listener to {}: {err}", config.bind),
            )
        })?;
        listener.set_nonblocking(true)?;
        let bind = listener.local_addr()?;
        let listener = tokio::net::TcpListener::from_std(listener)?;

        let public_url = resolve_public_url(&config.public_url, bind);
        let state = Arc::new(WebState {
            tokens: Mutex::new(TokenStore::new(Duration::from_secs(config.invite_ttl_secs))),
            secure: public_url.starts_with("https://"),
            public_url,
        });

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let router = http::router(state.clone());
        tokio::spawn(async move {
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(err) = served {
                warn!(err = %err, "web listener stopped");
            }
        });

        info!(%bind, "web listener bound");
        Ok(Self {
            state,
            bind,
            shutdown: Some(shutdown_tx),
        })
    }

    pub(crate) fn bind(&self) -> SocketAddr {
        self.bind
    }

    /// Issues a fresh connect link. Every call yields a new single-use invite.
    pub(crate) fn connect_link(
        &self,
        public_url_override: Option<&str>,
    ) -> io::Result<(String, u64)> {
        let mut tokens = self.state.tokens();
        let invite = tokens.mint_invite()?;
        let base = public_url_override
            .map(normalize_public_url)
            .unwrap_or_else(|| self.state.public_url.clone());
        Ok((
            format!("{base}/connect/{invite}"),
            tokens.invite_ttl().as_secs(),
        ))
    }

    pub(crate) fn sessions(&self) -> Vec<WebSessionInfo> {
        self.state.tokens().sessions()
    }

    /// Revokes a session and closes its live connections.
    pub(crate) fn disconnect(&self, session: &str) -> Option<u32> {
        self.state.tokens().revoke(session)
    }
}

impl Drop for WebRuntime {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Chooses the origin connect links are built from.
///
/// TLS terminates outside herdr, so the externally reachable name has to be
/// configured; the bound address is only a usable fallback on loopback.
fn resolve_public_url(configured: &str, bind: SocketAddr) -> String {
    let configured = configured.trim();
    if configured.is_empty() {
        return format!("http://{bind}");
    }
    normalize_public_url(configured)
}

fn normalize_public_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.contains("://") {
        return url.to_string();
    }
    format!("http://{url}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(public_url: &str) -> WebState {
        WebState {
            tokens: Mutex::new(TokenStore::new(Duration::from_secs(300))),
            secure: public_url.starts_with("https://"),
            public_url: public_url.to_string(),
        }
    }

    #[test]
    fn an_empty_public_url_falls_back_to_the_bound_address() {
        let bind: SocketAddr = "127.0.0.1:7777".parse().unwrap();

        assert_eq!(resolve_public_url("", bind), "http://127.0.0.1:7777");
    }

    #[test]
    fn a_configured_public_url_wins_and_loses_its_trailing_slash() {
        let bind: SocketAddr = "127.0.0.1:7777".parse().unwrap();

        assert_eq!(
            resolve_public_url("https://box.tail1234.ts.net/", bind),
            "https://box.tail1234.ts.net"
        );
    }

    #[test]
    fn a_public_url_without_a_scheme_is_assumed_plain_http() {
        let bind: SocketAddr = "127.0.0.1:7777".parse().unwrap();

        assert_eq!(
            resolve_public_url("box.local:8080", bind),
            "http://box.local:8080"
        );
    }

    #[test]
    fn https_origins_mark_the_cookie_secure() {
        let cookie = state("https://box.tail1234.ts.net").session_cookie("token");

        assert!(cookie.starts_with("herdr_web=token; "));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Secure"));
        assert!(!cookie.contains("Max-Age"));
    }

    #[test]
    fn plain_http_origins_omit_secure_so_the_cookie_is_not_dropped() {
        let cookie = state("http://127.0.0.1:7777").session_cookie("token");

        assert!(cookie.contains("HttpOnly"));
        assert!(!cookie.contains("Secure"));
    }
}
