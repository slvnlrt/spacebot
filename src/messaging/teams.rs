//! Microsoft Teams messaging adapter (Bot Framework).
//!
//! This module provides the outbound Azure AD token provider used to mint and
//! cache Bot Connector bearer tokens for proactive messaging and replies.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How long before the token actually expires we consider it stale and refresh
/// proactively.  5 minutes matches common Azure AD guidance.
const REFRESH_LEEWAY: Duration = Duration::from_secs(300);

/// Azure AD token endpoint template — `{tenant}` is replaced at runtime.
const TOKEN_ENDPOINT: &str =
    "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token";

/// The scope required to call the Bot Connector service.
const BOT_FRAMEWORK_SCOPE: &str = "https://api.botframework.com/.default";

// ---------------------------------------------------------------------------
// Internal cache entry
// ---------------------------------------------------------------------------

struct CachedToken {
    /// The opaque bearer token string.
    token: String,
    /// Absolute instant at which this token expires.
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// Token response from Azure AD
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Lifetime of the token in seconds.
    expires_in: u64,
}

// ---------------------------------------------------------------------------
// TeamsTokenProvider
// ---------------------------------------------------------------------------

/// Mints and caches an Azure AD client-credentials bearer token for the Bot
/// Connector service.
///
/// Concurrent callers that race on an expired token all block on the same
/// mutex acquisition; whichever wins refreshes once and all subsequent waiters
/// read the freshly cached value.  This deliberately holds the mutex across
/// the await so there is only ever one in-flight refresh.
///
/// **Secrets policy:** `client_secret` and the returned token are NEVER
/// written to tracing spans, log fields, or error messages.
pub struct TeamsTokenProvider {
    tenant_id: String,
    app_id: String,
    /// The client secret is stored in memory but must never be logged.
    client_secret: String,
    http: Client,
    cached: Mutex<Option<CachedToken>>,
}

impl TeamsTokenProvider {
    /// Construct a new provider from credentials.
    pub fn new(tenant_id: String, app_id: String, client_secret: String) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client for Teams token provider")?;

        Ok(Self {
            tenant_id,
            app_id,
            client_secret,
            http,
            cached: Mutex::new(None),
        })
    }

    /// Return a valid Bot Connector bearer token, refreshing from Azure AD if
    /// necessary.
    ///
    /// The mutex is held across the network call so that concurrent callers
    /// coalesce onto a single refresh rather than stampeding the endpoint.
    pub async fn bearer(&self) -> anyhow::Result<String> {
        let mut guard = self.cached.lock().await;

        let now = Instant::now();

        // Return the cached token if it is still comfortably valid.
        if let Some(ref cached) = *guard {
            if !needs_refresh(cached.expires_at, now, REFRESH_LEEWAY) {
                return Ok(cached.token.clone());
            }
        }

        // Cache is empty or stale — fetch a new token.
        tracing::debug!(
            tenant_id = %self.tenant_id,
            app_id = %self.app_id,
            "refreshing Azure AD token for Bot Connector",
        );

        let endpoint = TOKEN_ENDPOINT.replace("{tenant}", &self.tenant_id);

        let response = self
            .http
            .post(&endpoint)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.app_id),
                ("client_secret", &self.client_secret),
                ("scope", BOT_FRAMEWORK_SCOPE),
            ])
            .send()
            .await
            .context("Azure AD token request failed")?;

        let status = response.status();
        if !status.is_success() {
            // Read the body for diagnostics but do NOT include secrets.
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            anyhow::bail!(
                "Azure AD token endpoint returned {status}: {body}"
            );
        }

        let token_resp: TokenResponse = response
            .json()
            .await
            .context("failed to deserialise Azure AD token response")?;

        let expires_at = now + Duration::from_secs(token_resp.expires_in);

        tracing::info!(
            tenant_id = %self.tenant_id,
            app_id = %self.app_id,
            expires_in_secs = token_resp.expires_in,
            "Azure AD Bot Connector token refreshed",
        );

        *guard = Some(CachedToken {
            token: token_resp.access_token.clone(),
            expires_at,
        });

        Ok(token_resp.access_token)
    }
}

// ---------------------------------------------------------------------------
// Pure helper — separated for unit-testability
// ---------------------------------------------------------------------------

/// Return `true` if the cached token should be refreshed.
///
/// Refreshing is needed when `now` is at or past `expires_at - leeway`.
///
/// # Arguments
///
/// * `expires_at` – the absolute instant the token expires.
/// * `now`        – the current instant (injectable for tests).
/// * `leeway`     – how far before expiry we start treating the token as
///   stale (typically [`REFRESH_LEEWAY`]).
#[inline]
pub fn needs_refresh(expires_at: Instant, now: Instant, leeway: Duration) -> bool {
    // If expires_at < leeway we clamp to Instant::ZERO to avoid panics on
    // saturating subtraction.
    let refresh_after = expires_at.checked_sub(leeway).unwrap_or(expires_at);
    now >= refresh_after
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const LEEWAY: Duration = REFRESH_LEEWAY;

    /// Well before expiry: token is fresh, no refresh needed.
    #[test]
    fn test_needs_refresh_fresh_token() {
        let now = Instant::now();
        // Token expires 1 hour from now; leeway is 5 min → still fresh.
        let expires_at = now + Duration::from_secs(3600);
        assert!(!needs_refresh(expires_at, now, LEEWAY));
    }

    /// Exactly at the refresh boundary (now == expires_at - leeway): refresh.
    #[test]
    fn test_needs_refresh_at_boundary() {
        let now = Instant::now();
        // Token expires exactly `leeway` seconds from now.
        let expires_at = now + LEEWAY;
        assert!(needs_refresh(expires_at, now, LEEWAY));
    }

    /// Inside the leeway window (expires in 2 min, leeway 5 min): refresh.
    #[test]
    fn test_needs_refresh_inside_leeway() {
        let now = Instant::now();
        let expires_at = now + Duration::from_secs(120); // 2 min < 5 min leeway
        assert!(needs_refresh(expires_at, now, LEEWAY));
    }

    /// Token already expired: must refresh.
    #[test]
    fn test_needs_refresh_expired() {
        let now = Instant::now();
        // `expires_at` is in the past.
        let expires_at = now - Duration::from_secs(1);
        assert!(needs_refresh(expires_at, now, LEEWAY));
    }

    /// Token still has leeway + 1 second left: not yet time to refresh.
    #[test]
    fn test_needs_refresh_just_before_boundary() {
        let now = Instant::now();
        // Expires in leeway + 1 s → still fresh by exactly 1 second.
        let expires_at = now + LEEWAY + Duration::from_secs(1);
        assert!(!needs_refresh(expires_at, now, LEEWAY));
    }
}
