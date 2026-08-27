# M0/C0 spike results — chatgpt_web connector (2026-08-27)

Live run against the user's real Chrome + chatgpt.com (account `pro`,
`joaovitorbor@gmail.com`) via the chrome-mcp daemon (127.0.0.1:8848), using the
**cloudflared** quick-tunnel fallback. Endpoint payloads are in
[`api_shapes.md`](./api_shapes.md). Throwaway MCP server: `scratchpad/spikes/server.mjs`
(`@modelcontextprotocol/sdk` 1.30.0, express, stateless Streamable HTTP, SSE, the 6-tool
contract; `codex_exec` sleeps `N`s for a `cmd` of `sleep N`; `codex_apply_patch` writes the
patch to `scratchpad/spikes/patches/`).

## ⚠️ Pending the user (blocks the `tunnel = "openai"` default)
The OpenAI Secure MCP Tunnel path (`tunnel-client`, `tunnel_id` in the connector body)
could **not** be exercised: it needs a Tunnel + a restricted API key
(`Tunnels: Read + Use`) created on `platform.openai.com` **logged in as this same web
account**. `GET /backend-api/aip/connectors/mcp/tunnels` returns `{"tunnels":[]}` (200,
Developer Mode on) — the account has no tunnel yet. **All spikes below used cloudflared.**
Everything about the connector CRUD / MCP transport / mention / approval is transport-
agnostic and applies to the openai path too; only "does `tunnel_id` in the create body
resolve" and the `tunnel-client` flags remain to verify once the user does the one-time
platform setup. Recommend `codex chatgpt-web setup` implement that, and until then the
implementation should default-test with cloudflared.

---

## S0 — Developer Mode toggle — ✅ VERIFIED (automatable)
- The UI switch fires `PATCH /backend-api/settings/account_user_setting?feature=developer_mode&value=true`
  (no body; `value` in the query as a string) → `200 {"developer_mode":true}`. Fully
  scriptable via a page `fetch` with the bearer + `ChatGPT-Account-ID`.
- Gate proven: `mcp/tunnels` is 403 `"Developer mode is required"` when off, 200 when on.
- `developer_mode` shows up in `GET /backend-api/settings/user`'s `settings` map once set.
- **Decision:** automate the toggle (plan's `connector_auto_developer_mode=true` is
  achievable with no UI clicks). Keep the Settings→Segurança e login→"Modo desenvolvedor"
  UI path only as a documented manual fallback.

## S1 — connector CRUD + MCP transport — ✅ VERIFIED (cloudflared); openai tunnel pending
- Create/link/refresh/read-actions/list/delete all succeed from a page `fetch` with just
  `Authorization` + `OAI-Product-Sku: CONNECTOR_SETTING`. Both **UI-created** and
  **API-created** connectors produced identical results; the API-created one was created,
  linked, refreshed, verified (6 actions) and deleted cleanly. See `api_shapes.md`.
- **`purpose` is a non-issue in this build:** `list_accessible` takes `{"principals":[]}`
  with **no `purpose` field** (no 422 risk). There are two list endpoints — connectors and
  links — both used for reconcile.
- Connector id shape is **`asdk_app_<32hex>`**, link id **`link_<32hex>`**. "Change URL" is
  delete+create (no URL PATCH).
- **MCP transport decision — stateless + SSE works:** ChatGPT's client (`openai-mcp/1.0.0`)
  did `initialize`(2025-11-25) → `notifications/initialized` → `tools/list` → `tools/call`,
  **no `Mcp-Session-Id`**, no session GET/DELETE. Our server ran with
  `sessionIdGenerator: undefined` (stateless) + SSE responses and answered every call.
  → **`legacy_session_mode: false`, `json_response: false` (SSE) is the correct config.**
  One quirk: the client first probes `server/discover` (a pre-init JSON-RPC method); our
  SDK returned 400 and the client fell back to `initialize` fine — but our own Rust server
  should answer `server/discover` gracefully (return an error object, not drop the
  connection). Keep the PRM (`/.well-known/oauth-protected-resource/mcp/<secret>`) as
  **200 JSON** with `authorization_servers:[]` and every other path as **JSON 404**.
- **Auth decision:** **NONE + secret path** is accepted end-to-end (auth=None create;
  `supported_auth:[]` in the body → server stores `[{"type":"NONE"}]`; no OAuth flow
  attempted). API_KEY hardening (fase 2) not needed for correctness. `S1b` (which header the
  `api_key` link uses) was **not** run — deferred with fase 2.
- Host-header note: cloudflared must run `--http-host-header 127.0.0.1:<port>` so the
  rmcp/SDK host allow-list passes (we did; requests arrived with `host: 127.0.0.1:8790`).

## S2 — @mention in a background (unfocused, dedicated) tab — ✅ mostly; one caveat
- On the chat root, typing `@Codex` via `execCommand('insertText')` in a **hidden**
  dedicated tab mounted the mention popover in **~530 ms** and produced the pill on `Enter`
  **without activating the tab**. Pill DOM:
  `span[data-id="plugin:asdk_app_<id>"][data-symbol="ecosystemMention"][data-keyword="<name>"]`.
  Row selector confirmed: `.__menu-item[tabindex="0"]` with the connector title + its
  description line, gaining `data-highlighted` on `ArrowDown`.
- Send worked headless (`[data-testid="send-button"]`), the turn ran, the tool was called,
  the answer came back — **all with the tab hidden.**
- **Caveat / flake:** on a *fresh* chat opened by `?model=…` navigation, re-mentioning
  sometimes surfaced only recent-prompt rows (`.__menu-item` also matches sidebar/recents),
  and my synthetic click didn't always commit the pill. The reliable pattern is: clear the
  composer, `insertText('@Codex')`, wait for a `.__menu-item[tabindex="0"]` whose text
  contains **both** the connector name and its description (to exclude recents), `ArrowDown`
  until `data-highlighted`, `Enter`, verify `[data-id^="plugin:"]`; **fallback** to
  activate→mention→send→restore on failure.
- **Decision:** `connector_mention_strategy = "auto"` (try background first; the pill
  mounts unfocused). Keep the activate-fallback for the fresh-chat flake. Distinguish the
  real popover row from recents by requiring the description text, not just the name.

## S3 — approval card, clicked in background — ✅ VERIFIED
- Card appeared essentially instantly: `div[data-testid="tool-approval-card"]`, text
  "Permitir que o ChatGPT use Codex Native Spike?" listing "Os dados compartilhados incluem:
  segredo de autenticação, turn_token". Buttons (PT): **`Sempre permitir`**, `Negar`,
  **`Permitir uma vez`**, `Ver detalhes`, and an aria `"Allow … for this conversation"`.
- Clicking `Sempre permitir` via a synthetic pointerdown/up+click (with coordinates) in the
  **hidden** tab worked; the tool call fired and completed.
- **Decision:** background approval is feasible → `connector_auto_approve_ui = true` clicks
  `Sempre permitir|Allow always` (regex must include PT). 60s→deny/fail path still needed.

## S4 — selection persists on the 2nd message without re-mention — ✅ VERIFIED
- After one use, a follow-up with **no pill and `system_hints: []`** still invoked the
  connector. So per-conversation the connector stays attached; the driver need not
  re-mention every turn (but re-mentioning is harmless and is the safe reconnect path).
- Approval also persisted: the 2nd call raised **no** new card (`Sempre permitir` is
  per-conversation sticky).

## S5 — write tools per mode — ✅ Instant fully; ⚠️ Thinking/Extended/Pro not re-run
- **Instant (`gpt-5-6-instant`):** `codex_exec` ✅ and `codex_apply_patch` ✅ both invoked;
  apply_patch wrote the real patch text to disk (`patches/patch-*.txt` =
  `"BEGIN_PATCH hello from codex spike END_PATCH"`). **No `api_tool unavailable`.** Write
  tools work on Instant.
- **Thinking / Extended / Pro:** not individually confirmed this run — the mention popover
  was flaky to remount on the fresh Thinking chat (see S2 caveat) and I stopped rather than
  burn the budget re-driving it. The connector, contract and backend path are identical
  across modes, and both reference projects (codex-chatgpt-web, chat-on-steroids) drive
  Thinking/Pro, so the expectation is they work; **the "Pro is read-only for custom MCP"
  claim remains unverified** (plan already marks it so). **Action:** verify per-mode in the
  connector smoke test (M-connector / C5) before setting `supported_modes` in the catalog;
  until then treat all five `chatgpt-web/*` lines as connector-capable and revisit if Pro
  refuses writes.

## S6 — call duration & parallelism — ✅ VERIFIED (important constraints)
- **Duration:** a `codex_exec` that slept **60 s** completed cleanly — ChatGPT held the
  connection the full 60 s (server logged `60018ms`, the answer returned `"slept 60s"`).
  The single SSE response per call plus cloudflare survived 60 s with no keep-alive bytes
  from us (our server sent nothing until the result). Longer bounds (180/300 s) were **not**
  pushed this run; the plan's SSE keep-alive (15 s) is still the right hedge against the
  ~100 s Cloudflare idle cutoff for the openai path.
- **Parallelism: calls SERIALIZE.** Asked explicitly for 3 concurrent `sleep 15` calls in
  one response, the server saw them **strictly sequential**: 07→22, 25→40, 44→59 (each 15 s,
  ~3 s gap between). ChatGPT does **not** run connector tool calls concurrently within a
  response. → total response time ≈ Σ per-call time. This makes the per-call deadline the
  thing that matters, and argues for the plan's yield≤30 s + `codex_write_stdin` polling for
  anything long, since three 30 s calls already cost ~100 s wall-clock and approach the CF
  cutoff.
- **Decision:** `connector_call_timeout_ms = 120000` is safe for a single call (60 s proven,
  headroom above). Keep the exec default yield ≤ 30 s and lean on `write_stdin` polling.
  Because calls serialize, the daemon's per-call deadline + an explicit "did not finish,
  poll with codex_write_stdin" error (already in the plan) is the right behavior.

---

## Config knobs settled by these spikes
| knob | value | source |
|---|---|---|
| `legacy_session_mode` | `false` | S1 (stateless worked, no Mcp-Session-Id) |
| `json_response` | `false` (SSE) | S1 |
| connector auth | `NONE` + secret path (API_KEY = optional fase 2) | S1 |
| `connector_auto_developer_mode` | `true` (PATCH is scriptable) | S0 |
| `connector_mention_strategy` | `"auto"` (background works; activate-fallback for fresh-chat flake) | S2 |
| `connector_auto_approve_ui` | `true`, click `Sempre permitir\|Allow always` (PT+EN regex) | S3 |
| re-mention每turn | not required (selection sticky per conversation) | S4 |
| `connector_call_timeout_ms` | `120000` (60 s proven) | S6 |
| exec yield cap | ≤ 30 s + `codex_write_stdin` polling (calls serialize) | S6 |
| `supported_modes` | Instant confirmed write-capable; Thinking/Extended/Pro TBD in smoke | S5 |

## Surprises worth carrying into the implementation
1. **No `purpose` field** on `list_accessible` in this build — simpler than the plan feared.
2. **Two** list endpoints (`connectors/list_accessible` and `links/list_accessible`), both
   `{"principals":[]}`; reconcile must query both.
3. Connector ids are **`asdk_app_<hex>`**, not `connector_<hex>`.
4. Create already returns the full `actions[]` — no mandatory `refresh_actions` right after.
5. The client sends a pre-init **`server/discover`** JSON-RPC call; the Rust server must
   answer it gracefully (an error object is fine) instead of dropping the connection.
6. tools/call `_meta` includes **approx user geolocation** and opaque
   `openai/session`/`openai/subject` tokens — note in the security section (mitigated to
   loopback under the openai tunnel).
7. Send encodes the connector via `system_hints:["plugin:<connector_id>"]` at both message
   and top level + `custom_symbol_offsets` — enables a future "attach without UI" path.

## Cleanup done
Both spike connectors + links deleted (`list_accessible` → no `Codex Native Spike*`
remain), the spike conversation set `is_visible:false`, the dedicated tab closed, and the
cloudflared quick-tunnel + spike node server killed. (A separate pre-existing
`cloudflared.exe` Windows **service**, session 0, was left running — it is not ours.)
Developer Mode left **ON** (the feature requires it). `scratchpad/spikes/server.mjs` kept
for reuse in the Rust integration tests.
