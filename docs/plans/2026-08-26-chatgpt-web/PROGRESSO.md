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


## M3 — ops: envio, seleção de modelo, stop, uploads ✅

`core/src/chatgpt_web/driver/ops.rs` (porte de `ops.ts`, ~2.1k linhas) + `ops_tests.rs`:

- `ModelSpec{Auto,Instant,Thinking,Medium,High,ExtraHigh,Pro,Slug}`, `resolve_model_with` (puro; base = `default_model_slug` sem sufixo `-(instant|thinking|pro|mini|t-mini)`), `LEVEL_LABEL_*` (PT/EN), `set_level_via_menu` (ativar → menu → reload → restaurar).
- `send(SendRequest{conversation_id, text, model, files}) -> Sent{conversation_id, phase_reached, model_label, notes}` com a máquina de fases navigate→model→precheck→upload→compose→attachments-wait→submit→confirm; todo erro sai com `FailurePhase` + `message_landed` (`Some(false)` antes do submit, `Some(true)` no confirm, `None` só em submit ambíguo ⇒ `SubmitAmbiguous`); `confirm_submitted` (15 s) em continuação; precheck "generating" = `DriverErrorKind::Busy` (novo, transitório) com graça de 5 s + reload.
- Uploads: imagens em `input[data-testid="upload-photos-input"]`, restantes pelos 3 seletores do TS; dedupe por nome+tamanho; popup "já carregou este arquivo" auto-dispensado (o ChatGPT dedupe uploads por conteúdo, à escala da conta).
- `wait_reply` (âncora no último user turn; `done = reply && idle && (end_turn==true || fingerprint estável)`), `stop` (click_stop até 8 s), `download_assets`, `check_reply` puro; `classify_page_error` (diálogos PT/EN → RateLimited/MessageTooLong/LoginRequired/Upstream — **não exercitado ao vivo**).
- Seam corrigido: `PageEval::eval`/`ChatGptApi` passam a usar `TabId = i64`.
- Divergências: `send` não espera a resposta (separado em `wait_reply`); continuação sem botão Stop após 12 s → confirmação pela API em vez de poll cego.

Gate: 119 testes do driver verdes; live 5/5 (`--test-threads=1`, 64 s): `pro_resolves` (`gpt-5-6-pro`), `new_chat_instant_reply` ("PONG", label "Instantâneo"), `continue_conversation`, `stop` (thinking, parado aos 3 s), `upload_and_reply` ("Red"). Conversas escondidas (`is_visible:false` — repetir dá 404 `conversation_deleted`), registo de abas limpo.

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
## M4 — núcleo: history, sessions, prompt, poll→eventos, `stream()` em `tools = "none"` ✅

Reuso do `claude_code` (`// FORK:` em cada ponto): `StreamAssembler`/`message_item`/`reasoning_item`
movidos para `claude_code/assembler.rs` (`pub(crate)`, + `open_reasoning`/`open_message`/
`reasoning_open`/`message_open`); `claude_code::history::{render_item, render_item_with,
fingerprint, item_fingerprint, truncate_*}` e `claude_code::state_file` passaram a `pub(crate)`.
`render_item_with` recebe o renderizador de imagens (o Claude continua a ver `[image omitted]`).
Gate: `cargo test -p codex-core --lib claude_code::` → 68 passed.

`core/src/chatgpt_web/`:

- `history.rs`: `ConversationContinuity{conversation_id, model_slug, delivered_items,
  delivered_fingerprint, echoed, message_landed_unanswered}`; `plan_request(input, &continuity,
  model_slug, compact_prompt)` = cópia de `claude_code/history.rs:64–96` com `can_extend` a exigir o
  mesmo `model_slug`; `is_compaction` quando o último item é a mensagem user igual ao prompt de
  sumarização (→ `restart`, conversa descartável). 7 testes.
- `sessions.rs`: `CODEX_HOME/chatgpt_web_sessions.json` (TTL 7 d, 512 entradas, `state_file`),
  `load`/`store`/`forget`. `ChatGptWebThreadState{continuity, store}` com `hydrate`/`record`/
  `invalidate`/`mark_unanswered`.
- `prompt.rs`: `render(RenderRequest{plan, workspace, mode: None|Connector(linhas)|Compaction,
  is_pro, resume_after_interrupt, images})` → `RenderedTurn{text, attachments, is_replay}`.
  Replay = header + contrato do modo + `[pro]` + nota de imagens + fecho + `Environment:` +
  `<developer_instructions>` + `<codex_transcript>` + `<codex_transport_resume>`; extensão = itens
  novos (`(no new input; …)` / prefixo `(the previous request was interrupted; …)`).
  `warning_text(level)` = aviso commentary do modo none. `transcript_chars` para o medidor.
- `attachments.rs` (M5): `ImageStore` em `CODEX_HOME/chatgpt_web/attachments/`, nome
  `codex-img-<hash[..12]>.<ext>` (**sha1**, já dependência do core — o plano dizia sha256; só afeta
  o nome), cap 10 por mensagem (as mais recentes; as antigas ficam `(not attached; …)` na
  transcrição), limpeza >24 h no início do turno.
- `stream.rs`: `ReplyTracker::observe(conv, mode) -> Vec<Delta>` (puro; âncora = primeiros 120
  chars do texto enviado, como o `waitReply` TS; `OpenReasoning/Reasoning/OpenText/Text/Rewrite/
  Note/Progress/PartialCompletion/Done{EndTurn|Stable}`; idle = `!is_generating && async_status ∈
  {None,0}`; `Done{EndTurn}` = idle && texto mais recente `end_turn:true` + `finished_successfully`;
  `Done{Stable}` = idle && assets && fingerprint estável 2 polls; **fallback** `Stable` sem assets
  após 8 polls estáveis (~20 s) para não ficar 20 min preso quando o `end_turn` nunca chega —
  divergência do plano, fail-closed na janela em que o botão Stop pisca entre fases; `Connector`
  exige `api_tool_requests.all(has_result)`; `finished_partial_completion` idle → erro upstream).
  `PollLoop::run` (`select!{consumer_dropped, watchdog last_progress+idle_timeout, sleep(poll)}`,
  404 tolerado 30 s após o envio, 8 leituras falhadas consecutivas → `Failed`, `connector_rx`
  stub para M6) alimenta o `StreamAssembler`. 15 testes sobre as fixtures reais.
- `mod.rs`: `stream()` → `tokio::spawn(run_turn)`; `run_turn`: `Created` → `TURN_SLOTS`
  (semáforo estático, `max_parallel_turns`) → `plan_request` → `prompt::render` → item commentary
  de aviso (modo none) → driver partilhado por processo (`OnceLock` por `daemon_url|base_url`:
  `DaemonClient` + `TabPool` + `ChatGptOps`; cliente HTTP plain `ReqwestDefault` porque o daemon
  é loopback) → `health()` (exige `extension_connected`) → `ops.send` (modelo só no restart; 429 →
  pausa 30 s e 1 retry) → grava continuidade com `message_landed_unanswered=true` **antes** do
  poll → `PollLoop` → `Completed{response_id: conv_id, usage, end_turn: Some(true)}` + grava
  `echoed = assembler.take_authored()`. Uso = `ceil(chars(render(histórico inteiro))/4) + 8192`
  entrada, `ceil(chars(reply)/4)` saída. Mapeamento de erros = tabela do plano (`Busy`, novo no
  driver, → `Stream`). Interrupt/stall → `ops.stop` (10 s) + `mark_unanswered`. `tools =
  "connector"` → `UnsupportedOperation` acionável até M6/C3.

Gate: `cargo test -p codex-core --lib -- chatgpt_web:: claude_code::` → 302 passed, 0 failed
(8 ignored = live); clippy limpo nos ficheiros novos/tocados; `just fmt` (revertido o churn de
EOL nos `BUILD.bazel`).

E2E com o binário (`target/debug/codex.exe`, conta web `pro`, aba dedicada 626460273):

| corrida | resultado |
|---|---|
| `codex exec -c model_provider=chatgpt_web -m chatgpt-web/instant "Reply with the single word PONG."` | **PONG** em 20 s (26 389 chars enviados, conversa nova, `EndTurn`) |
| 2 turnos (`exec` + `exec resume --last`, `archive_on_shutdown=false`) | turno 2 = **extensão** de 57 chars na mesma conversa; respondeu o codeword `ZEBRA-42` em 6 s |
| `exec -i red.png "What color…"` | **Red**; ficheiro `codex-img-8218b38a220c.png` materializado e anexado (popup "já carregou este arquivo" auto-dispensado) |
| `spawn_agent` do role `~/.codex/agents/chatgpt-pro.toml` (`chatgpt-web/thinking`) a partir de um pai `gpt-5.6-sol` | pai respondeu `AGENT SAID: PONG` (e `PING` na 2.ª corrida); o filho correu numa conversa própria |

Nota: `codex exec` lê o prompt de stdin quando não é TTY — nos scripts usar `< /dev/null`.

## M5 — imagens, arquivamento, compaction ✅

- Imagens: ver `attachments.rs` acima; `ContentItem::InputImage{data:…}` → ficheiro → upload pelo
  driver; placeholder `[image_attachment: nome]` + nota no header.
- Arquivar: `chatgpt_web::archive_thread_conversation(config, thread_id)` (gate
  `archive_on_shutdown`, `PATCH {is_archived:true}` com 10 s, depois `sessions::forget`; no-op sem
  registo, logo agnóstico ao provider). Chamado (a) no shutdown do **root** (`session/handlers.rs
  shutdown`, via `Session::is_root_thread`) para o root **e todos os agentes vivos da árvore**
  (`AgentControl::archive_chatgpt_web_conversations`), e (b) em `close_agent`
  (`agent/control/legacy.rs`) para o agente e descendentes — nunca na eviction, que passa por
  `shutdown_and_wait` e reconstrói o agente depois. Verificado: a 2.ª corrida de `spawn_agent`
  arquivou a conversa do filho ao sair. Consequência: cada `codex exec` arquiva a conversa ao sair,
  pelo que `exec resume` replay (26 k chars) em vez de estender — desligar com
  `-c chatgpt_web.archive_on_shutdown=false` quando se quer continuidade entre `exec`s. "Enviar
  numa conversa arquivada" ficou moot: o registo é esquecido ao arquivar.
- Compaction: `plan_request` reconhece o prompt de sumarização (`config.compact_prompt` ou
  `SUMMARIZATION_PROMPT`) → replay com o contrato de checkpoint em conversa nova, continuidade
  intocada, conversa arquivada após a resposta (`archive_conversation`); `compact.rs:128` continua
  a saltar só o Claude. Testes em `history_tests`/`prompt_tests`; o turno seguinte replay porque
  `replace_compacted_history` muda o prefixo (coberto por `a_shorter_history_than_delivered_restarts`
  e `echoed_items_are_dropped…`).
- Pendente: um filho `chatgpt-pro` da 1.ª corrida (binário anterior à arquivagem em árvore) ficou
  sem arquivar (`6a8fc37a…`, registo expira em 7 d); sub-agentes não são retomáveis por `exec resume`.


## C2 — registo do conector (`connector/daemon/registry*.rs`) ✅

- `registry.rs`: **planner puro** `plan(&Observed, &DesiredConnector) -> Result<Vec<RegistryOp>, String>` — (1) apaga nomes de contratos antigos (`<nome>` quando a versão > 1, `<nome> <n>` com n < versão) e sobras `<nome> Spike*`; (2) registo persistido com o mesmo endpoint (`tunnel:<id>` ou URL pública) e conector+link ainda existentes → só `VerifyActions` + `Persist`; sem registo mas com um conector do mesmo nome já apontado ao endpoint → adota-o (apaga duplicados); (3) senão apaga link(s)+conector(es) do nome, `Create`, `Link` (noauth), `VerifyActions`, `Persist`; (4) `developer_mode == false` → `EnableDeveloperMode` primeiro; com `openai`, `tunnel_id` fora de `GET /aip/connectors/mcp/tunnels` → recusa com dica `codex chatgpt-web setup`. Ids ainda desconhecidos viajam como `ConnectorRef::Created`/`LinkRef::Created` e o executor resolve-os. **Executor** `execute` (pede `refresh_actions` uma vez quando o create veio sem ações; mismatch → `VerifyMismatch`), `observe` (403 "Developer mode is required" nas listas → `developer_mode = Some(false)`), `reconcile` (observe → plan → execute; 403 a meio → liga o Developer Mode uma vez e recomeça; mismatch → apaga e recria uma vez; grava `connector.json` com `write_secret`), `delete_recorded`. **`RegistryService`**: serializa (semáforo), backoff de falhas 2/5/10 s → 60 s (não re-tenta antes de `retry_at`), `BrowserUnavailable` re-tenta em 60 s, `hook()` para `POST /v1/registry/reconcile`, `spawn_watcher` (reconcilia no arranque, quando o túnel fica `Ready` com endpoint diferente do persistido, e após o backoff).
- `registry_api.rs`: `ChromeMcpPageApi` (`ConnectorApi` real) — cada op é um `fetch` da página via `page_scripts::api_call_with_headers` + `OAI-Product-Sku: CONNECTOR_SETTING`; empresta uma aba `chatgpt.com` existente (só `browser_eval`, nunca navega) ou cria uma dedicada registada em `tabs.json` com o pid do daemon e fecha-a no fim; backoff 429 `[2,5,10]s`; 404 em DELETE = já apagado; classificação: 403 "developer mode" → `developer_mode_required`, 401/"not logged in" → `login_required`, daemon/eval inacessível → `browser_unavailable`. Tabela de endpoints exatamente como em [api_shapes.md](./api_shapes.md) (sem `purpose`; dois `list_accessible`; `auth_request.supported_auth: []`; `links/noauth {connector_id, name, action_names: []}`).
- Wiring: `DaemonRunConfig.live_registry` (+ `with_live_registry()`, usado pelo CLI) constrói o serviço em `start()` quando não há hook explícito; `ControlState.trigger_reconcile_if_needed()` dispara um reconcile em background em `POST /v1/turns` quando o estado ≠ `Verified` (deduplicado por `reconcile_in_flight`); facade `codex_core::chatgpt_web_daemon::{registry, registry_api}`; CLI `registry show` imprime o `connector.json` + estado vivo do daemon; `registry delete` apaga diretamente via chrome-mcp (não precisa do daemon).
- Fora do meu escopo mas necessário para o clippy do crate compilar: `control.rs`/`state.rs` (closures redundantes) e `tunnel.rs` (dois `expect` em regex estático → `OnceLock<Option<Regex>>`).
- **Live** (`live_registry_reconciles_a_manual_url`, conta real): URL inalcançável → `POST /aip/connectors/mcp` devolve **HTTP 424** `{"kind":"network","type":"mcp_error"}` (o ChatGPT liga-se ao servidor no create — facto novo, registado no api_shapes.md); URL alcançável (spike `server.mjs` + cloudflared) → create/link/verify com **6 ações** em ~18 s, e `delete_recorded` não deixou nada.

Gate: 28 testes em `registry_tests.rs` (planner, executor com `FakeApi`, serviço/backoff/watcher, `ChromeMcpPageApi` com daemon fake — 429→backoff→ok, corpos/headers capturados, 403/login, aba emprestada vs criada+fechada, `POST /v1/turns` dispara reconcile) + 1 live verde; clippy limpo nos ficheiros novos.
