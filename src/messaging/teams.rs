//! Microsoft Teams messaging adapter (Bot Framework).
//!
//! This module provides:
//! - Outbound Azure AD token provider (mint/cache Bot Connector bearer tokens).
//! - Inbound JWT validator: verifies that incoming POST /api/messages requests
//!   are signed by Azure Bot Service, preventing message injection by third
//!   parties.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use jsonwebtoken::{
    Algorithm, DecodingKey, Header, Validation, decode, decode_header,
    jwk::{JwkSet},
};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

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
// Inbound JWT validation constants
// ---------------------------------------------------------------------------

/// The OpenID Connect metadata endpoint for Azure Bot Framework.
/// Source: https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-connector-authentication
/// (Connector to Bot authentication — protocol v3.1 & v3.2)
const BOT_FRAMEWORK_OPENID_CONFIG: &str =
    "https://login.botframework.com/v1/.well-known/openidconfiguration";

/// The issuer claim that Azure Bot Service includes in tokens it sends to bots.
/// Source: MS docs table "JWT Issuer" under "Connector to Bot authentication".
const BOT_FRAMEWORK_ISSUER: &str = "https://api.botframework.com";

/// JWKS cache is considered stale after 24 h (per Microsoft guidance).
const JWKS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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
// JWKS cache (inbound JWT validation)
// ---------------------------------------------------------------------------

/// OpenID Connect configuration document (subset of fields we care about).
#[derive(Deserialize)]
struct OpenIdConfig {
    jwks_uri: String,
}

/// Cached JWKS state.
struct JwksCacheInner {
    /// The parsed JWK set fetched from `jwks_uri`.
    keyset: JwkSet,
    /// Wall-clock time at which this cache entry was populated.
    fetched_at: std::time::SystemTime,
}

/// Thread-safe JWKS cache that fetches keys from the Bot Framework OpenID
/// endpoint and refreshes:
/// - automatically when the cached copy is older than 24 hours, and
/// - on-demand when a `kid` is not found (refresh-once-on-unknown-kid).
///
/// Constructed once and shared (via `Arc`) across request handlers.
pub struct JwksCache {
    http: Client,
    inner: RwLock<Option<JwksCacheInner>>,
}

impl JwksCache {
    /// Create a new, empty cache.  Keys are fetched lazily on first use.
    pub fn new() -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client for JWKS cache")?;
        Ok(Self {
            http,
            inner: RwLock::new(None),
        })
    }

    /// Return the current JWK set, refreshing if stale (> 24 h) or absent.
    ///
    /// Uses a write lock only when a refresh is actually required; read-side
    /// accesses are concurrent.
    pub async fn keyset(&self) -> anyhow::Result<Arc<JwkSet>> {
        // Fast path: read lock.
        {
            let guard = self.inner.read().await;
            if let Some(ref cached) = *guard {
                if !Self::is_stale(&cached.fetched_at) {
                    return Ok(Arc::new(cached.keyset.clone()));
                }
            }
        }

        // Slow path: write lock — fetch fresh keys.
        let mut guard = self.inner.write().await;
        // Double-check: another waiter may have refreshed while we waited.
        if let Some(ref cached) = *guard {
            if !Self::is_stale(&cached.fetched_at) {
                return Ok(Arc::new(cached.keyset.clone()));
            }
        }

        let keyset = Self::fetch_keyset(&self.http).await?;
        let fetched_at = SystemTime::now();
        *guard = Some(JwksCacheInner { keyset: keyset.clone(), fetched_at });
        Ok(Arc::new(keyset))
    }

    /// Find a key by `kid`, refreshing once if not found.
    ///
    /// Returns `Err` if the key is still absent after one refresh.
    pub async fn find_key(&self, kid: &str) -> anyhow::Result<DecodingKey> {
        let keyset = self.keyset().await?;
        if let Some(jwk) = keyset.find(kid) {
            return DecodingKey::from_jwk(jwk)
                .context("failed to build DecodingKey from JWK");
        }

        // Refresh once on unknown kid (key rotation).
        tracing::debug!(kid, "kid not found in JWKS cache; forcing refresh");
        self.force_refresh().await?;
        let keyset = self.keyset().await?;
        let jwk = keyset
            .find(kid)
            .with_context(|| format!("kid '{kid}' not found in JWKS even after refresh"))?;
        DecodingKey::from_jwk(jwk).context("failed to build DecodingKey from JWK after refresh")
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn is_stale(fetched_at: &SystemTime) -> bool {
        fetched_at
            .elapsed()
            .map(|age| age >= JWKS_TTL)
            .unwrap_or(true)
    }

    async fn force_refresh(&self) -> anyhow::Result<()> {
        let mut guard = self.inner.write().await;
        let keyset = Self::fetch_keyset(&self.http).await?;
        *guard = Some(JwksCacheInner {
            keyset,
            fetched_at: SystemTime::now(),
        });
        Ok(())
    }

    async fn fetch_keyset(http: &Client) -> anyhow::Result<JwkSet> {
        // Step 1: Get the OpenID configuration to find `jwks_uri`.
        let config: OpenIdConfig = http
            .get(BOT_FRAMEWORK_OPENID_CONFIG)
            .send()
            .await
            .context("failed to fetch Bot Framework OpenID config")?
            .error_for_status()
            .context("Bot Framework OpenID config returned error status")?
            .json()
            .await
            .context("failed to parse Bot Framework OpenID config")?;

        tracing::debug!(jwks_uri = %config.jwks_uri, "fetched Bot Framework OpenID config");

        // Step 2: Fetch the JWKS from the URI advertised in the config.
        let keyset: JwkSet = http
            .get(&config.jwks_uri)
            .send()
            .await
            .context("failed to fetch Bot Framework JWKS")?
            .error_for_status()
            .context("Bot Framework JWKS endpoint returned error status")?
            .json()
            .await
            .context("failed to parse Bot Framework JWKS")?;

        tracing::info!(
            key_count = keyset.keys.len(),
            "refreshed Bot Framework JWKS signing keys",
        );

        Ok(keyset)
    }
}

// ---------------------------------------------------------------------------
// Inbound JWT validation
// ---------------------------------------------------------------------------

/// Validate a signed JWT token sent by Azure Bot Service to our webhook.
///
/// This is the security gate on `/api/messages`.  It rejects any request whose
/// `Authorization` header is absent, malformed, signed with a wrong key, has
/// an incorrect issuer/audience, or is expired.
///
/// # Arguments
///
/// * `auth_header` – the raw value of the `Authorization` HTTP header.
/// * `app_id`      – our Microsoft App ID (used as the expected `aud` claim).
/// * `jwks`        – the shared JWKS cache.
///
/// # Errors
///
/// Returns `Err` for ANY validation failure.  The error messages are safe to
/// log but should NOT be returned verbatim in HTTP responses (avoid leaking
/// token fragments).
pub async fn validate_inbound_jwt(
    auth_header: &str,
    app_id: &str,
    jwks: &JwksCache,
) -> anyhow::Result<()> {
    // Strip "Bearer " prefix.
    let token = auth_header
        .strip_prefix("Bearer ")
        .with_context(|| "Authorization header is not a Bearer token")?;

    if token.is_empty() {
        anyhow::bail!("Authorization header contains an empty Bearer token");
    }

    // Decode the header only (no signature check) to get the `kid`.
    let header: Header = decode_header(token)
        .context("failed to decode JWT header")?;

    let kid = header
        .kid
        .as_deref()
        .context("JWT header missing 'kid' field")?;

    // Look up the signing key (with lazy refresh on unknown kid).
    let decoding_key = jwks
        .find_key(kid)
        .await
        .context("JWT signing key not found")?;

    validate_token_with_key(token, app_id, BOT_FRAMEWORK_ISSUER, &decoding_key)
}

/// Pure, injectable token validator — separated so tests can inject a
/// `DecodingKey` derived from a locally-generated test RSA keypair without
/// hitting any network.
///
/// # Arguments
///
/// * `token`        – raw JWT string (no "Bearer " prefix).
/// * `expected_aud` – the audience value that must appear in the token's `aud`
///   claim (our bot App ID).
/// * `expected_iss` – the issuer value that must appear in the token's `iss`
///   claim.
/// * `key`          – the `DecodingKey` to verify the signature with.
///
/// # Security invariants
///
/// - Only `RS256` is accepted; `alg: none`, `HS256`, and all other algorithms
///   are rejected at the `Validation` level — jsonwebtoken refuses any token
///   whose `alg` header does not match the allowed list.
/// - `validate_aud` is **always** `true`.
/// - `validate_exp` is **always** `true` (the default).
/// - Leeway is 300 s (5 min), matching Microsoft's industry-standard guidance.
/// - `exp`, `aud`, and `iss` are all **required** claims — a token that omits
///   any of them is rejected outright, regardless of signature validity.  This
///   prevents cross-bot replay attacks using tokens that simply lack an `aud`.
pub fn validate_token_with_key(
    token: &str,
    expected_aud: &str,
    expected_iss: &str,
    key: &DecodingKey,
) -> anyhow::Result<()> {
    let mut validation = Validation::new(Algorithm::RS256);

    // Audience: must equal our app ID.
    validation.set_audience(&[expected_aud]);
    validation.validate_aud = true;

    // Issuer: must equal the Bot Framework issuer.
    validation.set_issuer(&[expected_iss]);

    // Expiry: validated by default; allow 5 min clock skew.
    validation.validate_exp = true;
    validation.leeway = 300;

    // Require exp, aud, and iss to be present in the token.  jsonwebtoken only
    // *validates* claims that exist; without this, a token that simply omits
    // `aud` or `iss` would pass the audience/issuer checks entirely.
    validation.set_required_spec_claims(&["exp", "aud", "iss"]);

    // Decode and verify in one step.
    decode::<serde_json::Value>(token, key, &validation)
        .context("JWT validation failed")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, UNIX_EPOCH};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use serde_json::json;

    const LEEWAY: Duration = REFRESH_LEEWAY;

    // -----------------------------------------------------------------------
    // JWT test helpers
    // -----------------------------------------------------------------------

    /// RSA-2048 private key (PKCS#8 PEM) generated for tests only.
    /// This key is NOT used in production.
    const TEST_RSA_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDH2tXs+GABUCDT
URh3/iL7/5zZP3yXNThRzAFNiJiJ7Zt/RUE9SSLhD6UuoHsOAMeyKhP7AoodMinG
npsQXEV9R0JoCH2jISo8xV/BKRNLxdKCcOZFpye7e9mNnMvOETO2KEhUEgAcySDu
kMBn8WTyFUYrQNj/+ih0W4UbEaMVnJiiEWmq+yj0xo6DYedxEYIVrGVB+RC94RTo
fPE/UzRL1fnhid4X8RaG9vVhaSWUDX7b6LCsI73KB9yupiqMMBtW2hRU0L9UKE09
aBoahn5EoCHQnb/3/cVbBd4MpkuzGbLmQV6Pf3SsL8/yESgewGbLhZvp1lz4vDI3
4iuQ81zvAgMBAAECggEAGRxvdbNtiKyzOyn825LUfXpMEGXwNyWKOojZ/w5zMB1p
RNAEVvl6BvJKzHWAkK1bahDsasUSaoGziw/BpwgY+Rk7iEvM0XLo1jLsiZ4qHQKx
pQ8fd8/9Z4qztp3lY7J4n2InWFzco8FHwIHykvzbNKmko+mlemBJtfkL2+9W4O+P
r87SHfStVoFzOQo4hv8pTgCR6+ZFTcgEtCDvB3FM8sgO6+hYJR4TXeSL3lbjk1aB
VdNEgJjSzM7dcSB+4HUE7cnXS1MtkwdQNGgz995j0cQbm4LsdV1F87Hcm6M3QorL
qOqTl67hbldbTRYtotXazWOt/Qapujg/p1it8+kA6QKBgQDw4yBzJQC3TnHisdsf
twemuvCxFMdsZs1uxGPgWJjkv4VutkCKeu5KFWK3O92rBydDXB/IweON5AR7iqmi
0NhtmK1xFxBagSeTvz/kPsHkyzsm/lbZM8LoSCCjtQ91UXlh61XGgDsnAQIYsiNr
006ev9y7vsxqFhOjnQaOmAwmGQKBgQDUZLDEYltmh6MhyO6lPhj044MMCMCA+aEu
Shl/EWb/Eoo48MTrDDFet4/9tAwoVEjHO46ymiP6MEtne6ChwSjI75jW39lUgwPb
d0YXK/M+BZtXog6xqHXkJcGrJcZoBiFj8AwkwlK1n1cjGSVokodyK8JjQ0iJu0EV
C4tJQ1ysRwKBgBvuSgHv5XBbwSrG8qBvyYxUmrn9rc3s8Z8JWIdX3oqPhno62ar0
7BJc/nA+mcpN7wiJcwoFKUx3humIP3kofB/hFyNIyFWmKh+gilj9yd+sjPRNg2Z1
8QCb9GTnBp7Uzp1C+1Qj5Df2jvasGR1UiAYyOvbt/afDXY2YFH2ONcJpAoGBAKji
p+yAiU0t7Xmf3KNojU+s2TdofioQVSoJodx4af3JMD+2s95zA47dR5Hk6QXofzZt
FTrPdmwqmsrecwwsG9IrMs0pkhaxVw/b98/VEsXuj2dPZX+/BH81xpngn7N3rHVb
G0zfeAUTfqZaCHTujuUqBpgHmFZsn4OsekT3W2lhAoGAbnB7k+gt5cWiX4lgn28F
YtHcWbK536eBIV1/zZ2u9Yx5qkfUmIVLLLQU2pFKO804Jkq1XsguHgYIsoUoz4LF
yVH0ymEQeYGdqgh4Q5k1ckpY5pHeJuEr6r3snx4gMZsH40jk+dVf58Ab30MOE1p3
XLeQgmAl46RoBo1wHm3lfDc=
-----END PRIVATE KEY-----";

    /// RSA-2048 public key (SPKI PEM) matching TEST_RSA_PRIVATE_KEY_PEM.
    const TEST_RSA_PUBLIC_KEY_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAx9rV7PhgAVAg01EYd/4i
+/+c2T98lzU4UcwBTYiYie2bf0VBPUki4Q+lLqB7DgDHsioT+wKKHTIpxp6bEFxF
fUdCaAh9oyEqPMVfwSkTS8XSgnDmRacnu3vZjZzLzhEztihIVBIAHMkg7pDAZ/Fk
8hVGK0DY//oodFuFGxGjFZyYohFpqvso9MaOg2HncRGCFaxlQfkQveEU6HzxP1M0
S9X54YneF/EWhvb1YWkllA1+2+iwrCO9ygfcrqYqjDAbVtoUVNC/VChNPWgaGoZ+
RKAh0J2/9/3FWwXeDKZLsxmy5kFej390rC/P8hEoHsBmy4Wb6dZc+LwyN+IrkPNc
7wIDAQAB
-----END PUBLIC KEY-----";

    /// A second RSA keypair (different from TEST_RSA_PRIVATE_KEY_PEM), used to
    /// test that a signature by the wrong key is rejected.
    const TEST_RSA_WRONG_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDMf0/gMOhseRlY
7HQlJ1/T5W6p6Dp4Wnqp/X0SzNtKYcZuC78GwQx7Ukha6GxHP/STH40KlIsZTtym
QiAlTpP9sZ874F11RqVuX5Sg1b3U3IE0bohgcyzpjVwYyBMCI5uxUDf2VJnOx9JI
Hhi9AxMGz+2dq8L3FGRWTgsEPEFpx6IuxK+rXS2LxAYDIlfIDVCksFuZLlJISDAT
4FIvAOMI2C7tDpFzVbn6blns5nxduz7O/IpW6XFkfeCZbL+lDfIFhK8jo1RGM4x7
39S4kHha/Er8sVVQKm83CHJEv+ueuGIAvFd6mp+W8CybE8Hx98nWvXgZXFAFrThl
lluC5JmtAgMBAAECggEAI9Djdj5Sotb13clyESTLh5L+NhNmlDgyli2/t2h6OtWT
magEgcQTcdDoO8XL2xHEPfVPcEwybZEOm67mruoLiOoQW74Q2E6ygDmM0DuHRy4E
kiCO0aeydMhNmkiGbcA7T0uftYy9MIZ2WautxQLyFOYbdZtE5x3i8euyyb/c7A/a
v1bG/pIfzSL2ZIA/3E0PpKL/17KbfNaIJoFBJcDd1AfwKG1zPJq7dWcyYol1nGtL
Flz+nuKD26SAjJNjtipjYkHczDoTtGM9gBfe/QULcakNpPyUetrI1ZKDqgglN27Q
zPeByY6GkTgmochYOtwf8s5LrpN2C5BXiTTNkZKRCQKBgQD3YySPHDxj2Fp4T9qQ
4l99S6ZU+fNFxr4f21ejnH5RtKH8m1WXZbk3VRjrfZOQ8OH4rAc6t4TEX3XPGQMV
iYLraoOGiw0AxiYLxYtvZvihz3xyw8Rwf8vtikiDEaRvENF0IUu2Bt75wU3G1lcV
HGvtySn7hbJPiFicpZqEVCGEyQKBgQDTnejFncArGRd0kMqGrhRJe4+k1004waj/
edspKS2QOL7BAYj884udRzpkxyr+bhr78/9R45qnrK/J4oDAvQatqzgLj6E3mCQq
STnb4OxFzJL9laxVMeT+zFFInft1muu2rAqSYowOTsS98fIhM4u4CP+g+m7vZjiz
ERNepgqTxQKBgQCoCqNZxr9KvzrtAKkhw3NDo/BvRn22RwL8lrzYOUQg8gcalNU2
CvYeHOLZi6qCSO3mQcyDWQeJcKKQs5fBuG/Cw85lxOxnOzG6y0wktxhqqYsKVeqI
1HZMe6M3zPMaMp1kOf24vsAVfPX8+7mZcH3rvrqSzMVLev1eIqtr+c3u6QKBgBJN
Qeh1cD1J+kFWlG15eL+yNAYpqMAT363YuB+jNBGZFsZSf6qA1b5QfrhgkVNX6nWH
8LkAWkvOH5XyRPhmYMF8YWh+j47jVZ1in+JoXYbb3oqX+0OTAR8YRJ9nKmxNbb1q
u69VXo+OOG3FEw/UCW1tOc6OWjHSQW0bOPWinp+RAoGBANlGO02t4Znm+EwB4E8g
X8MIxJ0kKgZuy+SZNWJ4J8BzH0X/C9hGSNAnVoKvsT3YpB/HHGNvo4CJv7PnEXFH
5nQOdg/gId6PsFwwKi/9e8QPM3MzUvpdgbYdk8xTDPyGpyEwDsV2kMo1WIYQchBu
vIyJeH8/89a9IXZXlMIA9KH9
-----END PRIVATE KEY-----";

    const APP_ID: &str = "test-app-id-12345";
    const ISSUER: &str = "https://api.botframework.com";

    /// Build a minimal JWT with the given claims and sign with the provided key.
    fn make_jwt(
        iss: &str,
        aud: &str,
        exp_offset_secs: i64, // positive = future, negative = past
        encoding_key: &EncodingKey,
    ) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time before epoch")
            .as_secs() as i64;

        let claims = json!({
            "iss": iss,
            "aud": aud,
            "exp": now + exp_offset_secs,
            "nbf": now - 60,
            "iat": now - 60,
        });

        let header = Header::new(Algorithm::RS256);
        encode(&header, &claims, encoding_key).expect("test JWT encoding failed")
    }

    fn encoding_key() -> EncodingKey {
        EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY_PEM.as_bytes())
            .expect("test private key is valid")
    }

    fn wrong_encoding_key() -> EncodingKey {
        EncodingKey::from_rsa_pem(TEST_RSA_WRONG_PRIVATE_KEY_PEM.as_bytes())
            .expect("wrong test private key is valid")
    }

    fn decoding_key() -> DecodingKey {
        DecodingKey::from_rsa_pem(TEST_RSA_PUBLIC_KEY_PEM)
            .expect("test public key is valid")
    }

    // -----------------------------------------------------------------------
    // JWT validation tests (use injectable validate_token_with_key)
    // -----------------------------------------------------------------------

    /// Valid token with correct iss, aud, exp, signed by the expected key.
    #[test]
    fn test_jwt_valid_token() {
        let token = make_jwt(ISSUER, APP_ID, 3600, &encoding_key());
        assert!(
            validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key()).is_ok(),
            "valid token should pass validation"
        );
    }

    /// Token with wrong audience → rejected.
    #[test]
    fn test_jwt_wrong_aud() {
        let token = make_jwt(ISSUER, "wrong-aud", 3600, &encoding_key());
        let err = validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key());
        assert!(err.is_err(), "wrong aud should be rejected; got: {err:?}");
    }

    /// Token with wrong issuer → rejected.
    #[test]
    fn test_jwt_wrong_iss() {
        let token = make_jwt("https://evil.example.com", APP_ID, 3600, &encoding_key());
        let err = validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key());
        assert!(err.is_err(), "wrong iss should be rejected; got: {err:?}");
    }

    /// Token already expired (exp in the past, beyond any leeway) → rejected.
    #[test]
    fn test_jwt_expired() {
        // 10 minutes in the past — beyond the 5-minute leeway.
        let token = make_jwt(ISSUER, APP_ID, -600, &encoding_key());
        let err = validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key());
        assert!(err.is_err(), "expired token should be rejected; got: {err:?}");
    }

    /// Token signed by a DIFFERENT private key → signature fails.
    #[test]
    fn test_jwt_wrong_signing_key() {
        let token = make_jwt(ISSUER, APP_ID, 3600, &wrong_encoding_key());
        let err = validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key());
        assert!(
            err.is_err(),
            "token signed by wrong key should be rejected; got: {err:?}"
        );
    }

    /// Token signed with HS256 (HMAC) → rejected because only RS256 is allowed.
    #[test]
    fn test_jwt_hs256_rejected() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time before epoch")
            .as_secs() as i64;

        let claims = serde_json::json!({
            "iss": ISSUER,
            "aud": APP_ID,
            "exp": now + 3600,
        });

        let hmac_key = EncodingKey::from_secret(b"some-hmac-secret");
        let header = Header::new(Algorithm::HS256);
        let token = jsonwebtoken::encode(&header, &claims, &hmac_key)
            .expect("HMAC token encoding failed");

        // The decoding key is an RSA key; jsonwebtoken will reject the HS256 alg.
        let err = validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key());
        assert!(err.is_err(), "HS256 token should be rejected; got: {err:?}");
    }

    /// Token with `aud` claim entirely absent → rejected.
    ///
    /// Regression test: jsonwebtoken only *validates* `aud` when the claim is
    /// present.  `set_required_spec_claims` must include `"aud"` so that a
    /// correctly signed token that simply omits the audience is not accepted.
    #[test]
    fn test_jwt_missing_aud_rejected() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time before epoch")
            .as_secs() as i64;

        // Intentionally omit the `aud` field.
        let claims = json!({
            "iss": ISSUER,
            "exp": now + 3600,
            "nbf": now - 60,
            "iat": now - 60,
        });

        let header = Header::new(Algorithm::RS256);
        let token = encode(&header, &claims, &encoding_key())
            .expect("test JWT encoding failed");

        let err = validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key());
        assert!(
            err.is_err(),
            "token missing `aud` claim should be rejected; got: {err:?}"
        );
    }

    /// Token with `iss` claim entirely absent → rejected.
    ///
    /// Regression test: jsonwebtoken only *validates* `iss` when the claim is
    /// present.  `set_required_spec_claims` must include `"iss"` so that a
    /// correctly signed token that omits the issuer is not accepted.
    #[test]
    fn test_jwt_missing_iss_rejected() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time before epoch")
            .as_secs() as i64;

        // Intentionally omit the `iss` field.
        let claims = json!({
            "aud": APP_ID,
            "exp": now + 3600,
            "nbf": now - 60,
            "iat": now - 60,
        });

        let header = Header::new(Algorithm::RS256);
        let token = encode(&header, &claims, &encoding_key())
            .expect("test JWT encoding failed");

        let err = validate_token_with_key(&token, APP_ID, ISSUER, &decoding_key());
        assert!(
            err.is_err(),
            "token missing `iss` claim should be rejected; got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // `validate_inbound_jwt` header-parsing tests (no network, no JWKS)
    // -----------------------------------------------------------------------

    /// Missing "Bearer " prefix → rejected immediately.
    #[tokio::test]
    async fn test_inbound_jwt_missing_bearer_prefix() {
        let jwks = JwksCache::new().expect("JwksCache::new");
        let token = make_jwt(ISSUER, APP_ID, 3600, &encoding_key());
        // Pass a raw token without the "Bearer " prefix.
        let err = validate_inbound_jwt(&token, APP_ID, &jwks).await;
        assert!(
            err.is_err(),
            "missing Bearer prefix should be rejected; got: {err:?}"
        );
    }

    /// Empty bearer token → rejected immediately.
    #[tokio::test]
    async fn test_inbound_jwt_empty_bearer() {
        let jwks = JwksCache::new().expect("JwksCache::new");
        let err = validate_inbound_jwt("Bearer ", APP_ID, &jwks).await;
        assert!(err.is_err(), "empty Bearer token should be rejected; got: {err:?}");
    }

    // -----------------------------------------------------------------------
    // Token-provider tests (needs_refresh)
    // -----------------------------------------------------------------------

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
