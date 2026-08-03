//! HTTP surface for the browser client.
//!
//! Four routes: the login redirect, the page, its assets, and the WebSocket
//! upgrade. Everything except the login redirect requires a session cookie.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ws::WebSocketUpgrade, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tracing::info;

use crate::web::assets;
use crate::web::bridge::{self, BridgeOptions};
use crate::web::{SharedState, COOKIE_NAME};

pub(crate) fn router(state: SharedState) -> Router {
    Router::new()
        .route("/connect/{invite}", get(connect))
        .route("/", get(index))
        .route("/assets/{file}", get(asset))
        .route("/ws", get(upgrade))
        .with_state(state)
}

/// Redeems a connect link, sets the session cookie, and sends the browser to
/// the client. The invite never reaches JavaScript and nothing is rendered
/// here, so the page that follows carries no `Referer` holding the token.
async fn connect(State(state): State<SharedState>, Path(invite): Path<String>) -> Response {
    let session = match state.tokens().redeem_invite(&invite) {
        Ok(session) => session,
        Err(err) => {
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("could not start a browser session: {err}"),
            )
        }
    };

    let Some(session) = session else {
        return expired_page();
    };

    // Tokens are base64url and the origin is validated at startup, so this only
    // fails if one of those invariants breaks — never quietly redirect without
    // the cookie, or the browser lands on a 401 with no explanation.
    let Ok(cookie) = HeaderValue::from_str(&state.session_cookie(&session)) else {
        return error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not build a session cookie for this origin",
        );
    };

    info!("browser session started");
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie);
    (StatusCode::FOUND, [(header::LOCATION, "/")], headers).into_response()
}

async fn index(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if claim_session(&state, &headers).is_none() {
        return expired_page();
    }
    asset_response(assets::INDEX_HTML.as_bytes(), "text/html; charset=utf-8")
}

async fn asset(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    if claim_session(&state, &headers).is_none() {
        return expired_page();
    }
    match assets::lookup(&file) {
        Some((bytes, content_type)) => asset_response(bytes, content_type),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[derive(Debug, Default, Deserialize)]
struct UpgradeQuery {
    cols: Option<u16>,
    rows: Option<u16>,
}

async fn upgrade(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<UpgradeQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(guard) = claim_session(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "session expired").into_response();
    };

    let options = BridgeOptions::from_requested(query.cols, query.rows);
    upgrade.on_upgrade(move |socket| bridge::run(socket, guard, options))
}

fn claim_session(
    state: &Arc<crate::web::WebState>,
    headers: &HeaderMap,
) -> Option<crate::web::auth::SessionGuard> {
    let token = session_cookie(headers)?;
    state.tokens().claim_session(&token)
}

/// Extracts the herdr session cookie from a request's `Cookie` header.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == COOKIE_NAME)
        .map(|(_, token)| token.trim().to_string())
}

fn asset_response(bytes: &'static [u8], content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // Assets are versioned with the binary, and the page itself must
            // never be cached or a revoked browser would keep rendering it.
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from(bytes),
    )
        .into_response()
}

fn expired_page() -> Response {
    error_page(
        StatusCode::UNAUTHORIZED,
        "This link is no longer valid. Run `herdr web connect` for a new one.",
    )
}

fn error_page(status: StatusCode, message: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        assets::error_page(message),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_cookie(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let headers = headers_with_cookie("theme=dark; herdr_web=abc123; other=1");

        assert_eq!(session_cookie(&headers), Some("abc123".to_string()));
    }

    #[test]
    fn a_lone_session_cookie_is_found() {
        let headers = headers_with_cookie("herdr_web=abc123");

        assert_eq!(session_cookie(&headers), Some("abc123".to_string()));
    }

    #[test]
    fn cookies_that_merely_end_in_the_name_are_not_matched() {
        let headers = headers_with_cookie("not_herdr_web=abc123");

        assert_eq!(session_cookie(&headers), None);
    }

    #[test]
    fn requests_without_cookies_have_no_session() {
        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }
}
