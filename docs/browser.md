# Browser

Browser automation for Spacebot workers via headless Chrome. Workers can navigate websites, interact with page elements, take screenshots, and extract content. Built on [chromiumoxide](https://github.com/mattsse/chromiumoxide), which drives Chrome over the DevTools Protocol (CDP).

## Why Worker-Only

Browser use is restricted to workers. Channels never touch a browser. This follows the delegation model -- the channel spawns a worker with a task like "go to example.com, find the pricing table, and screenshot it." The worker launches Chrome, does the work, returns a summary. The channel stays responsive.

Giving browser tools to channels or branches would violate the architecture. Channels talk to users. Branches think. Workers do things.

## The Ref System

The core pattern for LLM-browser interaction is ref-based element addressing. Instead of asking the LLM to write CSS selectors (fragile, hallucination-prone), we give it a structured view of the page with short element references.

```
1. Worker calls snapshot
2. Tool builds the page's accessibility tree via CDP
3. Interactive elements get refs: e0, e1, e2, ...
4. Worker receives a list like:
     e0: [textbox]  "Search"
     e1: [button]   "Submit"
     e2: [link]     "Sign In"
5. Worker acts using refs: act { kind: "click", element_ref: "e1" }
```

Refs reset on every `snapshot` call and on navigation. The LLM should snapshot before interacting with a new page state.

Only interactive ARIA roles get refs assigned: button, checkbox, combobox, link, listbox, menu, menuitem, option, radio, searchbox, slider, switch, tab, textbox, and a few others. Static content (headings, paragraphs, images) is excluded to keep the ref list focused. Max 200 refs per snapshot.

Under the hood, refs map to `[role='...'][aria-label='...']` CSS selectors for element re-resolution on the actual DOM.

## Tool Actions

Single `browser` tool with an `action` discriminator. All actions share one argument struct.

### Session Lifecycle

| Action | Required Args | Description |
|--------|--------------|-------------|
| `launch` | -- | Start Chrome. Must be called first. |
| `close` | -- | Shut down Chrome and clean up all state. |

### Navigation

| Action | Required Args | Description |
|--------|--------------|-------------|
| `navigate` | `url` | Go to a URL in the active tab. Clears element refs. |
| `content` | -- | Get the page HTML. Truncated at 100KB for LLM context. |

### Tabs

| Action | Required Args | Description |
|--------|--------------|-------------|
| `open` | -- | Open a new tab. Optional `url` (defaults to about:blank). |
| `tabs` | -- | List all open tabs with target IDs, titles, URLs. |
| `focus` | `target_id` | Switch active tab. |
| `close_tab` | -- | Close a tab by `target_id`, or the active tab if omitted. |

### Observation

| Action | Required Args | Description |
|--------|--------------|-------------|
| `snapshot` | -- | Get accessibility tree with element refs. |
| `screenshot` | -- | Capture viewport (or full page with `full_page: true`). Saved to disk. |

`screenshot` also accepts `element_ref` to capture a specific element.

### Interaction

| Action | Required Args | Description |
|--------|--------------|-------------|
| `act` | `act_kind` | Perform an interaction. Most actions require `element_ref`. |

Act kinds:

- **`click`** -- Click an element by ref.
- **`type`** -- Click an element to focus it, then type `text` into it.
- **`press_key`** -- Press a keyboard key (e.g., `Enter`, `Tab`, `Escape`). With `element_ref`, presses on that element. Without, dispatches to the page.
- **`hover`** -- Move the mouse over an element.
- **`scroll_into_view`** -- Scroll an element into the viewport.
- **`focus`** -- Focus an element without clicking.

### JavaScript

| Action | Required Args | Description |
|--------|--------------|-------------|
| `evaluate` | `script` | Execute JavaScript and return the result. Disabled by default. |

`evaluate` is gated behind `evaluate_enabled` in config. Disabled by default because arbitrary JS execution in an LLM-controlled browser is a security surface.

## Typical Workflow

```
browser { action: "launch" }
browser { action: "navigate", url: "https://example.com" }
browser { action: "snapshot" }
  → e0: [textbox] "Email", e1: [textbox] "Password", e2: [button] "Sign In"
browser { action: "act", act_kind: "click", element_ref: "e0" }
browser { action: "act", act_kind: "type", element_ref: "e0", text: "user@example.com" }
browser { action: "act", act_kind: "click", element_ref: "e1" }
browser { action: "act", act_kind: "type", element_ref: "e1", text: "hunter2" }
browser { action: "act", act_kind: "click", element_ref: "e2" }
browser { action: "snapshot" }
  → (new page state after login)
browser { action: "screenshot" }
browser { action: "close" }
```

## Screenshots

Screenshots are saved as timestamped PNGs to the agent's screenshot directory:

```
~/.spacebot/agents/main/data/screenshots/screenshot_20260212_143052_123.png
```

The file path is returned in the tool output so the worker can reference it in its summary. The directory is configurable via `screenshot_dir` in the browser config, defaulting to `{data_dir}/screenshots`.

## Configuration

Browser config lives in `config.toml` under `[defaults.browser]` (or per-agent override):

```toml
[defaults.browser]
enabled = true            # include browser tool in worker ToolServers
headless = true           # run Chrome without a visible window
evaluate_enabled = false  # allow JavaScript evaluation via the tool
executable_path = ""      # custom Chrome binary path (auto-detected if empty)
screenshot_dir = ""       # override screenshot storage location
```

Per-agent override:

```toml
[[agents]]
id = "web-scraper"

[agents.browser]
evaluate_enabled = true   # this agent's workers can run JS
headless = false          # show the browser window for debugging
```

When `enabled = false`, the browser tool is not registered on worker ToolServers. Workers for that agent won't see it in their available tools.

## Architecture

```
Worker (Rig Agent)
  │
  ├── shell, file, exec, set_status   (standard worker tools)
  │
  └── browser                          (BrowserTool)
        │
        ├── Arc<Mutex<BrowserState>>   (shared across tool invocations)
        │     ├── Browser              (chromiumoxide handle)
        │     ├── pages: HashMap       (target_id → Page)
        │     ├── active_target        (current tab)
        │     └── element_refs         (snapshot ref → ElementRef)
        │
        └── Config
              ├── headless
              ├── evaluate_enabled
              └── screenshot_dir
```

Each worker gets its own `BrowserTool` instance with its own `BrowserState`. The state is behind `Arc<Mutex<>>` because the Rig tool trait requires `Clone`. The Chrome process (and its CDP WebSocket handler task) live for the lifetime of the worker.

The CDP handler runs as a background tokio task that polls the WebSocket stream. It's spawned during `launch` and dropped when the browser closes or the worker completes.

## Implementation

Source: `src/tools/browser.rs`

Key types:

- `BrowserTool` -- implements `rig::tool::Tool`. Holds config and shared state.
- `BrowserState` -- mutable state behind the mutex: browser handle, pages, refs.
- `ElementRef` -- stored info about a snapshotted element (role, name, AX node ID, backend DOM node ID).
- `BrowserAction` -- enum discriminator for which action to perform.
- `ActKind` -- enum for interaction types (click, type, press_key, etc.).
- `BrowserArgs` / `BrowserOutput` -- tool input/output types.

Element resolution: refs map to CSS selectors built from `[role='...']` and `[aria-label='...']` attributes. This works for most standard web content. Pages with non-standard ARIA markup may need the `evaluate` action for custom DOM queries.

## Limitations

- **No file upload/download** -- chromiumoxide doesn't expose file chooser interception. Use `shell` + `curl` for downloads.
- **No network interception** -- request blocking and response modification aren't exposed through the tool, though chromiumoxide supports it via raw CDP commands.
- **No cookie/storage management** -- not exposed as tool actions. Could be added if needed.
- **Single browser per worker** -- each worker gets one Chrome process. No connection pooling across workers.
- **Selector fragility** -- the `[role][aria-label]` selector strategy works for well-structured pages but can fail on pages with missing ARIA attributes. The `content` action + `evaluate` (when enabled) serve as fallbacks.
