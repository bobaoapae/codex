# Progresso da execução — provider `chatgpt_web`

> Diário de execução do [PLANO.md](./PLANO.md). Uma seção por milestone, com o que foi feito,
> onde, e o gate que passou. Capturas dos spikes em [SPIKES.md](./SPIKES.md) e
> [api_shapes.md](./api_shapes.md).

## Base

- Commit `ac249a819` — snapshot do working tree (paridade Claude fases 0–10 + planos) antes de
  qualquer código `chatgpt_web`.
- Esqueleto: `core/src/chatgpt_web/{mod.rs,driver/mod.rs}` com `DriverError`,
  `DriverErrorKind`, `FailurePhase`, `DriverResult` partilhados pelo driver.
- Referências: fontes TS em `C:\Users\Joao\WebstormProjects\chatgpt-pro-mcp\src`; clones de
  `codex-chatgpt-web` e `chat-on-steroids` no scratchpad da sessão.

## Ordem de execução

1. M0/C0 spikes (live, cloudflared) ‖ M1 ‖ M2 (driver: page_scripts+api, daemon+tabs)
2. M3 ops → M4 núcleo (`tools = "none"`) → M5 imagens/arquivo/compaction
3. M6 seam + C1–C4 daemon/conector → C5 + M7 polimento

## M0/C0 — spikes S0–S6 ✅ (live, cloudflared; conta `pro`)

Detalhe em [SPIKES.md](./SPIKES.md) e payloads em [api_shapes.md](./api_shapes.md).

- S0: Developer Mode é automatizável por `PATCH /backend-api/settings/account_user_setting?feature=developer_mode&value=true` (gate provado: `mcp/tunnels` 403→200). Ficou ligado.
- S1: CRUD do conector inteiro por `fetch` da página com `Authorization` + `OAI-Product-Sku: CONNECTOR_SETTING`. **Sem campo `purpose`** em `list_accessible`; há **dois** endpoints de listagem (`connectors/list_accessible` e `links/list_accessible`, ambos `{"principals":[]}`); ids são `asdk_app_<hex>`; o create já devolve as 6 `actions`. Cliente MCP `openai-mcp/1.0.0`: `initialize` → `notifications/initialized` → `tools/list` → `tools/call`, **stateless (sem `Mcp-Session-Id`)** → `legacy_session_mode:false` + SSE; manda um `server/discover` pré-init que o servidor deve responder com erro JSON-RPC em vez de derrubar a ligação. Auth NONE + path secreto aceite ponta a ponta.
- S2: @mention monta a pill (`[data-id^="plugin:"]`) em aba **oculta** em ~530 ms; re-mention em chat novo é instável (recents poluem `.__menu-item`) → `connector_mention_strategy = "auto"` com fallback de ativação.
- S3: card de aprovação clicável em background (`[data-testid="tool-approval-card"]`, `Sempre permitir`). S4: seleção e aprovação persistem na 2.ª mensagem sem re-mention.
- S5: Instant executa `codex_exec` e `codex_apply_patch` (patch real gravado em disco). Thinking/Extended/Pro **não re-verificados** → confirmar no smoke C5 antes de fixar `supported_modes`.
- S6: chamada de 60 s mantida; **3 chamadas "paralelas" serializam** → deadline por chamada é o que importa; yield ≤ 30 s + `codex_write_stdin`. `connector_call_timeout_ms = 120000` seguro.
- Envio com conector codifica `system_hints:["plugin:<connector_id>"]` + `custom_symbol_offsets` (caminho futuro "anexar sem UI"). `tools/call._meta` traz geolocalização aproximada e tokens `openai/session|subject` (nota de segurança).
- **Pendente do usuário:** túnel `openai` (Tunnel + key restrita em platform.openai.com, mesma conta web) — `GET /aip/connectors/mcp/tunnels` devolve `{"tunnels":[]}`. Tudo o resto é agnóstico ao transporte.
- Servidor MCP Node dos spikes mantido em `<scratchpad>/spikes/server.mjs` para reuso nos testes de integração.

## M1 — catálogo, provider, config e todos os `match` ✅

- `model-provider-info/src/lib.rs`: `CHATGPT_WEB_PROVIDER_ID = "chatgpt_web"`, `WireApi::ChatGptWeb` (`#[serde(rename = "chatgpt_web")]`), arms em `Display`/`Deserialize`, `create_chatgpt_web_provider()` (sem HTTP, `requires_openai_auth: false`), entrada em `built_in_model_providers()`; 3 testes novos.
- `models-manager/chatgpt_web_models.json` (5 linhas `chatgpt-web/{instant,thinking,high,extra-high,pro}`, `visibility: hide`, `apply_patch_tool_type: "freeform"`, `priority` 40–44, um `supported_reasoning_levels` por linha, ctx/auto_compact 41000/32000, 90000/80000 ×3, 111193/95000) + `local_models.rs` com os dois bundles num só `OnceLock` e `provider_for_locally_served_model(slug)`; 4 testes.
- `config/src/config_toml.rs`: `ChatGptWebToml` (33 chaves opcionais), `ChatGptWebTools{None,Connector}`, `ChatGptWebTunnel{Openai,Cloudflared,Manual}`, `ChatGptWebMentionStrategy{Auto,BackgroundOnly,Activate}`; `core/config.schema.json` regenerado.
- `core/src/config/mod.rs`: `pub struct ChatGptWebSettings` (defaults do plano; `idle_timeout_ms 0 → None`, `max_tabs` clamp 1..=8) + `from_toml`; `Config.chatgpt_web`; re-export em `core-api`; `thread-manager-sample` compila.
- `core/src/chatgpt_web/mod.rs`: `ChatGptWebWorkspace::from_config`, `ChatGptWebThreadState`, `stream()` stub (`UnsupportedOperation`).
- Sites espelhados: `client.rs` (estado + workspaces + arm de dispatch), `session.rs`, `turn.rs`, `compact.rs` (só o workspace — sem skip de auto-compaction), `thread_config/remote.rs`, `multi_agents_common.rs` (`task_fork_mode_for_wire_api` para `ClaudeCode|ChatGptWeb`, `max_fork_turns_for_wire_api`, `align_provider…` via `provider_for_locally_served_model`, service tier), `multi_agents_v2.rs`, `agent/role.rs` (filtro + nota do ChatGPT Web). Sem código específico de wire API em `exec`/`tui`/`turn_timing`; `otel`/`features` são só Claude.

Gate: check verde em core/tui/exec/app-server/cli/otel/config/core-api/sample; model-provider-info 29, models-manager 54, config 284; `cargo test -p codex-core --lib` 2387 passed / 3 failed (as 3 pré-existentes do débito conhecido).

## M2 — driver: daemon, page scripts, API, pool de abas ✅

`core/src/chatgpt_web/driver/`:

- `page_scripts.rs`: 14 scripts verbatim (`wait_ready`, `composer_state`, `set_composer_text`, `attachment_tiles`, `dismiss_upload_dialog`, `clear_composer`, `click_send`, `click_stop`, `api_call`, `api_call_with_headers` (novo, para o registo do conector), `stage_download`, `read_download_chunk`, `dom_turns`, `menu_discover`, `menu_select`); interpolação só via `serde_json::to_string` num filler `@@NAME@@` de passagem única; testes: nenhum `async`, escaping.
- `api.rs`: `RawConversation`/`RawMessage` tolerantes (`#[serde(default)]`), `normalize()` = porte de `api.ts:182–280` + `api_tool_requests` (regra do chat-on-steroids `fiber.js`), `fingerprint`, `trait PageEval`, `ChatGptApi` (get/read/patch/list/models com cache 5 min e backoff 429 `[2,5,10]s`; status→`DriverErrorKind`). 6 fixtures reais em `fixtures/`.
- `daemon.rs`: `DaemonClient` sobre `RmcpClient` Streamable HTTP (connect lazy com semáforo, `initialize` 30 s, `call` com timeout `max(120s, t+30s)`, `isError`→`Tool`, imagens base64, 1 reconexão pela regex de `daemon.ts:107–109`, `eval_in` com dupla decodificação, `health()` GET `/healthz` 3 s, `shutdown` → `DELETE` da sessão). Confirmado: o adaptador rmcp **não** manda `Origin`; bearer sai em `Authorization`. Live: `live_daemon_health` e `live_daemon_lists_tabs_over_mcp` verdes.
- `tabs.rs`: registo `~/.chatgpt-pro-mcp/tabs.json` com os bytes exatos do Node (interop com o `chatgpt-pro-mcp` concorrente), lock `mkdir tabs.json.lock` (steal >10 s, deadline 5 s), `pid_alive` (Windows `OpenProcess`+`GetExitCodeProcess`; unix `kill(0)`), `TabPool` com afinidade conversa↔aba, `TabLock` FIFO, sweeper idle, adoção de órfãs, `with_activated_on` (semáforo de foco → ativar → f → reload → restaurar), `shutdown`/`Drop` limpam o registo.
- `core/Cargo.toml`: `windows-sys 0.52` (`Win32_Foundation`, `Win32_System_Threading`) só em `cfg(windows)`.

Gate: 77 testes unitários (`chatgpt_web::driver::`) + 2 live verdes; clippy limpo nos ficheiros novos; `just fmt`.


## C1 — esqueleto do daemon do conector (+ TurnBroker) ✅

`core/src/chatgpt_web/connector/` (facade pública `codex_core::chatgpt_web_daemon` em `core/src/lib.rs`):

- `contract.rs`: as 6 tools fixas (`codex_exec`, `codex_write_stdin`, `codex_apply_patch`, `codex_view_image`, `codex_tool_inventory`, `codex_tool_call`) com schemas JSON, `turn_token` obrigatório (20..256, `[A-Za-z0-9_-]`), descrições ≤120 chars, `CONTRACT_VERSION = 1`; `parse()` valida nome+token; `to_call()` resolve contra as `ToolSummary` do turno (`codex_exec` → `exec_command` ou `shell`; `codex_apply_patch` → `CustomToolCall` só se anunciado; `codex_tool_call` com namespace; `codex_tool_inventory` servido localmente); `yield_time_ms` clampado a 250..30000.
- `daemon/broker.rs`: `TurnBroker` (`std::sync::Mutex`, sem awaits sob lock): sessões com heartbeat (30 s), turnos com TTL, `claim` cunha binding idempotente, `invoke` enfileira + batch 15 ms + oneshot com `timeout(min(call_timeout, expires_at))` e mensagem "did not finish… use yield_time_ms ≤ 30000 and poll with codex_write_stdin", `next_batches` long-poll at-least-once com ack por `seq`, `complete` exige `in_flight` + sessão dona, `revoke`/`sweep` retiram tokens (LRU 256) com a mensagem "already finished (<trace>)".
- `daemon/public_server.rs`: axum 0.8 em `127.0.0.1:{tunnel_port}`, path secreto `/mcp/<base64url(32B)>` por start com comparação em tempo constante, 404 JSON fora do path, `GET /mcp/<s>/healthz`, PRM RFC 9728 em JSON (`authorization_servers: []`, `cache-control: no-store`); rmcp `StreamableHttpServerConfig` **stateless** (`legacy_session_mode: false`, SSE, keep-alive 15 s, body 8 MiB, `allowed_hosts` loopback); `PublicMcpHandler` com rate limit (30 chamadas/10 s, 10 claims falhados/min) — claims recusados voltam como `is_error` (o modelo lê o texto), args inválidos como `McpError`.
- `daemon/tunnel.rs`: `TunnelAdapter` + `watch<TunnelState>` (`Connecting|Ready{OpenAi{tunnel_id}|Public{mcp_url}}|Down|Fatal`); adapters `NoopTunnel`, `FatalTunnel`, `CloudflaredTunnel` (URL `*.trycloudflare.com` em 45 s, healthz público ≤60 s, backoff `min(60s, 2s·2^n)`), `OpenAiTunnel` (`tunnel-client run --control-plane.tunnel-id … --health.listen-addr 127.0.0.1:0 --health.url-file … --log.format json`, credenciais só em env `CONTROL_PLANE_API_KEY`/`MCP_SERVER_URL`, `/readyz` até 120 s, regex de auth → `Fatal` sem retry, regex de rede → `Down`); árvore de processos por `JobObject` (Windows) / process group (unix) + `taskkill /T /F` de fallback; download da release pinada `v0.0.12` com SHA-256 (tabela de 6 hashes do chat-on-steroids) e extração `zip`; resolução `tunnel_client_path` → `CODEX_HOME/chatgpt_web/bin/` → PATH → homebrew. **Flags do tunnel-client não confirmadas ao vivo** (binário ausente, sem tunnel na conta) — tabela única de constantes para ajuste.
- `daemon/state.rs`: `CODEX_HOME/chatgpt_web/{daemon.lock,daemon.json,daemon.token,connector.json,daemon.log,tunnel.key,bin/}`; lock exclusivo Windows (`share_mode(0)`) / `flock`; escrita atómica; `RegistryStatus` (C2 preenche; aqui `not_implemented`).
- `daemon/control.rs` + `wire.rs`: control API loopback com bearer (`daemon.token`): `GET /healthz`, `POST/DELETE /v1/sessions`, heartbeat, `POST /v1/turns` (409 duplicado), `DELETE /v1/turns/{token}`, `GET /v1/sessions/{sid}/calls?after&wait_ms` (≤30 s), `POST /v1/calls/{id}/result`, `POST /v1/registry/reconcile` (501 até C2; hook `ReconcileHook`), `POST /v1/admin/shutdown_when_idle`.
- `daemon/mod.rs`: `start/run/status/stop/ensure_daemon/running_endpoint/spawn_detached` (Windows `DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP|CREATE_NO_WINDOW`), idle shutdown, writer de `daemon.json`, versão diferente → `shutdown_when_idle` + respawn; helpers para o CLI (`setup_tunnel`, `probe_chrome_mcp`, `reconcile_via_daemon`, `wait_tunnel_ready`).
- CLI `cli/src/chatgpt_web_cmd.rs` (`codex chatgpt-web daemon [--foreground] [--idle-shutdown-ms] | status | stop | doctor | setup --tunnel-id --api-key-file | registry reconcile|show|delete`); log em `daemon.log` com rotação a 5 MB.
- `core/Cargo.toml`: `axum` (http1,json,query,tokio), rmcp `transport-streamable-http-server`, `sha2`, `zip`.

Gate: 46 testes unitários (`chatgpt_web::connector`) + 4 de integração (`core/tests/suite/chatgpt_web_connector.rs`: contrato via `initialize`+`tools/list`, `tools/call` sem `initialize` e `server/discover` respondidos, round-trip `codex_exec`/`codex_apply_patch`/inventory por long-poll, token retirado, sessão desconectada falha a chamada, resultado de outra sessão → 403); clippy limpo nos ficheiros novos; smoke do CLI com `tunnel = "manual"` em `CODEX_HOME` temporário: `status` → `doctor` → autostart destacado via `registry reconcile` (501 esperado) → `status` vivo → `stop`; `daemon --foreground` + segunda instância recusada pelo lock + `stop` termina o processo.

Pendente para C2/C3/C4: registo do conector (chrome-mcp a partir do daemon, `ReconcileHook`), cliente de sessão (`connector/client.rs`) sobre `wire.rs`, anexação no browser; validação ao vivo das flags do `tunnel-client` quando o usuário criar o Tunnel + key.
