//! Microsoft Teams messaging adapter (Bot Framework).
//!
//! This module provides:
//! - Outbound Azure AD token provider (mint/cache Bot Connector bearer tokens).
//! - Inbound JWT validator: verifies that incoming POST /api/messages requests
//!   are signed by Azure Bot Service, preventing message injection by third
//!   parties.
//! - Bot Framework Activity deserialization and normalization to `InboundMessage`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use jsonwebtoken::{
    Algorithm, DecodingKey, Header, Validation, decode, decode_header, jwk::JwkSet,
};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use url::Url;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How long before the token actually expires we consider it stale and refresh
/// proactively.  5 minutes matches common Azure AD guidance.
const REFRESH_LEEWAY: Duration = Duration::from_secs(300);

/// Azure AD token endpoint template — `{tenant}` is replaced at runtime.
const TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token";

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
        if let Some(ref cached) = *guard
            && !needs_refresh(cached.expires_at, now, REFRESH_LEEWAY)
        {
            return Ok(cached.token.clone());
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
            anyhow::bail!("Azure AD token endpoint returned {status}: {body}");
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

        let token = token_resp.access_token;
        *guard = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });

        Ok(token)
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
    // If expires_at < leeway, checked_sub returns None; fall back to expires_at
    // so we trigger an immediate refresh rather than panicking on underflow.
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
            if let Some(ref cached) = *guard
                && !Self::is_stale(&cached.fetched_at)
            {
                return Ok(Arc::new(cached.keyset.clone()));
            }
        }

        // Slow path: write lock — fetch fresh keys.
        let mut guard = self.inner.write().await;
        // Double-check: another waiter may have refreshed while we waited.
        if let Some(ref cached) = *guard
            && !Self::is_stale(&cached.fetched_at)
        {
            return Ok(Arc::new(cached.keyset.clone()));
        }

        let keyset = Self::fetch_keyset(&self.http).await?;
        let fetched_at = SystemTime::now();
        *guard = Some(JwksCacheInner {
            keyset: keyset.clone(),
            fetched_at,
        });
        Ok(Arc::new(keyset))
    }

    /// Find a key by `kid`, refreshing once if not found.
    ///
    /// Returns `Err` if the key is still absent after one refresh.
    pub async fn find_key(&self, kid: &str) -> anyhow::Result<DecodingKey> {
        let keyset = self.keyset().await?;
        if let Some(jwk) = keyset.find(kid) {
            return DecodingKey::from_jwk(jwk).context("failed to build DecodingKey from JWK");
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
    let header: Header = decode_header(token).context("failed to decode JWT header")?;

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
    decode::<serde_json::Value>(token, key, &validation).context("JWT validation failed")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Bot Framework Activity deserialization
// ---------------------------------------------------------------------------

/// The `from` / `recipient` identity object in a Bot Framework Activity.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ActivityAccount {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// The `conversation` object in a Bot Framework Activity.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityConversation {
    pub id: String,
    #[serde(rename = "conversationType")]
    pub conversation_type: Option<String>,
}

/// Subset of the Bot Framework Activity schema used for inbound message
/// processing.  Unknown fields are silently ignored (tolerant parsing).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Activity {
    /// The activity type, e.g. "message", "conversationUpdate", "typing".
    #[serde(rename = "type")]
    pub activity_type: String,

    /// Unique activity identifier assigned by the Bot Connector.
    #[serde(default)]
    pub id: String,

    /// The text body of the message (may be absent for non-message activities).
    pub text: Option<String>,

    /// The sender of the activity (the user or service sending to the bot).
    pub from: ActivityAccount,

    /// The conversation this activity belongs to.
    pub conversation: ActivityConversation,

    /// Base URI of the channel service — used by the outbound adapter to send
    /// replies back to the correct Bot Connector endpoint.
    #[serde(rename = "serviceUrl")]
    pub service_url: String,

    /// The bot (recipient) identity.
    pub recipient: ActivityAccount,

    /// Platform-specific extension data.
    #[serde(rename = "channelData", default)]
    pub channel_data: serde_json::Value,

    /// The ID of the activity this is a reply to, if any.
    #[serde(rename = "replyToId")]
    pub reply_to_id: Option<String>,

    /// Mention entities and other structured data attached to the activity.
    #[serde(default)]
    pub entities: Option<Vec<serde_json::Value>>,
}

// ---------------------------------------------------------------------------
// Activity → InboundMessage normalization
// ---------------------------------------------------------------------------

/// Remove all `<at …>…</at>` mention spans from `text` and collapse any
/// resulting runs of whitespace into single spaces.
///
/// Handles both plain `<at>` and attributed forms such as `<at id="0">`.
fn strip_at_mentions(text: &str) -> String {
    // Iteratively remove <at...>...</at> tags (non-greedy inner match).
    // We match the open tag by finding the "<at" prefix and then scanning
    // forward to the closing ">" so that attributes like id="0" are consumed.
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<at") {
        // Confirm the char after "<at" is either '>' or a space/tab (attribute),
        // to avoid accidentally matching e.g. "<attempt>" etc.
        let after_prefix = &remaining[start + "<at".len()..];
        let next_ch = after_prefix.chars().next();
        if !matches!(
            next_ch,
            Some('>') | Some(' ') | Some('\t') | Some('\r') | Some('\n')
        ) {
            // Not a real <at> tag — emit up to and including "<at" and continue.
            result.push_str(&remaining[..start + "<at".len()]);
            remaining = after_prefix;
            continue;
        }
        result.push_str(&remaining[..start]);
        // Find the closing '>' of the open tag.
        let open_tag_end = match after_prefix.find('>') {
            Some(i) => i,
            None => {
                // Malformed — no closing '>'; keep the rest verbatim.
                result.push_str(remaining);
                remaining = "";
                break;
            }
        };
        // Skip past the entire open tag (e.g. `<at id="0">`).
        let after_open = &after_prefix[open_tag_end + 1..];
        if let Some(end) = after_open.find("</at>") {
            remaining = &after_open[end + "</at>".len()..];
        } else {
            // Malformed — no closing tag; keep the rest verbatim.
            result.push_str(remaining);
            remaining = "";
            break;
        }
    }
    result.push_str(remaining);

    // Collapse runs of whitespace (spaces, tabs, newlines) to a single space
    // and trim the result.
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Determine whether the bot was @mentioned in `activity`.
///
/// Priority:
/// 1. Check `entities` for a `"mention"` entry whose `mentioned.id` equals
///    `activity.recipient.id`.
/// 2. Fall back to presence of any `<at>` tag in the raw text.
fn bot_was_mentioned(activity: &Activity) -> bool {
    let bot_id = &activity.recipient.id;

    if let Some(entities) = &activity.entities {
        for entity in entities {
            let is_mention = entity
                .get("type")
                .and_then(|v| v.as_str())
                .map(|t| t.eq_ignore_ascii_case("mention"))
                .unwrap_or(false);

            if is_mention {
                let mentioned_id = entity
                    .pointer("/mentioned/id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if mentioned_id == bot_id {
                    return true;
                }
            }
        }
    }

    // Fallback: presence of an <at …> tag (with or without attributes) in text
    // implies a mention.
    activity
        .text
        .as_deref()
        .map(|t| t.contains("<at"))
        .unwrap_or(false)
}

/// Convert a Bot Framework [`Activity`] to a project-standard [`InboundMessage`].
///
/// Returns `None` for any activity whose `type` is not `"message"` — v1 only
/// handles conversational message activities.
///
/// # Metadata keys set
///
/// | Key                        | Description                                      |
/// |----------------------------|--------------------------------------------------|
/// | `"message_id"`             | Bot Connector activity `id`                      |
/// | `"teams_service_url"`      | `serviceUrl` — needed by the outbound adapter    |
/// | `"teams_conversation_type"`| `conversationType` when present                  |
/// | `"teams_reply_to_id"`      | `replyToId` when present                         |
/// | `"teams_mentioned"`        | `"true"` / `"false"` — whether the bot was @mentioned |
pub fn activity_to_inbound(
    activity: &Activity,
    runtime_key: &str,
) -> Option<crate::InboundMessage> {
    if !activity.activity_type.eq_ignore_ascii_case("message") {
        return None;
    }

    let raw_text = activity.text.as_deref().unwrap_or("").trim().to_string();
    let clean_text = strip_at_mentions(&raw_text);
    let mentioned = bot_was_mentioned(activity);

    // Build the conversation_id.
    let base_conversation_id = format!("teams:{}", activity.conversation.id);
    let conversation_id = crate::messaging::apply_runtime_adapter_to_conversation_id(
        runtime_key,
        base_conversation_id,
    );

    // Assemble metadata.
    let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();

    metadata.insert(
        crate::metadata_keys::MESSAGE_ID.to_string(),
        serde_json::json!(activity.id),
    );
    metadata.insert(
        "teams_service_url".to_string(),
        serde_json::json!(activity.service_url),
    );
    if let Some(conv_type) = &activity.conversation.conversation_type {
        metadata.insert(
            "teams_conversation_type".to_string(),
            serde_json::json!(conv_type),
        );
    }
    if let Some(reply_to) = &activity.reply_to_id {
        metadata.insert("teams_reply_to_id".to_string(), serde_json::json!(reply_to));
    }
    metadata.insert(
        "teams_mentioned".to_string(),
        serde_json::json!(if mentioned { "true" } else { "false" }),
    );

    let formatted_author = if activity.from.name.is_empty() {
        None
    } else {
        Some(activity.from.name.clone())
    };

    Some(crate::InboundMessage {
        id: activity.id.clone(),
        source: "teams".to_string(),
        adapter: Some(runtime_key.to_string()),
        conversation_id,
        sender_id: activity.from.id.clone(),
        agent_id: None,
        content: crate::MessageContent::Text(clean_text),
        timestamp: chrono::Utc::now(),
        metadata,
        formatted_author,
    })
}

// ---------------------------------------------------------------------------
// SSRF guard
// ---------------------------------------------------------------------------

/// Return `true` iff `url` is a safe Bot Framework serviceUrl.
///
/// Rules:
/// - Must parse as a valid URL.
/// - Scheme must be `https` (case-insensitive).
/// - Host must end with `.botframework.com` or `.trafficmanager.net`.
///
/// This guards against attacker-supplied serviceUrl values that could be used
/// to exfiltrate the Bot Connector bearer token.
fn is_allowed_service_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let host = match parsed.host_str() {
        Some(h) => h.to_lowercase(),
        None => return false,
    };
    host.ends_with(".botframework.com") || host.ends_with(".trafficmanager.net")
}

// ---------------------------------------------------------------------------
// Sidecar persistence helpers
// ---------------------------------------------------------------------------

/// Persist `map` as JSON to `path` atomically (write-tmp → rename).
/// Errors are silently swallowed (log-only) to avoid disrupting normal flow.
fn save_service_urls(map: &HashMap<String, String>, path: &std::path::Path) {
    let tmp = path.with_extension("json.tmp");
    let Ok(json) = serde_json::to_string(map) else {
        tracing::warn!(?path, "teams sidecar: failed to serialise service_urls");
        return;
    };
    if std::fs::write(&tmp, &json).is_ok() {
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::warn!(%e, ?path, "teams sidecar: rename failed");
        }
    } else {
        tracing::warn!(?path, "teams sidecar: write to tmp file failed");
    }
}

// ---------------------------------------------------------------------------
// TeamsAdapter — inbound HTTP server
// ---------------------------------------------------------------------------

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use tokio::sync::mpsc;

use crate::config::TeamsPermissions;
use crate::messaging::traits::{
    InboundStream, Messaging, ensure_supported_broadcast_response, mark_classified_broadcast,
    mark_permanent_broadcast,
};
use crate::{InboundMessage, OutboundResponse};

/// Shared state injected into axum handlers.
#[derive(Clone)]
struct TeamsHandlerState {
    inbound_tx: mpsc::Sender<InboundMessage>,
    /// Available to handlers for future outbound calls (e.g., typing indicators).
    #[allow(dead_code)]
    token: Arc<TeamsTokenProvider>,
    jwks: Arc<JwksCache>,
    app_id: String,
    service_urls: Arc<Mutex<HashMap<String, String>>>,
    permissions: Arc<ArcSwap<TeamsPermissions>>,
    runtime_key: String,
    /// Available to handlers for future outbound calls.
    #[allow(dead_code)]
    http_client: Client,
    sidecar_path: Option<PathBuf>,
}

/// Microsoft Teams Bot Framework adapter.
pub struct TeamsAdapter {
    /// Runtime key (adapter name), e.g. `"teams"` or `"teams:prod"`.
    runtime_key: String,
    app_id: String,
    #[allow(dead_code)]
    tenant_id: String,
    token: Arc<TeamsTokenProvider>,
    jwks: Arc<JwksCache>,
    port: u16,
    bind: String,
    /// `conversation_id → serviceUrl` — populated on each inbound Activity so
    /// the outbound adapter knows where to send replies.
    service_urls: Arc<Mutex<HashMap<String, String>>>,
    /// Lazily populated by `start()`; stored so handlers can send inbound
    /// messages without holding a lock across await points.
    inbound_tx: Arc<RwLock<Option<mpsc::Sender<InboundMessage>>>>,
    permissions: Arc<ArcSwap<TeamsPermissions>>,
    /// HTTP client for outbound Bot Connector requests.
    http_client: Client,
    /// Optional path for sidecar persistence of service_urls.
    sidecar_path: Option<PathBuf>,
}

impl TeamsAdapter {
    /// Construct a new `TeamsAdapter` from discrete credentials.
    ///
    /// `permissions` should be pre-built via `TeamsPermissions::from_config` /
    /// `from_instance_config` and wrapped in `Arc<ArcSwap<..>>` so the config
    /// watcher can hot-reload it without restarting the listener.
    pub fn new(
        runtime_key: impl Into<String>,
        app_id: impl Into<String>,
        client_secret: impl Into<String>,
        tenant_id: impl Into<String>,
        port: u16,
        bind: impl Into<String>,
        permissions: Arc<ArcSwap<TeamsPermissions>>,
    ) -> anyhow::Result<Self> {
        let tenant_id = tenant_id.into();
        let app_id = app_id.into();
        let token = Arc::new(TeamsTokenProvider::new(
            tenant_id.clone(),
            app_id.clone(),
            client_secret.into(),
        )?);
        let jwks = Arc::new(JwksCache::new()?);
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client for Teams outbound")?;

        Ok(Self {
            runtime_key: runtime_key.into(),
            app_id,
            tenant_id,
            token,
            jwks,
            port,
            bind: bind.into(),
            service_urls: Arc::new(Mutex::new(HashMap::new())),
            inbound_tx: Arc::new(RwLock::new(None)),
            permissions,
            http_client,
            sidecar_path: None,
        })
    }

    /// Set the sidecar persistence path for `service_urls`.
    ///
    /// When set, `service_urls` is loaded from this path on `start()` and
    /// persisted atomically after each inbound capture or outbound send.
    pub fn with_sidecar_path(mut self, path: PathBuf) -> Self {
        self.sidecar_path = Some(path);
        self
    }
}

/// Build a `TeamsAdapter` with the sidecar path set to
/// `<instance_dir>/teams_service_urls.json`.
///
/// This is the canonical constructor used by both the daemon startup path and
/// the config-watcher reload path so the two never drift apart.
pub fn build_teams_adapter(
    runtime_key: impl Into<String>,
    app_id: impl Into<String>,
    client_secret: impl Into<String>,
    tenant_id: impl Into<String>,
    port: u16,
    bind: impl Into<String>,
    permissions: std::sync::Arc<arc_swap::ArcSwap<crate::config::TeamsPermissions>>,
    instance_dir: &std::path::Path,
) -> anyhow::Result<TeamsAdapter> {
    Ok(TeamsAdapter::new(
        runtime_key,
        app_id,
        client_secret,
        tenant_id,
        port,
        bind,
        permissions,
    )?
    .with_sidecar_path(instance_dir.join("teams_service_urls.json")))
}

impl Messaging for TeamsAdapter {
    fn name(&self) -> &str {
        &self.runtime_key
    }

    async fn start(&self) -> crate::Result<InboundStream> {
        // Load sidecar on startup if configured.
        if let Some(ref path) = self.sidecar_path {
            match std::fs::read_to_string(path) {
                Ok(contents) => match serde_json::from_str::<HashMap<String, String>>(&contents) {
                    Ok(loaded) => {
                        *self.service_urls.lock().await = loaded;
                        tracing::info!(?path, "teams sidecar: loaded service_urls");
                    }
                    Err(e) => {
                        tracing::warn!(%e, ?path, "teams sidecar: failed to parse service_urls JSON");
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Expected on first startup — not an error.
                }
                Err(e) => {
                    tracing::warn!(%e, ?path, "teams sidecar: failed to read service_urls file");
                }
            }
        }

        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(256);
        *self.inbound_tx.write().await = Some(inbound_tx.clone());

        let state = TeamsHandlerState {
            inbound_tx,
            token: self.token.clone(),
            jwks: self.jwks.clone(),
            app_id: self.app_id.clone(),
            service_urls: self.service_urls.clone(),
            permissions: self.permissions.clone(),
            runtime_key: self.runtime_key.clone(),
            http_client: self.http_client.clone(),
            sidecar_path: self.sidecar_path.clone(),
        };

        let app = Router::new()
            .route("/api/messages", post(handle_messages))
            .route("/health", get(handle_health))
            .with_state(state);

        let bind = if self.bind.contains(':') {
            format!("[{}]:{}", self.bind, self.port)
        } else {
            format!("{}:{}", self.bind, self.port)
        };

        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .with_context(|| format!("failed to bind Teams webhook server to {bind}"))?;

        tracing::info!(%bind, "Teams webhook server listening");

        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "Teams webhook server exited with error");
            }
        });

        let stream = tokio_stream::wrappers::ReceiverStream::new(inbound_rx);
        Ok(Box::pin(stream))
    }

    async fn respond(
        &self,
        message: &InboundMessage,
        response: OutboundResponse,
    ) -> crate::Result<()> {
        // Extract the text to send (or return Ok(()) for unsupported variants).
        let text = match response {
            OutboundResponse::Text(t) => t,
            OutboundResponse::ThreadReply { text, .. } => text,
            OutboundResponse::Ephemeral { text, .. } => text,
            OutboundResponse::RichMessage { text, .. } => text,
            OutboundResponse::ScheduledMessage { text, .. } => text,
            // Silent no-ops for unsupported variants.
            OutboundResponse::Reaction(_)
            | OutboundResponse::RemoveReaction(_)
            | OutboundResponse::Status(_)
            | OutboundResponse::StreamStart
            | OutboundResponse::StreamChunk(_)
            | OutboundResponse::StreamEnd
            | OutboundResponse::File { .. } => return Ok(()),
        };

        // Resolve serviceUrl.
        let service_url = message
            .metadata
            .get("teams_service_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // If not found in metadata, look up from the map.
        let service_url = match service_url {
            Some(u) => u,
            None => {
                let urls = self.service_urls.lock().await;
                match urls.get(&message.conversation_id).cloned() {
                    Some(u) => u,
                    None => {
                        return Err(mark_permanent_broadcast(anyhow::anyhow!(
                            "teams respond: no serviceUrl for conversation_id {}",
                            message.conversation_id
                        )));
                    }
                }
            }
        };

        // SSRF guard — reject non-allowlisted serviceUrls.
        if !is_allowed_service_url(&service_url) {
            return Err(mark_permanent_broadcast(anyhow::anyhow!(
                "teams respond: serviceUrl blocked by SSRF guard: {service_url}"
            )));
        }

        // Strip the platform prefix to get the bare MS conversation id.
        let bare_conv_id = message
            .conversation_id
            .strip_prefix(&format!("{}:", self.runtime_key))
            .unwrap_or(&message.conversation_id)
            .to_string();

        // Build the reply-to id if present.
        let reply_to_id = message
            .metadata
            .get("teams_reply_to_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Build the JSON body.
        let body = if let Some(ref reply_to) = reply_to_id {
            serde_json::json!({
                "type": "message",
                "text": text,
                "replyToId": reply_to,
            })
        } else {
            serde_json::json!({
                "type": "message",
                "text": text,
            })
        };

        let url = format!(
            "{}/v3/conversations/{}/activities",
            service_url.trim_end_matches('/'),
            bare_conv_id,
        );

        let token = self
            .token
            .bearer()
            .await
            .map_err(mark_classified_broadcast)?;

        let resp = self
            .http_client
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                mark_classified_broadcast(anyhow::anyhow!("teams respond HTTP error: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            return Err(mark_classified_broadcast(anyhow::anyhow!(
                "teams respond: Bot Connector returned {status}: {body_text}"
            )));
        }

        // Persist updated service_urls if sidecar is configured.
        if let Some(ref path) = self.sidecar_path {
            let urls = self.service_urls.lock().await;
            save_service_urls(&urls, path);
        }

        Ok(())
    }

    async fn broadcast(&self, target: &str, response: OutboundResponse) -> crate::Result<()> {
        // Gate: only Text is supported for proactive broadcast.
        fn is_supported(r: &OutboundResponse) -> bool {
            matches!(r, OutboundResponse::Text(_))
        }
        ensure_supported_broadcast_response("teams", &response, is_supported)?;

        let OutboundResponse::Text(text) = response else {
            // Already handled by ensure_supported_broadcast_response above,
            // but the compiler needs this to be exhaustive.
            unreachable!()
        };

        // Look up serviceUrl — the full `target` string is the map key.
        let service_url = {
            let urls = self.service_urls.lock().await;
            match urls.get(target).cloned() {
                Some(u) => u,
                None => {
                    return Err(mark_permanent_broadcast(anyhow::anyhow!(
                        "teams broadcast: no serviceUrl for target {target}"
                    )));
                }
            }
        };

        // SSRF guard.
        if !is_allowed_service_url(&service_url) {
            return Err(mark_permanent_broadcast(anyhow::anyhow!(
                "teams broadcast: serviceUrl blocked by SSRF guard: {service_url}"
            )));
        }

        // Strip the platform prefix to get the bare MS conversation id.
        let bare_conv_id = target
            .strip_prefix(&format!("{}:", self.runtime_key))
            .unwrap_or(target)
            .to_string();

        let body = serde_json::json!({
            "type": "message",
            "text": text,
        });

        let url = format!(
            "{}/v3/conversations/{}/activities",
            service_url.trim_end_matches('/'),
            bare_conv_id,
        );

        let token = self
            .token
            .bearer()
            .await
            .map_err(mark_classified_broadcast)?;

        let resp = self
            .http_client
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                mark_classified_broadcast(anyhow::anyhow!("teams broadcast HTTP error: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            return Err(mark_classified_broadcast(anyhow::anyhow!(
                "teams broadcast: Bot Connector returned {status}: {body_text}"
            )));
        }

        // Persist updated service_urls if sidecar is configured.
        if let Some(ref path) = self.sidecar_path {
            let urls = self.service_urls.lock().await;
            save_service_urls(&urls, path);
        }

        Ok(())
    }

    async fn health_check(&self) -> crate::Result<()> {
        self.token
            .bearer()
            .await
            .map(|_| ())
            .map_err(crate::error::Error::Other)
    }
}

// ---------------------------------------------------------------------------
// Axum handlers
// ---------------------------------------------------------------------------

/// POST /api/messages — the Bot Framework inbound message endpoint.
///
/// Security flow:
///   1. Validate `Authorization: Bearer <jwt>` via Azure Bot Service JWKS → 401 on failure.
///   2. Parse JSON body as `Activity` → 400 on failure.
///   3. Capture `serviceUrl` into the `service_urls` map.
///   4. Normalize to `InboundMessage` (returns `None` for non-message activities) → 200, no dispatch.
///   5. Permission check (DM: enforce `dm_allowed_users`; channel: enforce `channel_filter` if set)
///      → silently drop (200 OK, no dispatch) on deny.
///   6. Send to inbound channel; return 200.
async fn handle_messages(
    headers: HeaderMap,
    State(state): State<TeamsHandlerState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, (StatusCode, &'static str)> {
    // --- Step 1: JWT auth ---
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Err(err) = validate_inbound_jwt(auth_header, &state.app_id, &state.jwks).await {
        tracing::warn!(%err, "Teams inbound JWT validation failed — returning 401");
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }

    // --- Step 2: Parse body ---
    let activity: Activity = match serde_json::from_slice(&body) {
        Ok(a) => a,
        Err(err) => {
            tracing::warn!(%err, "Teams inbound activity parse failed — returning 400");
            return Err((StatusCode::BAD_REQUEST, "bad request"));
        }
    };

    // --- Step 3: Capture serviceUrl (keyed by rewritten conversation_id) ---
    {
        let base = format!("teams:{}", activity.conversation.id);
        let conv_key =
            crate::messaging::apply_runtime_adapter_to_conversation_id(&state.runtime_key, base);
        let mut urls = state.service_urls.lock().await;
        urls.insert(conv_key, activity.service_url.clone());
        // Persist sidecar if configured.
        if let Some(ref path) = state.sidecar_path {
            save_service_urls(&urls, path);
        }
    }

    // --- Step 4: Normalize to InboundMessage ---
    let Some(msg) = activity_to_inbound(&activity, &state.runtime_key) else {
        // Non-message activity (typing, conversationUpdate, etc.) — ack and ignore.
        return Ok(StatusCode::OK);
    };

    // --- Step 5: Permission check ---
    let conversation_type = activity.conversation.conversation_type.as_deref();
    let channel_id = &activity.conversation.id;
    let sender_id = &activity.from.id;

    let perms = state.permissions.load();
    if !perms.is_allowed(conversation_type, sender_id, channel_id) {
        tracing::debug!(
            sender_id,
            ?conversation_type,
            channel_id,
            "Teams inbound message dropped by permission filter"
        );
        // Return 200 so Bot Framework doesn't retry.
        return Ok(StatusCode::OK);
    }

    // --- Step 6: Dispatch ---
    if state.inbound_tx.send(msg).await.is_err() {
        tracing::warn!("Teams inbound channel closed; dropping message");
    }

    Ok(StatusCode::OK)
}

async fn handle_health() -> StatusCode {
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use serde_json::json;
    use std::time::{Duration, Instant, UNIX_EPOCH};

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
        DecodingKey::from_rsa_pem(TEST_RSA_PUBLIC_KEY_PEM).expect("test public key is valid")
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
        assert!(
            err.is_err(),
            "expired token should be rejected; got: {err:?}"
        );
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
        let token =
            jsonwebtoken::encode(&header, &claims, &hmac_key).expect("HMAC token encoding failed");

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
        let token = encode(&header, &claims, &encoding_key()).expect("test JWT encoding failed");

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
        let token = encode(&header, &claims, &encoding_key()).expect("test JWT encoding failed");

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
        assert!(
            err.is_err(),
            "empty Bearer token should be rejected; got: {err:?}"
        );
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

    // -----------------------------------------------------------------------
    // Activity → InboundMessage mapping tests
    // -----------------------------------------------------------------------

    /// Personal (DM) message: conversation_id, content, sender, metadata.
    #[test]
    fn test_activity_to_inbound_personal_dm() {
        let raw = r#"{
            "type": "message",
            "id": "act-001",
            "text": "Hello bot!",
            "from": { "id": "user-aaa", "name": "Alice Smith" },
            "conversation": { "id": "conv-dm-001", "conversationType": "personal" },
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "recipient": { "id": "bot-bbb", "name": "MyBot" },
            "channelData": {},
            "entities": []
        }"#;

        let activity: Activity = serde_json::from_str(raw).expect("parse activity");
        let msg = activity_to_inbound(&activity, "teams")
            .expect("should produce InboundMessage for message activity");

        assert_eq!(msg.id, "act-001");
        assert_eq!(msg.source, "teams");
        assert_eq!(msg.adapter, Some("teams".to_string()));
        assert_eq!(msg.conversation_id, "teams:conv-dm-001");
        assert_eq!(msg.sender_id, "user-aaa");
        assert_eq!(msg.formatted_author, Some("Alice Smith".to_string()));

        // Content text should be unchanged (no <at> tags).
        if let crate::MessageContent::Text(text) = &msg.content {
            assert_eq!(text, "Hello bot!");
        } else {
            panic!("expected Text content");
        }

        // Metadata: serviceUrl and conversationType.
        assert_eq!(
            msg.metadata
                .get("teams_service_url")
                .and_then(|v| v.as_str()),
            Some("https://smba.trafficmanager.net/amer/")
        );
        assert_eq!(
            msg.metadata
                .get("teams_conversation_type")
                .and_then(|v| v.as_str()),
            Some("personal")
        );
        // No mention in DM (no <at> tag and no mention entity for bot-bbb).
        assert_eq!(
            msg.metadata.get("teams_mentioned").and_then(|v| v.as_str()),
            Some("false")
        );
        // message_id metadata.
        assert_eq!(
            msg.metadata
                .get(crate::metadata_keys::MESSAGE_ID)
                .and_then(|v| v.as_str()),
            Some("act-001")
        );
    }

    /// Channel @mention: <at> tag is stripped, teams_mentioned == "true".
    #[test]
    fn test_activity_to_inbound_channel_mention_strips_at_tag() {
        let raw = r#"{
            "type": "message",
            "id": "act-002",
            "text": "<at>MyBot</at> hello world",
            "from": { "id": "user-bbb", "name": "Bob Jones" },
            "conversation": { "id": "conv-ch-999", "conversationType": "channel" },
            "serviceUrl": "https://smba.trafficmanager.net/emea/",
            "recipient": { "id": "bot-bbb", "name": "MyBot" },
            "channelData": {},
            "entities": [
                {
                    "type": "mention",
                    "mentioned": { "id": "bot-bbb", "name": "MyBot" },
                    "text": "<at>MyBot</at>"
                }
            ]
        }"#;

        let activity: Activity = serde_json::from_str(raw).expect("parse activity");
        let msg = activity_to_inbound(&activity, "teams").expect("should produce InboundMessage");

        // <at>MyBot</at> prefix should be stripped; remaining text trimmed.
        if let crate::MessageContent::Text(text) = &msg.content {
            assert_eq!(text, "hello world");
        } else {
            panic!("expected Text content");
        }

        assert_eq!(
            msg.metadata.get("teams_mentioned").and_then(|v| v.as_str()),
            Some("true")
        );
        assert_eq!(msg.conversation_id, "teams:conv-ch-999");
    }

    /// Non-message activity type returns None.
    #[test]
    fn test_activity_to_inbound_non_message_returns_none() {
        let raw = r#"{
            "type": "conversationUpdate",
            "id": "act-003",
            "from": { "id": "user-ccc", "name": "Carol" },
            "conversation": { "id": "conv-xyz", "conversationType": "channel" },
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "recipient": { "id": "bot-ddd", "name": "MyBot" },
            "channelData": {}
        }"#;

        let activity: Activity = serde_json::from_str(raw).expect("parse activity");
        let result = activity_to_inbound(&activity, "teams");
        assert!(
            result.is_none(),
            "conversationUpdate should produce None, got: {result:?}"
        );
    }

    /// Named-instance runtime_key rewrites the conversation_id prefix.
    #[test]
    fn test_activity_to_inbound_named_runtime_key() {
        let raw = r#"{
            "type": "message",
            "id": "act-004",
            "text": "ping",
            "from": { "id": "user-ddd", "name": "Dave" },
            "conversation": { "id": "conv-named-001" },
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "recipient": { "id": "bot-eee", "name": "MyBot" },
            "channelData": {}
        }"#;

        let activity: Activity = serde_json::from_str(raw).expect("parse activity");
        let msg =
            activity_to_inbound(&activity, "teams:support").expect("should produce InboundMessage");

        // Named adapter: runtime_key != "teams", so prefix should be rewritten.
        assert_eq!(msg.conversation_id, "teams:support:conv-named-001");
        assert_eq!(msg.adapter, Some("teams:support".to_string()));
    }

    // -----------------------------------------------------------------------
    // TeamsAdapter unit tests
    // -----------------------------------------------------------------------

    use crate::config::TeamsPermissions;

    /// `TeamsAdapter::name()` returns the runtime key supplied at construction.
    #[test]
    fn adapter_name_returns_runtime_key() {
        let perms = Arc::new(arc_swap::ArcSwap::from_pointee(TeamsPermissions::default()));
        let adapter = TeamsAdapter::new(
            "teams:ops",
            "app-id",
            "secret",
            "common",
            3979,
            "0.0.0.0",
            perms,
        )
        .expect("TeamsAdapter::new");
        assert_eq!(adapter.name(), "teams:ops");
    }

    // -----------------------------------------------------------------------
    // TeamsPermissions::is_allowed tests
    // -----------------------------------------------------------------------

    /// DM sender in dm_allowed_users → allowed.
    #[test]
    fn permission_dm_allowed_user_is_permitted() {
        let perms = TeamsPermissions {
            channel_filter: None,
            dm_allowed_users: vec!["user-aad-123".to_string()],
        };
        assert!(
            perms.is_allowed(Some("personal"), "user-aad-123", "conv-dm-001"),
            "listed DM user should be allowed"
        );
    }

    /// DM sender NOT in dm_allowed_users → denied.
    #[test]
    fn permission_dm_unknown_user_is_denied() {
        let perms = TeamsPermissions {
            channel_filter: None,
            dm_allowed_users: vec!["user-aad-123".to_string()],
        };
        assert!(
            !perms.is_allowed(Some("personal"), "other-user", "conv-dm-001"),
            "unlisted DM user should be denied"
        );
    }

    /// Empty dm_allowed_users → all DMs blocked.
    #[test]
    fn permission_empty_dm_list_blocks_all_dms() {
        let perms = TeamsPermissions {
            channel_filter: None,
            dm_allowed_users: vec![],
        };
        assert!(
            !perms.is_allowed(Some("personal"), "any-user", "conv-dm-001"),
            "empty dm_allowed_users should block all DMs"
        );
    }

    /// No channel_filter → all channels accepted.
    #[test]
    fn permission_no_channel_filter_allows_any_channel() {
        let perms = TeamsPermissions {
            channel_filter: None,
            dm_allowed_users: vec![],
        };
        assert!(
            perms.is_allowed(Some("channel"), "user-x", "any-channel-id"),
            "None channel_filter should allow all channels"
        );
    }

    /// channel_filter present and channel in list → allowed.
    #[test]
    fn permission_channel_filter_allows_listed_channel() {
        let perms = TeamsPermissions {
            channel_filter: Some(vec!["ch-allowed".to_string()]),
            dm_allowed_users: vec![],
        };
        assert!(
            perms.is_allowed(Some("channel"), "user-x", "ch-allowed"),
            "channel in filter list should be allowed"
        );
    }

    /// channel_filter present but channel NOT in list → denied.
    #[test]
    fn permission_channel_filter_denies_unlisted_channel() {
        let perms = TeamsPermissions {
            channel_filter: Some(vec!["ch-allowed".to_string()]),
            dm_allowed_users: vec![],
        };
        assert!(
            !perms.is_allowed(Some("channel"), "user-x", "ch-other"),
            "channel absent from filter list should be denied"
        );
    }

    /// `conversation_type = None` falls through to channel path → filter applies.
    #[test]
    fn permission_none_conversation_type_treated_as_channel() {
        let perms = TeamsPermissions {
            channel_filter: Some(vec!["ch-ok".to_string()]),
            dm_allowed_users: vec![],
        };
        // None type + allowed channel → allowed.
        assert!(perms.is_allowed(None, "user-x", "ch-ok"));
        // None type + disallowed channel → denied.
        assert!(!perms.is_allowed(None, "user-x", "ch-other"));
    }

    // -----------------------------------------------------------------------
    // serviceUrl capture helper (inline test of the map logic)
    // -----------------------------------------------------------------------

    /// Inserting a conversation's serviceUrl into the map and reading it back.
    #[tokio::test]
    async fn service_urls_map_captures_and_retrieves_url() {
        let map: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let conv_id = "teams:conv-abc".to_string();
        let url = "https://smba.trafficmanager.net/amer/";

        map.lock().await.insert(conv_id.clone(), url.to_string());

        let stored = map.lock().await.get(&conv_id).cloned();
        assert_eq!(stored.as_deref(), Some(url));
    }

    // -----------------------------------------------------------------------
    // SSRF guard tests
    // -----------------------------------------------------------------------

    #[test]
    fn ssrf_guard_allows_valid_botframework() {
        assert!(
            is_allowed_service_url("https://api.botframework.com/"),
            "api.botframework.com should be allowed"
        );
    }

    #[test]
    fn ssrf_guard_allows_trafficmanager() {
        assert!(
            is_allowed_service_url("https://smba.trafficmanager.net/amer/"),
            "smba.trafficmanager.net should be allowed"
        );
    }

    #[test]
    fn ssrf_guard_rejects_http() {
        assert!(
            !is_allowed_service_url("http://smba.trafficmanager.net/"),
            "http scheme should be rejected"
        );
    }

    #[test]
    fn ssrf_guard_rejects_evil_host() {
        assert!(
            !is_allowed_service_url("https://evil.example.com"),
            "unrelated host should be rejected"
        );
    }

    #[test]
    fn ssrf_guard_rejects_evil_suffix() {
        assert!(
            !is_allowed_service_url("https://botframework.com.evil.com"),
            "botframework.com.evil.com should be rejected (evil suffix)"
        );
    }

    #[test]
    fn ssrf_guard_rejects_invalid_url() {
        assert!(
            !is_allowed_service_url("not-a-url"),
            "invalid URL should be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // is_supported broadcast variant tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_supported_text_is_true() {
        fn is_supported(r: &OutboundResponse) -> bool {
            matches!(r, OutboundResponse::Text(_))
        }
        assert!(
            is_supported(&OutboundResponse::Text("hello".to_string())),
            "Text should be supported for broadcast"
        );
    }

    #[test]
    fn is_supported_other_is_false() {
        fn is_supported(r: &OutboundResponse) -> bool {
            matches!(r, OutboundResponse::Text(_))
        }
        assert!(
            !is_supported(&OutboundResponse::Reaction("👍".to_string())),
            "Reaction should not be supported for broadcast"
        );
    }

    // -----------------------------------------------------------------------
    // Conversation ID strip tests
    // -----------------------------------------------------------------------

    #[test]
    fn conv_id_strip_default() {
        let conv_id = "teams:conv-abc";
        let runtime_key = "teams";
        let bare = conv_id
            .strip_prefix(&format!("{runtime_key}:"))
            .unwrap_or(conv_id);
        assert_eq!(bare, "conv-abc");
    }

    #[test]
    fn conv_id_strip_named() {
        let conv_id = "teams:prod:conv-abc";
        let runtime_key = "teams:prod";
        let bare = conv_id
            .strip_prefix(&format!("{runtime_key}:"))
            .unwrap_or(conv_id);
        assert_eq!(bare, "conv-abc");
    }

    #[test]
    fn conv_id_strip_colon_in_id() {
        // Inner colons in the MS id must be preserved.
        let conv_id = "teams:conv:abc:def";
        let runtime_key = "teams";
        let bare = conv_id
            .strip_prefix(&format!("{runtime_key}:"))
            .unwrap_or(conv_id);
        assert_eq!(bare, "conv:abc:def");
    }

    // -----------------------------------------------------------------------
    // strip_at_mentions — attributed <at id="…"> variant (FIX 4)
    // -----------------------------------------------------------------------

    /// Teams sends `<at id="0">BotName</at>` when the mention has an id
    /// attribute.  strip_at_mentions must strip those spans too.
    #[test]
    fn strip_at_mentions_attributed_tag() {
        let raw = r#"<at id="0">Bot</at> hello"#;
        let stripped = strip_at_mentions(raw);
        assert_eq!(
            stripped, "hello",
            "attributed <at id=\"0\"> should be stripped"
        );
    }

    /// bot_was_mentioned text-fallback must fire for `<at id="0">` when
    /// entities are absent (exercises the `contains("<at")` path).
    #[test]
    fn bot_was_mentioned_attributed_fallback() {
        // Build an activity with NO mention entities, only the attributed tag in text.
        let raw = r#"{
            "type": "message",
            "id": "act-attr-001",
            "text": "<at id=\"0\">Bot</at> hello",
            "from": { "id": "user-x", "name": "X" },
            "conversation": { "id": "conv-attr", "conversationType": "channel" },
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "recipient": { "id": "bot-attr", "name": "Bot" },
            "channelData": {}
        }"#;
        let activity: Activity = serde_json::from_str(raw).expect("parse activity");
        assert!(
            bot_was_mentioned(&activity),
            "attributed <at id=\"0\"> tag should trigger mentioned=true via text fallback"
        );
        // Also verify the text is stripped correctly end-to-end.
        let msg = activity_to_inbound(&activity, "teams").expect("should produce InboundMessage");
        if let crate::MessageContent::Text(text) = &msg.content {
            assert_eq!(text, "hello");
        } else {
            panic!("expected Text content");
        }
        assert_eq!(
            msg.metadata.get("teams_mentioned").and_then(|v| v.as_str()),
            Some("true")
        );
    }

    // -----------------------------------------------------------------------
    // SSRF guard — additional regression cases (FIX extra)
    // -----------------------------------------------------------------------

    #[test]
    fn ssrf_guard_rejects_userinfo() {
        // Userinfo in URL can be used to bypass naive host checks.
        assert!(
            !is_allowed_service_url("https://botframework.com@evil.com/foo"),
            "userinfo-based bypass must be rejected"
        );
    }

    #[test]
    fn ssrf_guard_rejects_metadata_ip() {
        assert!(
            !is_allowed_service_url("https://169.254.169.254/latest/meta-data/"),
            "link-local metadata IP must be rejected"
        );
    }

    #[test]
    fn ssrf_guard_rejects_localhost() {
        assert!(
            !is_allowed_service_url("https://localhost/"),
            "localhost must be rejected"
        );
    }

    #[test]
    fn ssrf_guard_accepts_real_trafficmanager() {
        assert!(
            is_allowed_service_url("https://smba.trafficmanager.net/amer/"),
            "real trafficmanager.net URL must be accepted"
        );
    }

    // -----------------------------------------------------------------------
    // Map-key fix test
    // -----------------------------------------------------------------------

    #[test]
    fn map_key_named_instance() {
        use crate::messaging::apply_runtime_adapter_to_conversation_id;

        let runtime_key = "teams:prod";
        let ms_id = "conv-abc";
        let base = format!("teams:{ms_id}");
        let rewritten = apply_runtime_adapter_to_conversation_id(runtime_key, base);

        assert_eq!(
            rewritten, "teams:prod:conv-abc",
            "named instance should produce teams:prod:conv-abc, not teams:conv-abc"
        );
    }
}
