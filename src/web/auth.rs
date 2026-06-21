//! Invite and session tokens for the browser client.
//!
//! Both token kinds live only in memory and only as SHA-256 digests. The digest
//! is the map key, so a lookup is the comparison and no separate constant-time
//! compare is needed. Nothing survives a server restart or a live handoff.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::api::schema::WebSessionInfo;

/// Raw entropy behind both token kinds.
const TOKEN_BYTES: usize = 32;

/// Bytes of the session digest exposed as its public handle. Twelve hex
/// characters stay short enough to retype into `herdr web disconnect`.
const SESSION_HANDLE_BYTES: usize = 6;

type TokenDigest = [u8; 32];

struct SessionRecord {
    id: String,
    created: SystemTime,
    /// Unix seconds of the last interaction, shared with live bridges so they
    /// can record activity without taking the store lock on every frame.
    last_seen: Arc<AtomicU64>,
    /// Fires once when the session is revoked. Live bridges hold a receiver, so
    /// the receiver count doubles as the live connection count.
    revoke: broadcast::Sender<()>,
}

/// Proof that a request carried a valid session cookie.
pub(crate) struct SessionGuard {
    pub(crate) id: String,
    last_seen: Arc<AtomicU64>,
    /// Resolves when the session is revoked, so the bridge can close.
    pub(crate) revoked: broadcast::Receiver<()>,
}

impl SessionGuard {
    /// Records that the browser is still there.
    ///
    /// Only HTTP requests pass through the token store, and a browser makes
    /// none once its socket is open, so without this a live session would
    /// report the age of its handshake forever.
    pub(crate) fn touch(&self) {
        self.last_seen.store(now_unix(), Ordering::Relaxed);
    }
}

pub(crate) struct TokenStore {
    invite_ttl: Duration,
    invites: HashMap<TokenDigest, Instant>,
    sessions: HashMap<TokenDigest, SessionRecord>,
}

impl TokenStore {
    pub(crate) fn new(invite_ttl: Duration) -> Self {
        Self {
            invite_ttl,
            invites: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub(crate) fn invite_ttl(&self) -> Duration {
        self.invite_ttl
    }

    /// Issues a single-use connect token.
    pub(crate) fn mint_invite(&mut self) -> io::Result<String> {
        self.purge_expired_invites(Instant::now());
        let token = mint_token()?;
        self.invites.insert(digest(&token), Instant::now());
        Ok(token)
    }

    /// Consumes an invite and returns a freshly minted session token.
    ///
    /// Returns `None` when the invite is unknown, expired, or already spent.
    pub(crate) fn redeem_invite(&mut self, invite: &str) -> io::Result<Option<String>> {
        let now = Instant::now();
        self.purge_expired_invites(now);
        if self.invites.remove(&digest(invite)).is_none() {
            return Ok(None);
        }

        let token = mint_token()?;
        let token_digest = digest(&token);
        self.sessions.insert(
            token_digest,
            SessionRecord {
                id: session_handle(&token_digest),
                created: SystemTime::now(),
                last_seen: Arc::new(AtomicU64::new(now_unix())),
                revoke: broadcast::channel(1).0,
            },
        );
        Ok(Some(token))
    }

    /// Validates a session token and marks it as recently seen.
    pub(crate) fn claim_session(&mut self, token: &str) -> Option<SessionGuard> {
        let record = self.sessions.get(&digest(token))?;
        let guard = SessionGuard {
            id: record.id.clone(),
            last_seen: record.last_seen.clone(),
            revoked: record.revoke.subscribe(),
        };
        guard.touch();
        Some(guard)
    }

    pub(crate) fn sessions(&self) -> Vec<WebSessionInfo> {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .map(|record| WebSessionInfo {
                id: record.id.clone(),
                created_unix: unix_seconds(record.created),
                last_seen_unix: record.last_seen.load(Ordering::Relaxed),
                connections: record.revoke.receiver_count() as u32,
            })
            .collect();
        sessions.sort_by(|a, b| a.created_unix.cmp(&b.created_unix).then(a.id.cmp(&b.id)));
        sessions
    }

    /// Revokes a session by its public handle and closes its live connections.
    ///
    /// Returns `None` when no session carries that handle.
    pub(crate) fn revoke(&mut self, id: &str) -> Option<u32> {
        let key = *self
            .sessions
            .iter()
            .find(|(_, record)| record.id == id)
            .map(|(key, _)| key)?;
        let record = self.sessions.remove(&key)?;
        let closed = record.revoke.receiver_count() as u32;
        let _ = record.revoke.send(());
        Some(closed)
    }

    fn purge_expired_invites(&mut self, now: Instant) {
        let ttl = self.invite_ttl;
        self.invites
            .retain(|_, created| now.duration_since(*created) < ttl);
    }
}

fn mint_token() -> io::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|err| io::Error::other(format!("failed to read system entropy: {err}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn digest(token: &str) -> TokenDigest {
    Sha256::digest(token.as_bytes()).into()
}

fn session_handle(token_digest: &TokenDigest) -> String {
    token_digest[..SESSION_HANDLE_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_unix() -> u64 {
    unix_seconds(SystemTime::now())
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TokenStore {
        TokenStore::new(Duration::from_secs(300))
    }

    #[test]
    fn minted_invites_are_unique_and_url_safe() {
        let mut store = store();
        let first = store.mint_invite().unwrap();
        let second = store.mint_invite().unwrap();

        assert_ne!(first, second);
        for token in [&first, &second] {
            assert!(
                token
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
                "token is not url-safe: {token}"
            );
            assert!(token.len() >= 43, "token is too short: {token}");
        }
    }

    #[test]
    fn redeeming_an_invite_issues_a_session() {
        let mut store = store();
        let invite = store.mint_invite().unwrap();

        let session = store.redeem_invite(&invite).unwrap().unwrap();

        assert_ne!(session, invite);
        assert!(store.claim_session(&session).is_some());
    }

    #[test]
    fn an_invite_cannot_be_redeemed_twice() {
        let mut store = store();
        let invite = store.mint_invite().unwrap();

        assert!(store.redeem_invite(&invite).unwrap().is_some());
        assert!(store.redeem_invite(&invite).unwrap().is_none());
    }

    #[test]
    fn unknown_invites_and_sessions_are_rejected() {
        let mut store = store();

        assert!(store.redeem_invite("not-a-real-invite").unwrap().is_none());
        assert!(store.claim_session("not-a-real-session").is_none());
    }

    #[test]
    fn expired_invites_are_rejected() {
        let mut store = TokenStore::new(Duration::ZERO);
        let invite = store.mint_invite().unwrap();

        assert!(store.redeem_invite(&invite).unwrap().is_none());
    }

    #[test]
    fn sessions_report_their_public_handle_not_their_token() {
        let mut store = store();
        let invite = store.mint_invite().unwrap();
        let session = store.redeem_invite(&invite).unwrap().unwrap();

        let listed = store.sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.len(), SESSION_HANDLE_BYTES * 2);
        assert!(!session.contains(&listed[0].id));
        assert_eq!(listed[0].connections, 0);
    }

    #[test]
    fn claimed_sessions_count_as_connections_until_dropped() {
        let mut store = store();
        let invite = store.mint_invite().unwrap();
        let session = store.redeem_invite(&invite).unwrap().unwrap();

        let guard = store.claim_session(&session).unwrap();
        assert_eq!(store.sessions()[0].connections, 1);

        drop(guard);
        assert_eq!(store.sessions()[0].connections, 0);
    }

    #[test]
    fn revoking_closes_live_connections_and_invalidates_the_token() {
        let mut store = store();
        let invite = store.mint_invite().unwrap();
        let session = store.redeem_invite(&invite).unwrap().unwrap();
        let mut guard = store.claim_session(&session).unwrap();
        let id = store.sessions()[0].id.clone();

        assert_eq!(store.revoke(&id), Some(1));

        assert!(guard.revoked.try_recv().is_ok());
        assert!(store.claim_session(&session).is_none());
        assert!(store.sessions().is_empty());
    }

    #[test]
    fn revoking_an_unknown_handle_reports_nothing() {
        let mut store = store();

        assert_eq!(store.revoke("deadbeefcafe"), None);
    }

    #[test]
    fn revoking_one_session_leaves_the_others_alone() {
        let mut store = store();
        let first_invite = store.mint_invite().unwrap();
        let second_invite = store.mint_invite().unwrap();
        let first = store.redeem_invite(&first_invite).unwrap().unwrap();
        let second = store.redeem_invite(&second_invite).unwrap().unwrap();
        let first_id = store.claim_session(&first).unwrap().id;

        assert_eq!(store.revoke(&first_id), Some(0));

        assert!(store.claim_session(&first).is_none());
        assert!(store.claim_session(&second).is_some());
        assert_eq!(store.sessions().len(), 1);
    }
}
