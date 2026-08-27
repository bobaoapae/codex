# ChatGPT connector API — live captures (2026-08-27, account `pro`, `joaovitorbor@gmail.com`)

Captured with a `window.fetch` tap on chatgpt.com through the chrome-mcp daemon, using
the cloudflared fallback tunnel. All requests carry:

- `Authorization: Bearer <accessToken from GET /api/auth/session>` (redacted below)
- `OAI-Product-Sku: CONNECTOR_SETTING` (on connector/settings calls; **absent** on the
  `f/conversation` send and on `list_accessible` calls the app fires for the picker,
  which use `SLURM` or no sku — sku is not required for the connector CRUD calls we make)
- The app also sends `OAI-Client-Version`, `OAI-Client-Build-Number`, `OAI-Device-Id`,
  `OAI-Language`, `OAI-Session-Id`, `ChatGPT-Account-ID`, `X-OAI-IS-Client-Observation`.
  **Only `Authorization` + `OAI-Product-Sku: CONNECTOR_SETTING` are needed** for the CRUD
  calls to succeed from a page `fetch` (verified: the API-created connector used only
  those two plus `Content-Type`).

Account id (`ChatGPT-Account-ID`): `b7000e3e-7276-4615-9c4e-563c50a94509`.

---

## Developer Mode toggle (S0) — VERIFIED

Read current state:
```
GET /backend-api/settings/user   (Authorization; OAI-Product-Sku optional)
→ 200 { ..., "settings": { ... } }   // key "developer_mode" appears once set
```
The `developer_mode` flag is **not** present in `settings` until it has been toggled at
least once; after enabling it is reflected there.

Enable (this is exactly what the UI switch fires):
```
PATCH /backend-api/settings/account_user_setting?feature=developer_mode&value=true
  (Authorization; ChatGPT-Account-ID; NO body; value is the query string, as a string)
→ 200 {"developer_mode":true}
```
Disable: same with `value=false`.

Gate proof: with Developer Mode OFF,
`GET /backend-api/aip/connectors/mcp/tunnels` → **403 `{"detail":"Developer mode is required"}`**.
With it ON → **200 `{"tunnels":[]}`** (empty because no OpenAI Secure Tunnel is configured
on this account yet).

`FORCE CSP` sub-switch (`Forçar CSP no modo de programador`) auto-enabled with Developer
Mode and is disabled/greyed (cannot be turned off independently).

---

## Connector CRUD (S1a, cloudflared) — VERIFIED

### Create — `POST /backend-api/aip/connectors/mcp`
Request body the UI actually sent (auth = None):
```json
{
  "name": "Codex Native Spike",
  "mcp_url": "https://<rand>.trycloudflare.com/mcp/<secret>",
  "description": "…",
  "logo_url": null,
  "auth_request": { "supported_auth": [], "oauth_client_params": null }
}
```
- `auth_request.supported_auth: []` (empty array) is what the UI sends for "Sem
  autenticação". The server normalizes it to `supported_auth: [{"type":"NONE"}]` in the
  response. `oidc_enabled`, `default_scopes`, `use_cimd` were **not** sent by the UI and
  are not required.
- For the OpenAI Secure Tunnel path, send `"tunnel_id":"tunnel_<...>"` **instead of**
  `mcp_url` (not exercised — no tunnel configured; the modal normalizes one or the other).
- Response `{"connector": {...}}`. Connector id shape: **`asdk_app_<32hex>`** (NOT
  `connector_<...>` — that prefix is only for OAI first-party/service connectors). Key
  response fields: `id`, `base_url` (the full mcp_url incl. secret), `service` (origin
  only), `tunnel_id`, `supported_auth:[{"type":"NONE"}]`, `status:"ONLY_ME"`,
  `developer_type:"UNTRUSTED"`, `distribution_channel:"INDIVIDUAL"`,
  `policy_info.safety_status:"SCANNED_OK"`, and **`actions: [...]`** already populated with
  all 6 tools (create fetches the schema immediately — no separate refresh needed).
- `labels.writes: "true"` was set by the server because our tools are non-readonly.

Before create, the UI probes `GET /backend-api/aip/connectors/oauth_clients?service=<origin>`
→ `{"oauth_clients":[]}` — informational, skippable for auth=None.

### Link (no auth) — `POST /backend-api/aip/connectors/links/noauth`
Request:
```json
{ "connector_id": "asdk_app_<...>", "name": "Codex Native Spike", "action_names": [] }
```
- `link_params` and `action_param_schemas` were **not** sent (omit them; `action_names: []`
  means "all actions").
- Response: `{"id":"link_<32hex>", "connector_id", "name", "actions":[6 names],
  "auth_type":"NONE", "auth_status":"ACTIVE", "visibility":"VISIBLE", ...}`.

### Refresh actions — `POST /backend-api/aip/connectors/mcp/refresh_actions`
```json
{ "link_id": "link_<...>" }
→ 200 { "actions": [ {name, description, params (JSON schema), is_consequential, ...}, … ] }
```
(Returns the full action list. Not strictly needed right after create since actions are
already present, but this is the call to re-pull the contract if the server changes.)

### Read actions — `GET /backend-api/aip/connectors/<connector_id>/actions`
```
→ 200 { "actions": [ codex_exec, codex_write_stdin, codex_apply_patch,
                      codex_view_image, codex_tool_inventory, codex_tool_call ] }
```
Each action: `name`, `description`, `is_consequential` (true for our write tools),
`params` (`$schema` draft-07 object with `required:["turn_token", …]`),
`supported_auth:[{"type":"NONE"}]`, `is_read_only`, `is_open_world`, `is_destructive`,
`meta:{"openai/outputSchemaMissing":true}`, `visibility:"public"`.

### List (S1 `purpose` question) — RESOLVED, no `purpose` field
The app uses **two** list endpoints, both taking `{"principals":[]}` (NO `purpose` field —
the plan's worry about a 422 from a wrong `purpose` does not apply to this build):
```
POST /backend-api/aip/connectors/list_accessible?include_actions=false&external_logos=true&skip_directory=true
  { "principals": [] }
→ { "connectors": [ ... includes our asdk_app_<id> with name/base_url/tunnel_id ... ] }

POST /backend-api/aip/connectors/links/list_accessible
  { "principals": [], "link_refresh_strategy": "NONE" }
→ { "links": [ ... includes our link_<id> with connector_id, name, actions[] ... ] }
```
Use **connectors/list_accessible** to find a connector by name (and to read its
`base_url`/`tunnel_id` for reconcile), and **links/list_accessible** to find its link(s).
There is also `POST /backend-api/aip/connectors/batch {connector_ids:[...],include_actions}`
for hydrating specific ids (used by the picker; optional for us).

### Delete — VERIFIED (delete link first, then connector)
```
DELETE /backend-api/aip/connectors/links/<link_id>   → 200 {}
DELETE /backend-api/aip/connectors/<connector_id>    → 200 {}
GET   /backend-api/aip/connectors/<id>/actions       → 404 {"detail":"Connector not found"}
```
"Change URL" = delete + create (there is no PATCH of `mcp_url`). Confirmed clean.

### OAuth probe of our server (RFC 9728)
The UI did **not** call `/aip/connectors/mcp/oauth_config` for the auth=None create in this
build; discovery is driven by the MCP client (`openai-mcp`) at connect time. Our server
answered `GET /.well-known/oauth-protected-resource/mcp/<secret>` → 200
`{resource, resource_name, authorization_servers:[], scopes_supported:[]}` and every other
path as JSON 404 `{"error":"not_found"}`. **No OAuth flow was attempted** — the empty
`authorization_servers` + auth=None is accepted as "no auth". (Keep serving the PRM as JSON
regardless of status, per chat-on-steroids' note; our 404s were JSON too.)

---

## What ChatGPT's MCP client sent to our server (S1) — VERIFIED

Client UA: **`openai-mcp/1.0.0`**. Reached us via cloudflare (`cf-connecting-ip`,
`x-forwarded-for`, `x-forwarded-host`, `x-forwarded-proto` present; `host: 127.0.0.1:8790`
because cloudflared was run with `--http-host-header 127.0.0.1:8790`).

Sequence on first use:
1. `POST /mcp/<secret>` `{"method":"server/discover","id":"openai-mcp-discover",
   "params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28", …}}}`
   — our stateless SDK server answered **400** to `server/discover` (unknown method); the
   client tolerated it and fell back.
2. `POST /mcp/<secret>` `{"method":"initialize","id":1,
   "params":{"protocolVersion":"2025-11-25","clientInfo":{"name":"openai-mcp","version":"1.0.0"},
   "capabilities":{"experimental":{"openai/visibility":{"enabled":true}},
   "extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}}}}}` → 200
3. `POST` `{"method":"notifications/initialized"}` → 202
4. `POST` `{"method":"tools/list","id":1}` → 200

Headers on the MCP requests: `accept: application/json, text/event-stream`,
`content-type: application/json`, `mcp-protocol-version: 2025-11-25` (after init),
plus datadog trace headers (`traceparent`, `tracestate`, `x-datadog-*`).
- **No `Mcp-Session-Id`** was sent by the client and none was required — our **stateless**
  server (`sessionIdGenerator: undefined`, SSE responses) worked end to end. (So
  `legacy_session_mode:false` + stateless is fine; see SPIKES S1.)
- **No `x-request-id`/`wfr_*` header** in this build — instead the tool-call carries
  identity in the JSON-RPC `_meta` and in headers `x-openai-session` / `x-openai-subject`
  (opaque `v1/...` tokens) added on the tool-call requests.
- No `GET` SSE long-poll stream and no `DELETE` were observed for the stateless server
  (those are session-mode behaviors). Each request opened its own SSE response and closed.

### tools/call shape (S3)
```json
{ "method": "tools/call", "id": 0, "jsonrpc": "2.0",
  "params": {
    "name": "codex_exec",
    "arguments": { "cmd": "echo hi", "turn_token": "spike-turn-token-0123456789abcdef" },
    "_meta": {
      "openai/userAgent": "Mozilla/5.0 … Chrome/152 …",
      "openai/locale": "pt-BR",
      "openai/userLocation": { "city": "...", "region": "...", "country": "BR", "timezone": "...", "latitude": "...", "longitude": "..." },
      "openai/subject": "v1/…", "openai/session": "v1/…", "openai/organization": "v1/…",
      "io.modelcontextprotocol/clientCapabilities": { "experimental": {"openai/visibility": {"enabled": true}}, "extensions": {...} }
    }
  } }
```
`_meta` leaks approximate user geolocation to the connector — worth noting for the security
section, though our server is loopback-only under the openai tunnel.

---

## Send with a connector attached (S3) — how the selection is encoded — VERIFIED

The @mention inserts a composer pill:
```html
<span data-inline-selection-pill data-id="plugin:asdk_app_<connector_id>"
      data-symbol="ecosystemMention" data-keyword="Codex Native Spike"
      data-system-hint-type="plugin:asdk_app_<connector_id>">…</span>
```
On send, `POST /backend-api/f/conversation` body carries the connector in **two** places:
```json
{
  "action": "next",
  "messages": [ { "author": {"role":"user"}, "content": {"content_type":"text","parts":["@Codex Native Spike …"]},
    "metadata": {
      "system_hints": ["plugin:asdk_app_<connector_id>"],
      "serialization_metadata": { "custom_symbol_offsets": [
        { "id":"plugin:asdk_app_<connector_id>", "symbol":"ecosystemMention", "startIndex":0, "endIndex":19 } ] }
    } } ],
  "system_hints": ["plugin:asdk_app_<connector_id>"],   // top-level, mirrors the message hint
  "model": "gpt-5-6-instant",
  "parent_message_id": "client-created-root",
  ...
}
```
So attaching a connector without the UI = put `plugin:<connector_id>` in both
`messages[i].metadata.system_hints` and the top-level `system_hints`, with a matching
`custom_symbol_offsets` entry covering the pill text at the message start.

**Follow-up message (S4):** the 2nd message had `system_hints: []` and **no** pill, yet the
tool was still invoked — once a connector is used in a turn ChatGPT keeps it available for
the conversation. (It also injected `metadata.disable_tool_ids:["gmail","gcal","gcontacts"]`
on the follow-up, unrelated to our connector.)

The send endpoint is `POST /backend-api/f/conversation` (SSE), preceded by
`POST /backend-api/conversation/init {conversation_id:null,...}` and
`POST /backend-api/f/conversation/prepare {action:"next",...}` for a brand-new chat.

---

## Conversation tool-call trace (how a connector call appears in /backend-api/conversation)

For one `codex_exec` round, the linear turn (walking `current_node`→parents) contained:
- `role:user`, `content_type:text`, `recipient:all`.
- `role:assistant`, `channel:commentary`, `content_type:code`,
  **`recipient:"api_tool.call_tool"`**, `end_turn:false` — the tool request. Its text is
  `{"path":"/Codex Native Spike/<link_id>/codex_exec","args":{...}}`.
- `role:tool`, `name:"api_tool.call_tool"`, `recipient:"assistant"`, then another
  `role:tool` `recipient:"all"` — the tool result, with `metadata.parent_id` pointing at
  the request message id.
- `role:assistant`, `channel:"final"`, `content_type:text`, `recipient:"all"`,
  `end_turn:true` — the answer (`"simulated output of: echo hi\nCONNECTOR_OK"`).

So the driver's `api_tool_requests` detection (assistant msg with `recipient` starting
`api_tool`, matched to a later `tool` msg via `metadata.parent_id`) is correct, and the
"all api_tool answered" completion gate maps to: every `recipient:"api_tool…"` assistant
message has a following `tool` message with `parent_id == its id`.
