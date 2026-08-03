use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WebConnectParams {
    /// Externally reachable origin to build the connect link from, overriding
    /// the `[web] public_url` config value for this link only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WebDisconnectParams {
    /// Session handle reported by `web.sessions`.
    pub session: String,
}

/// A browser session holding a valid cookie, whether or not it is connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WebSessionInfo {
    /// Short public handle for this session. Never the session token.
    pub id: String,
    pub created_unix: u64,
    pub last_seen_unix: u64,
    /// Live browser connections currently attached under this session.
    pub connections: u32,
}
