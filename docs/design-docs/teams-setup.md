# Microsoft Teams Adapter Setup

Connect a spacebot agent to Microsoft Teams via the Azure Bot Service / Bot Framework.

**Time estimate:** 20–30 minutes.

**Prerequisites:**

- A running spacebot instance reachable from the public internet (or via a tunnel for dev).
- An Azure subscription with permission to create app registrations and Azure Bot resources.
- A Microsoft 365 tenant with Teams, and (for sideloading) admin permission to enable custom app upload.

---

## Overview

Teams bots are not webhook-registered with a URL you paste into a developer portal — they work the other way around. Azure Bot Service acts as an intermediary relay: Teams delivers messages to Azure, and Azure forwards them to your bot's **public HTTPS endpoint** as signed HTTP POST requests.

The flow is:

```
Teams user  →  Azure Bot Service  →  POST https://<your-host>/api/messages  →  spacebot
spacebot    →  POST https://<serviceUrl>/v3/conversations/...              →  Azure Bot Service  →  Teams user
```

spacebot exposes a plain HTTP server (default `0.0.0.0:3979`). Because Azure requires HTTPS, you **must** put a TLS-terminating reverse proxy in front — see [Public HTTPS reverse proxy (required)](#public-https-reverse-proxy-required) below.

---

## Step 1: Azure AD app registration

The app registration is the bot's identity in Azure AD. It provides the App ID (client ID) and client secret that spacebot uses both to validate inbound JWTs and to mint outbound Bot Connector tokens.

1. Go to the [Azure portal](https://portal.azure.com/) → **Microsoft Entra ID** → **App registrations** → **New registration**.

2. Fill in:
   - **Name:** a display name for the bot (e.g. "My Spacebot").
   - **Supported account types:** choose **Accounts in this organizational directory only (Single tenant)** for a private/internal bot. Use "Any Azure AD directory" only if you need multi-tenant.

   > **Note on multi-tenant deprecation:** Microsoft deprecated new multi-tenant bot creation after July 31, 2025. For new deployments use single-tenant or user-assigned managed identity. Existing multi-tenant bots continue to work. Verify the current policy at [learn.microsoft.com/azure/bot-service/bot-service-quickstart-registration](https://learn.microsoft.com/en-us/azure/bot-service/bot-service-quickstart-registration).

3. Click **Register**.

4. On the **Overview** page, copy:
   - **Application (client) ID** → this is your `app_id`.
   - **Directory (tenant) ID** → this is your `tenant_id`.

5. Go to **Certificates & secrets** → **New client secret**. Give it a description and expiry. Click **Add**. Copy the **Value** immediately (it is only shown once) → this is your `client_secret`.

---

## Step 2: Azure Bot resource

The Azure Bot resource registers the messaging endpoint with the Bot Connector service and links the app registration to the Teams channel.

1. In the Azure portal, click **Create a resource**, search for **Azure Bot**, and select it.

2. Fill in:
   - **Bot handle:** unique name (alphanumeric + hyphens).
   - **Subscription / Resource group:** as appropriate.
   - **Microsoft App ID:** select **Use existing app registration** and paste the Application (client) ID from Step 1.
   - **App type:** **Single Tenant** (matching the app registration above).
   - **App Tenant ID:** paste the Directory (tenant) ID from Step 1.

3. Click **Review + create** → **Create**.

4. Once deployed, open the Azure Bot resource → **Configuration**:
   - Set **Messaging endpoint** to:
     ```
     https://<your-public-host>/api/messages
     ```
     Replace `<your-public-host>` with the domain pointing at your reverse proxy. See [Public HTTPS reverse proxy (required)](#public-https-reverse-proxy-required).
   - Click **Apply**.

5. Still in the Azure Bot resource, go to **Channels** → click **Microsoft Teams**:
   - Read and accept the terms of service.
   - On the **Messaging** tab, select the appropriate cloud (Commercial, GCC, etc.).
   - Click **Apply**.

---

## Step 3: Teams app manifest

The Teams app manifest is a zip package that tells Teams about your bot. You upload this package to make the bot available in Teams.

### Build the package

Create a directory with three files:

```
my-teams-bot/
├── manifest.json
├── icon-color.png      (192×192 px, full-color PNG)
└── icon-outline.png    (32×32 px, transparent PNG with white/transparent icon)
```

#### Minimal `manifest.json`

```json
{
  "$schema": "https://developer.microsoft.com/json-schemas/teams/v1.29/MicrosoftTeams.schema.json",
  "manifestVersion": "1.29",
  "version": "1.0.0",
  "id": "<YOUR-APP-ID-UUID>",
  "packageName": "com.example.myspacebot",
  "name": {
    "short": "My Spacebot",
    "full": "My Spacebot — AI assistant"
  },
  "description": {
    "short": "AI bot powered by spacebot.",
    "full": "An AI assistant powered by spacebot, connected to Microsoft Teams."
  },
  "icons": {
    "color": "icon-color.png",
    "outline": "icon-outline.png"
  },
  "developer": {
    "name": "Your Name / Org",
    "websiteUrl": "https://example.com",
    "privacyUrl": "https://example.com/privacy",
    "termsOfUseUrl": "https://example.com/terms"
  },
  "bots": [
    {
      "botId": "<YOUR-APP-ID-UUID>",
      "scopes": ["personal", "team", "groupChat"]
    }
  ]
}
```

- Replace `<YOUR-APP-ID-UUID>` (both `id` and `bots[0].botId`) with the **Application (client) ID** from Step 1.
- `scopes`: `personal` = DMs; `team` = channel @mentions; `groupChat` = group chat messages.

> **Schema version note:** The example uses schema version `1.29` (current as of June 2026). Verify the latest version at [learn.microsoft.com/microsoftteams/platform/resources/schema/manifest-schema](https://learn.microsoft.com/en-us/microsoftteams/platform/resources/schema/manifest-schema) before packaging.

#### Zip the package

```sh
cd my-teams-bot
zip -r ../my-teams-bot.zip .
```

The zip file must contain `manifest.json` at the root (not inside a subdirectory).

### Upload to Teams (sideloading)

For a private/internal bot without publishing to the Teams Store:

**Enable sideloading (admin, one-time):**

1. Sign in to the [Teams admin center](https://admin.teams.microsoft.com/).
2. Go to **Teams apps** → **Setup Policies** → **Global**.
3. Toggle **Upload custom apps** → **On** → **Save**.
4. Go to **Teams apps** → **Manage apps** → **Actions** → **Org-wide app settings** → enable **Let users interact with custom apps in preview**.

   > Allow up to 24 hours for the policy change to propagate.

**Upload the package:**

In Microsoft Teams (desktop or web):
1. Go to **Apps** (left sidebar) → **Manage your apps** → **Upload an app** → **Upload a custom app**.
2. Select `my-teams-bot.zip`.
3. Confirm the install dialog.

For organization-wide rollout, upload via **Teams admin center** → **Teams apps** → **Manage apps** → **Upload new app** instead of per-user sideloading.

---

## Public HTTPS reverse proxy (required)

spacebot's Teams adapter binds a plain HTTP server (default `0.0.0.0:3979`). Azure Bot Service **requires HTTPS** and will not deliver messages to a plain HTTP endpoint. A TLS-terminating reverse proxy is a hard prerequisite — without it the bot cannot receive any messages from Teams.

```
Internet / Azure Bot Service
     │  HTTPS :443
     ▼
[ Reverse proxy (Caddy / nginx / cloudflared) ]
     │  HTTP
     ▼
spacebot Teams adapter  →  localhost:3979  (plain HTTP)
```

### Caddy example (production)

```
your-bot-domain.example.com {
    reverse_proxy /api/messages localhost:3979
    reverse_proxy /health       localhost:3979
}
```

Caddy automatically provisions a Let's Encrypt TLS certificate. Replace `your-bot-domain.example.com` with the domain you set as the messaging endpoint in Step 2.

### Dev/testing: cloudflared tunnel

For local development you need a public HTTPS URL before you have a real domain:

```sh
# Install cloudflared, then:
cloudflared tunnel --url http://localhost:3979
```

cloudflared prints a `https://<random>.trycloudflare.com` URL. Use that as the messaging endpoint in the Azure Bot resource during testing. Update the endpoint each time the tunnel is restarted (the URL changes). `ngrok` works the same way.

> End-to-end testing requires a public endpoint — the bot cannot receive messages from Teams over a non-routable localhost address.

---

## Step 4: spacebot config

### Environment variables (recommended for secrets)

Set these before starting spacebot:

```sh
export TEAMS_APP_ID="<application-client-id>"
export TEAMS_CLIENT_SECRET="<client-secret-value>"
export TEAMS_TENANT_ID="<directory-tenant-id>"
```

When these env vars are set, they take precedence over the TOML values for the corresponding fields. Never commit the client secret in `config.toml` — use the env var or a secrets manager.

### Default (single) instance

```toml
[messaging.teams]
enabled = true

# Credentials — prefer env vars for the secret (see above).
# Env vars TEAMS_APP_ID, TEAMS_CLIENT_SECRET, TEAMS_TENANT_ID override these.
app_id        = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
client_secret = "env:TEAMS_CLIENT_SECRET"   # reference env var; never hardcode
tenant_id     = "yyyyyyyy-yyyy-yyyy-yyyy-yyyyyyyyyyyy"

# Listener settings (defaults shown — omit if unchanged).
port = 3979
bind = "0.0.0.0"

# DMs: list the Teams user IDs (e.g. "29:xxx" object IDs) that may DM the bot.
# Leave empty to ignore all DMs.
dm_allowed_users = ["29:AbCdEfGhIj1234567890"]

[[bindings]]
agent_id = "main"
channel  = "teams"
# No further filter = catches all Teams traffic routed to the default instance.
```

### Named instances (multi-bot deployments)

> **v1 limitation — single Teams bot per instance.** Unlike Slack (which connects outbound), each Teams adapter binds its **own** inbound HTTP listener, and `[[messaging.teams.instances]]` has no per-instance `port`. A default instance plus one or more named instances would therefore all try to bind the *same* `port` and collide (only the first binds; the others fail to start). **v1 supports a single Teams bot — the default `[messaging.teams]` instance.** Multi-bot Teams (one shared listener that demuxes by the inbound JWT audience / `app_id`, or per-instance ports) is a planned enhancement. The config below is shown for forward-compatibility but is **not yet functional**; do not configure named Teams instances in v1.

Named instances follow the config pattern of other adapters (see `docs/design-docs/named-messaging-adapters.md`); the planned routing distinguishes instances by their own `app_id` (the inbound JWT audience check):

```toml
[messaging.teams]
enabled = true
# Default instance credentials
app_id        = "aaaa-..."
client_secret = "env:TEAMS_CLIENT_SECRET"
tenant_id     = "bbbb-..."
port          = 3979
bind          = "0.0.0.0"

[[messaging.teams.instances]]
name          = "support-bot"
enabled       = true
app_id        = "cccc-..."
client_secret = "env:TEAMS_SUPPORT_CLIENT_SECRET"
tenant_id     = "dddd-..."
dm_allowed_users = ["29:SupportAgentUserId"]

[[bindings]]
agent_id = "main"
channel  = "teams"
# adapter omitted → default instance

[[bindings]]
agent_id = "support-agent"
channel  = "teams"
adapter  = "support-bot"
# Routes traffic from the support-bot app registration to the support-agent.
```

For the full binding syntax and semantics (filters, adapter scoping) see `docs/design-docs/named-messaging-adapters.md`.

---

## Step 5: Verify

### 1. Health endpoint

Once spacebot is running and the reverse proxy is in place:

```sh
curl https://your-bot-domain.example.com/health
# Expected: 200 OK
```

This confirms the proxy is correctly forwarding to spacebot's listener.

### 2. DM the bot

In Microsoft Teams, find the bot by name (via the app you sideloaded), open a chat, and send a message. If the bot responds, the full end-to-end path (Teams → Azure Bot Service → HTTPS endpoint → spacebot → Azure Bot Service → Teams) is working.

Note: the user's Teams object ID must be in `dm_allowed_users` (or the list may be empty to deny all DMs — in v1 the bot ignores DMs from users not in that list).

### 3. @mention in a channel

Add the bot to a Teams channel by @mentioning it in a post. If `team` is in the bot's scopes (manifest `bots[].scopes`), it will receive the message and reply.

### What is not in v1

The following are not supported in the current Teams adapter — text messages and @mentions only:

- Slash commands / message extensions
- Adaptive cards (rendered card output)
- File/attachment upload or download
- Typing indicators
- Streaming responses

---

## Security notes

- **Inbound JWT validation:** every incoming `POST /api/messages` request carries a Bearer JWT issued by Azure Bot Service. spacebot validates the JWT against the Bot Framework JWKS endpoint (`https://login.botframework.com/v1/.well-known/openidconfiguration`), enforcing issuer `https://api.botframework.com` and audience = your `app_id`. Requests with invalid or missing tokens are rejected with 401.

- **Outbound SSRF allowlist:** spacebot only POSTs outbound replies to URLs whose host ends with `.botframework.com` or `.trafficmanager.net`. Attacker-supplied `serviceUrl` values pointing elsewhere are rejected before any network call is made.

- **Client secret handling:** keep `client_secret` in an environment variable (`TEAMS_CLIENT_SECRET`) or a secrets manager. Do not write it in `config.toml` in plaintext and do not commit it to source control.

- **Reverse proxy:** spacebot's listener is plain HTTP and should never be exposed directly to the internet. Bind it to `127.0.0.1` if your proxy is on the same host, or to a private network interface if it is not, and let the proxy handle TLS termination.

- **Rotate secrets:** Azure AD client secrets have an expiry. Set a calendar reminder before expiry; rotation requires updating the secret in the Azure portal and redeploying with the new value. The bot will stop working the moment the secret expires.
