# Microsoft Teams Adapter Setup

Connect a spacebot agent to Microsoft Teams via the Azure Bot Service / Bot Framework.

> **Battle-tested:** this guide was walked end-to-end against a live tenant on 2026-06-26 (real Azure Bot + sideloaded app + a real DM round-trip). The ⚠️ callouts are the actual snags hit along the way.

**Time estimate:** 30–45 minutes (more if you're creating an Azure subscription for the first time).

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
   - **Subscription / Resource group:** as appropriate. **⚠️ A Microsoft 365 tenant does NOT come with an Azure subscription** — Azure billing is separate. If you have none, the create blade can't proceed: go to **Subscriptions → + Add → Pay-As-You-Go** first (the subscription itself is free; a card is required for identity verification but isn't charged on free tiers).
   - **Pricing tier:** click **Change plan** and select **F0 (Free)**. The default is often **S1 (paid)**. **F0 covers standard channels — including Teams — with unlimited messages, at $0.** Verify the estimated cost shows 0; avoid S1.
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

#### Minimal `manifest.json` (battle-tested)

```json
{
  "$schema": "https://developer.microsoft.com/en-us/json-schemas/teams/v1.17/MicrosoftTeams.schema.json",
  "manifestVersion": "1.17",
  "version": "1.0.0",
  "id": "<APP-MANIFEST-GUID>",
  "developer": {
    "name": "Your Org",
    "websiteUrl": "https://example.com",
    "privacyUrl": "https://example.com/privacy",
    "termsOfUseUrl": "https://example.com/terms"
  },
  "icons": { "color": "icon-color.png", "outline": "icon-outline.png" },
  "name": { "short": "My Spacebot", "full": "My Spacebot — AI assistant" },
  "description": {
    "short": "AI bot powered by spacebot.",
    "full": "An AI assistant powered by spacebot, connected to Microsoft Teams."
  },
  "accentColor": "#6264A7",
  "bots": [
    {
      "botId": "<YOUR-APP-ID>",
      "scopes": ["personal", "team", "groupchat"],
      "supportsFiles": false,
      "isNotificationOnly": false
    }
  ],
  "validDomains": []
}
```

> **⚠️ Gotchas that will reject your manifest (learned the hard way — validated 2026-06-26):**
> - **`id` ≠ `botId`.** `id` is the Teams *app* id and must be **its own plain GUID** (generate a fresh one). `bots[0].botId` is your **App (client) ID** from Step 1. Reusing the App ID as `id` gets rejected: *"The manifest product ID could not be parsed. The ID must be a plain GUID."*
> - **No `packageName`.** Manifest schema 1.17 rejects it: *"Property 'packageName' has not been defined and the schema does not allow additional properties."* (Older docs show it — drop it.)
> - **`manifestVersion: "1.17"`** works reliably. `1.29` is not a valid manifest value. Confirm at [manifest schema docs](https://learn.microsoft.com/en-us/microsoftteams/platform/resources/schema/manifest-schema).
> - **`scopes`**: `personal` = DMs, `team` = channel @mentions, `groupchat` = group chats (lowercase).
> - `accentColor` and `validDomains` are required by the schema.

#### Easiest: generate the package with a script

Hand-crafting icons + zip is fiddly (Teams validates icon dimensions, and the zip must have the three files at the **root**). This Python script (stdlib only — no PIL/imagemagick/zip needed) emits a valid package. Set your two GUIDs and run it:

```python
import zlib, struct, json, zipfile, os, uuid
APP_ID   = "<YOUR-APP-ID>"            # from Step 1 (bots[].botId)
APP_GUID = str(uuid.uuid4())          # distinct Teams app id
def png(w, h, px):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        for x in range(w): raw += bytes(px(x, y))
    def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b""))
color   = png(192, 192, lambda x, y: (98, 100, 167, 255))
outline = png(32, 32, lambda x, y: (255, 255, 255, 255) if 6 <= x < 26 and 6 <= y < 26 else (0, 0, 0, 0))
manifest = {
    "$schema": "https://developer.microsoft.com/en-us/json-schemas/teams/v1.17/MicrosoftTeams.schema.json",
    "manifestVersion": "1.17", "version": "1.0.0", "id": APP_GUID,
    "developer": {"name": "Your Org", "websiteUrl": "https://example.com",
                  "privacyUrl": "https://example.com/privacy", "termsOfUseUrl": "https://example.com/terms"},
    "icons": {"color": "icon-color.png", "outline": "icon-outline.png"},
    "name": {"short": "My Spacebot", "full": "My Spacebot — AI assistant"},
    "description": {"short": "AI bot powered by spacebot.", "full": "AI assistant powered by spacebot."},
    "accentColor": "#6264A7",
    "bots": [{"botId": APP_ID, "scopes": ["personal", "team", "groupchat"],
              "supportsFiles": False, "isNotificationOnly": False}],
    "validDomains": [],
}
os.makedirs("pkg", exist_ok=True)
open("pkg/icon-color.png", "wb").write(color)
open("pkg/icon-outline.png", "wb").write(outline)
open("pkg/manifest.json", "w").write(json.dumps(manifest, indent=2))
with zipfile.ZipFile("teams-app.zip", "w", zipfile.ZIP_DEFLATED) as z:
    for f in ("manifest.json", "icon-color.png", "icon-outline.png"): z.write(f"pkg/{f}", f)
print("wrote teams-app.zip (app id", APP_GUID, "/ botId", APP_ID + ")")
```

The resulting `teams-app.zip` has `manifest.json` + both icons at the root, ready to upload.

### (Optional) Validate the manifest first

If upload fails with an opaque error, import the zip into the [Teams Developer Portal](https://dev.teams.microsoft.com) (**Apps → Import app**) — it reports **specific** schema errors (this is how the `packageName`/`id` gotchas above were found). Fix and re-zip until it imports cleanly. (You don't have to *install* from here.)

### Install the app (the path that works — validated 2026-06-26)

**1. Enable custom-app upload (admin, one-time):** [Teams admin center](https://admin.teams.microsoft.com/) → **Teams apps → Setup policies → Global** → **Upload custom apps = On** → Save. (Policy changes can take a few minutes to propagate.)

**2. Upload via Teams:** in the Teams client → **Apps → Manage your apps → Upload an app → Upload a custom app** → select your `teams-app.zip`.

**3. Approve + restrict (admin):** the upload typically triggers **"this app needs admin approval"** / *"Permissions needed — ask your IT admin to add ..."*. As admin:
   - [Teams admin center](https://admin.teams.microsoft.com/) → **Teams apps → Manage apps** → find your app → **Allow** it (status must not be *Blocked*).
   - In the same place (or **Permission policies**), **restrict who can install it**. For a private/internal bot, scope it to **specific users** (e.g. just yourself) so it is **not** exposed org-wide. This keeps the bot invisible to everyone except the users you allow.

   > Allow a few minutes for the approval/restriction to propagate, then retry **Add** in Teams.

**Alternatives:** the **Developer Portal → Preview in Teams** can install it directly for the signed-in account (no admin-center round-trip), but it installs for *that* account — make sure it's the account you'll test from. For org-wide rollout, upload via **Manage apps → Upload new app** and publish.

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

In Microsoft Teams, find the bot by name (via the app you installed), open a chat, and send a message. If the bot responds, the full end-to-end path (Teams → Azure Bot Service → HTTPS endpoint → spacebot → Azure Bot Service → Teams) is working.

**⚠️ DMs are fail-closed.** In v1 the bot **silently ignores** DMs from any user not listed in `dm_allowed_users` (an empty list = all DMs denied). You need the sender's Teams **MRI** (a `29:…` string), which you don't know up front. Two ways:
- **Easiest — @mention in a channel instead.** Channel messages are *not* gated by `dm_allowed_users` (the channel path is open), so adding the bot to a team and `@mentioning` it gives an immediate round-trip without any allowlist.
- **To enable DMs:** send the bot a DM once (it'll be dropped), then read the spacebot log — the drop is logged at debug as `Teams inbound message dropped by permission filter sender_id="29:…"`. Copy that `29:…` value into `dm_allowed_users`, restart, and DM again. (Run with `--debug` to see the line.)

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

---

## Facilitating onboarding (future work)

This setup is ~30–45 min of mostly-manual Azure/Teams clicking. The Azure-side steps (app registration, Azure Bot, Teams admin approval) are inherent to the platform and can't be automated from spacebot without Graph API access + delegated admin consent. But several parts *can* be made easier, roughly in increasing effort:

1. **Ship the manifest generator** (low effort, high value). The Python script above is already a working generator. Promote it to a first-class tool — e.g. a `spacebot teams-manifest --app-id <id> --name "..."` subcommand (or a `scripts/` file) that emits a valid `teams-app.zip` with correct icons and a fresh app GUID. Removes the #1 source of failure (hand-edited manifests: `packageName`/`id`/version).
2. **Pre-fill from the Channels UI** (medium). The v1.2 *Settings → Channels → Teams* form already collects `app_id`/`tenant_id`/secret. It could also offer a **"Download Teams app package"** button that runs the generator server-side and hands back the zip — so the admin never touches JSON.
3. **Guided checklist in the UI** (medium). A step-by-step panel mirroring this doc (with the ⚠️ gotchas inline), plus a **live "/health via your endpoint" probe** and an **"inbound received / JWT validated" indicator** to confirm the Azure wiring before the admin goes hunting.
4. **Publish to the Teams Store** (high effort, only if distributing broadly). Microsoft store validation + review; gives users one-click install but is overkill for internal/self-hosted deployments and adds a review/maintenance burden. Not recommended unless spacebot ships a public Teams app.

**Recommended next:** (1) — turn the generator into a CLI command and link it from this doc; it's small and kills the manifest pitfalls outright.
