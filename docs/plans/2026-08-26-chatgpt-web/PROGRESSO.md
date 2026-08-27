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

## M2 — driver: daemon, page scripts, API, pool de abas ✅

`core/src/chatgpt_web/driver/`:

- `page_scripts.rs`: 14 scripts verbatim (`wait_ready`, `composer_state`, `set_composer_text`, `attachment_tiles`, `dismiss_upload_dialog`, `clear_composer`, `click_send`, `click_stop`, `api_call`, `api_call_with_headers` (novo, para o registo do conector), `stage_download`, `read_download_chunk`, `dom_turns`, `menu_discover`, `menu_select`); interpolação só via `serde_json::to_string` num filler `@@NAME@@` de passagem única; testes: nenhum `async`, escaping.
- `api.rs`: `RawConversation`/`RawMessage` tolerantes (`#[serde(default)]`), `normalize()` = porte de `api.ts:182–280` + `api_tool_requests` (regra do chat-on-steroids `fiber.js`), `fingerprint`, `trait PageEval`, `ChatGptApi` (get/read/patch/list/models com cache 5 min e backoff 429 `[2,5,10]s`; status→`DriverErrorKind`). 6 fixtures reais em `fixtures/`.
- `daemon.rs`: `DaemonClient` sobre `RmcpClient` Streamable HTTP (connect lazy com semáforo, `initialize` 30 s, `call` com timeout `max(120s, t+30s)`, `isError`→`Tool`, imagens base64, 1 reconexão pela regex de `daemon.ts:107–109`, `eval_in` com dupla decodificação, `health()` GET `/healthz` 3 s, `shutdown` → `DELETE` da sessão). Confirmado: o adaptador rmcp **não** manda `Origin`; bearer sai em `Authorization`. Live: `live_daemon_health` e `live_daemon_lists_tabs_over_mcp` verdes.
- `tabs.rs`: registo `~/.chatgpt-pro-mcp/tabs.json` com os bytes exatos do Node (interop com o `chatgpt-pro-mcp` concorrente), lock `mkdir tabs.json.lock` (steal >10 s, deadline 5 s), `pid_alive` (Windows `OpenProcess`+`GetExitCodeProcess`; unix `kill(0)`), `TabPool` com afinidade conversa↔aba, `TabLock` FIFO, sweeper idle, adoção de órfãs, `with_activated_on` (semáforo de foco → ativar → f → reload → restaurar), `shutdown`/`Drop` limpam o registo.
- `core/Cargo.toml`: `windows-sys 0.52` (`Win32_Foundation`, `Win32_System_Threading`) só em `cfg(windows)`.

Gate: 77 testes unitários (`chatgpt_web::driver::`) + 2 live verdes; clippy limpo nos ficheiros novos; `just fmt`.

