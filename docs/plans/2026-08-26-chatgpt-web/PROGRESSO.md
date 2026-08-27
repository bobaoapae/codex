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

## M6 — seam do conector no provider ✅

`core/src/chatgpt_web/connector/mod.rs`:

- `trait ConnectorBroker { begin_turn(BeginTurn) -> ConnectorTurn; prompt_contract(&ConnectorTurn) -> Vec<String>; end_turn(turn_token, reason) }`; `ConnectorTurn{turn_token, connector_name, requests: mpsc::Receiver<ToolRequest>}`; `ToolRequest{call_id, target: contract::CallTarget, respond: oneshot::Sender<FunctionCallOutputPayload>}`.
- `tool_summaries(&[ToolSpec]) -> (Vec<ToolSummary>, ExecTool, bool)`: achata o namespace default para `None`, mantém os namespaces MCP, detecta `exec_command` vs `shell` e o `apply_patch` freeform.

`core/src/chatgpt_web/mod.rs` (arms do conector):

- `ChatGptWebThreadState.live_turn: Mutex<Option<LiveTurn>>` guarda o turno suspenso entre `stream()`s (turn_token, conversa, `requests`, responders pendentes, `ReplyTracker`, `echoed`, metadados de continuidade).
- `run_connector_turn`: reattach (se o `input` já traz `FunctionCallOutput` para todo pending → responde os `oneshot`, retoma o poll) ou turno novo (slot, `plan_request`, `begin_turn`, prompt `PromptMode::Connector(prompt_contract)`, health, `send` com `mention` só em conversa nova, grava continuidade `message_landed_unanswered=true`, entra no loop).
- `connector_loop`: `select!` entre `consumer_dropped`, `requests.recv()` (batch 15 ms → emite `OutputItemAdded/Done(FunctionCall|CustomToolCall)` por chamada + `Completed{end_turn: Some(false)}`, estaciona o `LiveTurn`, retorna), poll da conversa (`TrackMode::Connector` → `apply_delta`; conclusão exige todo `api_tool` respondido), e watchdog de stall. `Done` → `Completed{end_turn: Some(true)}` + `end_turn` do broker.
- `target_to_item` (Function→`FunctionCall{namespace, arguments: string}`, Custom→`CustomToolCall{name:"apply_patch"}`), `extract_output` (casa `FunctionCallOutput`/`CustomToolCallOutput` por `call_id`), `collect_batch`.
- Compaction em modo conector: `run_compaction_turn` responde numa conversa descartável com `PromptMode::Compaction`, sem broker, e arquiva.
- `stream.rs`: `apply_delta`/`DeltaStep` extraídos do `PollLoop` e partilhados; `ReplyTracker::reset_open` para o reattach com assembler novo.

Divergência: o broker é anexado por um **process-global** (`connector_broker`, `OnceCell` por `CODEX_HOME` → uma sessão/`session_id` por processo) em vez de `turn.rs` — a construção do broker é assíncrona (regista sessão), o que não cabe no ponto de attach síncrono do host Claude; `workspace.connector` continua a existir como seam para injeção em teste. Sem edição de `turn.rs`/`client.rs`.

## C3 — cliente de sessão do conector ✅

`core/src/chatgpt_web/connector/client.rs` — `DaemonSessionBroker`:

- `connect(control_url, token, connector_name)` regista **uma** sessão (`POST /v1/sessions`), sobe heartbeat (10 s) e **um** long-poll partilhado (`GET /v1/sessions/{sid}/calls`, dedupe por `call_id`, ack por `seq`) que roteia cada batch para o canal do `turn_token` dono; `Drop` faz `DELETE` da sessão.
- `begin_turn`: espera `Verified` (poll do `/healthz`, mensagens acionáveis para `developer_mode_off`/`browser_unavailable`), cunha `turn_token` (32 chars base64url), `POST /v1/turns` com os `ToolSummary`, cria o canal e regista o sink; `prompt_contract` (nome, contrato do `turn_token`, as 6 tools); `end_turn` → `DELETE /v1/turns`.
- Resultado de cada chamada: `FunctionCallOutputPayload` → `POST /v1/calls/{id}/result` (texto; imagens `data:` → `ResultContent::Image`; `success==Some(false)` → `is_error`).

## C4 — anexação no browser ✅

`core/src/chatgpt_web/connector/connector_attach.rs`:

- `mention_and_compose_script(name, text)`: seleciona o conector (idempotente — pill `[data-id^="plugin:"][data-keyword=<name>]` já presente é no-op) via `@<primeira palavra>` → espera `.__menu-item[tabindex="0"]` com o título exato → `ArrowDown` sintético até `data-highlighted` → `Enter` → verifica a pill, e **acrescenta** o texto depois da pill (sem select-all, que apagaria a pill). Usado por `ops::send` quando `SendRequest.mention` está definido; `ops.rs` ganhou o campo `mention` e escolhe este script em vez de `set_composer_text`.
- `approval_script(name, prefer_always)` + `ConnectorAttach::approve_on_conversation`: clica `Sempre permitir|Allow always|Permitir uma vez|Allow once` (regex PT/EN) no `[data-testid="tool-approval-card"]`; o `connector_loop` chama a cada 2 polls com `connector_auto_approve_ui`.

Gate: `cargo test -p codex-core --lib chatgpt_web::connector` → **85 passed** (seam `tool_summaries`, scripts de attach, mapeamento de resultado do cliente, round-trip completo `DaemonSessionBroker`↔daemon in-process sem browser, `target_to_item`/`extract_output`); integração `--test all -- chatgpt_web_connector` → 4/4; `chatgpt_web::`+`claude_code::` → 316/316; clippy limpo nos ficheiros deste bloco; `cargo build -p codex-cli` ok.

**Pendente (validação):** o smoke live do modo conector (`codex chatgpt-web daemon` + cloudflared + turno real com `codex_exec` → `CONNECTOR_OK`) não foi corrido neste bloco — envolve registar o conector "Codex Native" na conta real e conduzir uma volta completa no browser, melhor executado interativamente pelo orquestrador (que coordena daemon + Chrome e observa a corrida), e o túnel `openai` continua a depender do setup do usuário no platform.openai.com.

## C5 — smoke ponta-a-ponta, endurecimento, docs, gates ✅ (conector ao vivo)

Smoke do modo conector conduzido ao vivo (binário `target/debug/codex.exe`, Chrome logado,
chrome-mcp 127.0.0.1:8848, Developer Mode on) com `tunnel = "cloudflared"` (o `openai`
continua pendente do setup do usuário):

| passo | resultado |
|---|---|
| `codex chatgpt-web daemon` (autostart) + tunnel + registo | Ready + `Codex Native` verified (6 actions) em ~75 s |
| `codex exec -m chatgpt-web/instant "…codex_exec… echo CONNECTOR_OK"` | **CONNECTOR_OK** (176 s): transcript com célula `pwsh.exe -Command 'echo CONNECTOR_OK'` executada pelo Codex, resposta `CONNECTOR_OK` — teste de aceitação do plano ✅ |
| `codex_apply_patch` (turno único, conversa nova) | **hello.txt = HELLO** no disco (via `CustomToolCall`); modelo respondeu `done` ✅ |
| matar o cloudflared | daemon reconecta URL nova + re-reconcilia para `verified` em ~88 s (< 90 s) ✅ |
| `codex chatgpt-web stop` | limpo; sem quick-tunnels órfãos ✅ |

**Bugs encontrados e corrigidos no smoke:**
1. **Readiness do cloudflared vs DNS local** (`connector/daemon/tunnel.rs`): o resolver local
   deste PC devolve NXDOMAIN para um `*.trycloudflare.com` fresco por muito tempo (negative
   caching) — provado: DoH resolvia em ~20 s, o resolver local nunca em 90 s — enquanto o ChatGPT
   (que resolve pela Cloudflare) alcança. O healthz público era sondado *desta* máquina e o túnel
   ficava eternamente `connecting`. Agora: "Registered tunnel connection" + probe que falha **antes**
   de qualquer resposta HTTP = ready-mas-não-verificado-localmente (o registo do conector, que dá
   424 se o ChatGPT não alcança, é o cheque efetivo); um probe que recebe resposta não-2xx mantém
   `down`. Novo `enum Probe {Ok, HttpError, Unreachable}`.
2. **Overrides do daemon autostart** (`connector/daemon/mod.rs`, `mod.rs`, `chatgpt_web_cmd.rs`):
   uma sessão sob `-c chatgpt_web.tunnel="cloudflared"` autostartava o daemon com as settings do
   *ficheiro* (tunnel `openai` → `fatal: no tunnel_id`). Novo `daemon_overrides(&settings)` passa as
   chaves que o daemon consome como `-c` ao processo destacado (`ensure_daemon` e `spawn_detached`
   recebem `overrides`; segredos nunca viajam — só o caminho do ficheiro de chave); `reconcile_via_daemon`
   e `wait_tunnel_ready` também.
3. **turn_token na extensão do conector** (`prompt.rs`): um turno de continuação (novo `codex exec`
   na mesma conversa) cunha um `turn_token` **novo**, mas o render de extensão mandava só os itens
   novos — sem o contrato — e o modelo lia o token do turno anterior no histórico e recusava
   ("esse token era do turno anterior"). Agora a extensão em modo `Connector` reafirma as linhas do
   contrato (com o token fresco) antes dos itens novos. Teste novo em `prompt_tests.rs`.
4. **`api_tool` sem nó `tool` no mapping** (`driver/api.rs`): para um conector custom o resultado da
   tool **não** aparece como nó `tool` em `/backend-api/conversation` — o mapping salta do pedido
   `api_tool.call_tool` direto para a mensagem seguinte do assistente. `has_result` passou a contar
   qualquer mensagem posterior na cadeia como "respondido", senão um turno de conector nunca
   concluía. Teste ajustado.

**Endurecimento (verificado, já vinha de C1/C2):** rate limiter no servidor público
(30 chamadas/10 s + 10 claims falhados/min), resultado de chamada só aceite da sessão dona
(`complete(session_id, call_id)` → `WrongSession` → 403), sem segredos em logs (auditado: nenhum
`info!/warn!/error!/debug!` do módulo `connector` imprime token/secret/key/bearer cru; o path
secreto do túnel saiu do único `warn!` que o continha), body cap 8 MiB, `legacy_session_mode:false`
+ SSE, path secreto por start com comparação em tempo constante. `scripts/chatgpt_web_connector_smoke.ps1`
adicionado (switch `-Tunnel cloudflared|openai`).

**Docs:** admonição "Status" de `docs/chatgpt_web_agents.md` atualizada para o que o smoke provou
(codex_exec/apply_patch ao vivo, reconexão de túnel) e o que falta (túnel `openai`; follow-up
como processo `exec resume` separado, onde o ChatGPT re-pede o card de aprovação para o token novo
e o auto-approver pode não clicar a tempo — turno único e continuação no mesmo processo funcionam).

**Pendente / não verificado ao vivo:** túnel `openai` (setup do usuário no platform.openai.com);
modos Thinking/Extra-high/Pro com tools de escrita (só Instant confirmado); `exec resume` como
processo separado (card de aprovação do token novo). `config_toml.rs` não mudou → schema intocado.

**Gates (C5, `RUST_MIN_STACK=8388608`):** clippy `-p codex-core -p codex-cli -p codex-config -p codex-models-manager -p codex-model-provider-info --lib --bins --tests` limpo (3 avisos residuais de C1 corrigidos: `collapsible_if` em `broker.rs`, `collapsible_match` em `tunnel.rs`, `err().expect()` em `daemon/mod_tests.rs`); `cargo fmt --check` core/cli ok (churn EOL do buildifier nos `.bazel` revertido); `cargo check --workspace` verde; `cargo test`: model-provider-info 29/29, models-manager 54/54, config 284/284, `--test all -- chatgpt_web_connector` 4/4, `chatgpt_web::` + `claude_code::` 316/316 (8 ignored = live), `chatgpt_web::connector` 85/85, `codex-cli --lib` 13/13, `codex-core --lib` **2559 passed / 3 failed / 11 ignored** — as 3 falhas são as pré-existentes do débito conhecido (`agents_md_paths_preserve_symlinked_cwd` privilégio de symlink; `environment_selection::blocking_snapshot_waits_for_starting_environment` e `session::turn::tests::post_sampling_token_estimate_is_disabled_by_always_on_sinks` flakes de execução paralela), nenhuma em `chatgpt_web`.

## Setup `openai` + testes de escrita por modo (conta `joao@joaoborges.dev`) ✅/⚠️

Conta web trocada para `joao@joaoborges.dev` (personal, plano `pro`), logada também no platform.

**Platform (pelo browser, `mcp__chrome`):**
- *Settings → Organization → Tunnels → Create tunnel*: nome **e descrição** obrigatórios; organização
  pré-selecionada (`SURFTANk`); criado `tunnel_6a902697a0888191963057ca639226fa` ("Codex Native").
- *API keys → Create new secret key*: owner *You*, projeto *Default project*, *Restricted* com
  `Tunnels` = `Read` + `Use` (listbox multi-select; a linha fica "All selected", "2 selected
  permissions"). O submit disparou duas vezes (dois clicks sintéticos) → duas keys criadas; a
  duplicada (`key_kpcuEEAmJ2trsu1K`) foi **revogada** pela UI. A key restante
  (`key_yCwcZ8GyzKysmtbB`, nome `codex-chatgpt-web-tunnel`) está em
  `%USERPROFILE%\.codex\chatgpt_web\tunnel.key` (164 bytes, sem newline).
- **Facto novo (bloqueante):** um tunnel recém-criado **não aparece** para a conta ChatGPT
  (`GET /backend-api/aip/connectors/mcp/tunnels` → `{"tunnels":[]}`, com Developer Mode já ligado
  automaticamente pelo registry). É preciso **partilhar o tunnel com a conta**: *Edit tunnel →
  ChatGPT workspaces → procurar o id exato* — para conta personal o id é o `account_id` de
  `/backend-api/accounts/check/v4-2023-04-27` (`fbf63138-24fb-489e-8c2b-49826f916056`); aparece
  como opção, *Save*, e o tunnel fica visível de imediato. Mensagem do planner do registry e
  `docs/chatgpt_web_agents.md` (passo 2) atualizados com este procedimento.

**`codex chatgpt-web setup --tunnel-id … --api-key-file …`** (binário debug):
- Backup `config.toml.bak-chatgpt-web-20260827-090243`; gravou `[chatgpt_web] tunnel_id` +
  `tunnel = "openai"`; download de `tunnel-client-v0.0.12-windows-amd64.zip` da release pinada com
  SHA-256 `2a2804…4356` = `SHA256SUMS.txt` real ✅ (o zip traz também um `cloudflared.exe` bundled,
  ignorado); binário em `chatgpt_web/bin/tunnel-client-v0.0.12.exe` + manifesto.
- **Flags confirmadas com `tunnel-client run --help` (0.0.12+881c9a8):** `--control-plane.tunnel-id`,
  `--health.listen-addr 127.0.0.1:0`, `--health.url-file`, `--log.format json`, `--log.level info`;
  env `CONTROL_PLANE_API_KEY` (preferido; `OPENAI_API_KEY` como fallback do próprio client),
  `MCP_SERVER_URL=url=…,channel=main`, `MCP_STARTUP_WAIT_TIMEOUT` (duração Go, ex. `60s`). Nada a
  mudar em `tunnel.rs`. `tunnel: ready` < 1 s após o spawn; `readyz` local responde.
- Registry na conta nova: Developer Mode ligado automaticamente (PATCH) ✅; recusa correta enquanto
  o tunnel não estava partilhado; depois `registry reconcile` → **verified** com `tunnel_id` no
  body do create (`asdk_app_6a9029caf6dc8191a5910f056eb5d423`, 6 actions) ✅.

**Testes de escrita por modo (`tools = "connector"`, túnel `openai`):**

| modo | resultado |
|---|---|
| `chatgpt-web/instant` | ✅ `codex_apply_patch` → `mode.txt = INSTANT` e `codex_exec type mode.txt` → `INSTANT`; resposta "Exact output: INSTANT" entregue ao Codex. 741 s no total por causa da tempestade de 429 (abaixo) — um `ERROR: Reconnecting… 1/5` a meio. |
| `chatgpt-web/thinking` | ✅ ambas as tools executadas (`mode.txt = THINKING`, `codex_exec` → `THINKING`) e a conversa terminou com `end_turn:true` "THINKING" (verificado por API); a entrega ao Codex ficou presa na tempestade de 429 (run abortado por mim aos ~10 min; conversa escondida). 1.ª tentativa falhou no @mention (`connector row not found`) — fallback de ativação adicionado; 2.ª/3.ª caíram numa janela em que o chatgpt.com deslogou no Chrome (o usuário voltou a entrar). |
| `chatgpt-web/extra-high` | ✅ tools (`mode.txt = EXTRAHIGH`, `codex_exec` chamado). ⚠️ `exact level selection via menu failed (submenu not found)` → correu com o default do slug thinking (o picker atual é a variante slider que o `menu_select` não conduz); label do composer lido como "Pro". Entrega final também presa nos 429 (run abortado). |
| `chatgpt-web/pro` | ✅ tools (`mode.txt = PRO`, `codex_exec` chamado em ~1 min); ⚠️ entrega final ao Codex falhou com `no progress for 1200s; generation stopped` (watchdog) porque as leituras da conversa ficaram em 429 o run inteiro (cooldown 20→40 s a funcionar, mas a conta continuou limitada mesmo a 1 pedido/min). Estado final da conversa não pôde ser confirmado por API (429 também num GET manual). |
| `exec resume --last` em processo separado | ⏸ não executado: a conta ficou limitada (429) no endpoint da conversa no fim da sessão, o que dominaria o resultado. Script pronto em `%TEMP%\cgw-modes
un_resume.sh` (turno instant com `archive_on_shutdown=false` + `exec resume --last`). |

**Tempestade de 429 (facto novo, verificado):** `GET /backend-api/conversation/<id>` é limitado por conta;
o poll a 2,5 s + 3 retries internos (2/5/10 s) por leitura manteve a conta em "Too many requests"
durante minutos — até um GET manual de outra aba dava 429 — e o turno nunca via a resposta
final (no modo instant o loop chegou a 8 falhas seguidas → `Stream` → "Reconnecting"). Correções:
leitura de poll sem retries internos (`read_conversation` com `with_backoff(vec![])`), cooldown
20→120 s após 429 sem contar como falha de leitura, e janela lenta (poll ≥ 15 s durante 5 min após
o último 429) em ambos os loops (`stream::PollLoop` e `connector_loop`).

**Bugs corrigidos nesta fase:**
1. `connector/daemon/mod.rs::spawn_detached` — no Windows o daemon destacado herdava os handles de
   pipe do CLI (`Stdio::null()` só troca os std handles; `CreateProcess` copia todos os herdáveis),
   por isso `codex chatgpt-web setup | tail` (ou qualquer agente/CI) ficava pendurado até o daemon
   morrer — o "hang" único que o C5 viu. Agora `SetHandleInformation(HANDLE_FLAG_INHERIT, 0)` nos
   três std handles antes do spawn (`detach_std_handles_from_inheritance`). Reproduzido: o `setup`
   pendurado terminou no instante em que o daemon foi parado.
2. `driver/ops.rs` + `driver/tabs.rs` — fallback de ativação do @mention (plano C4 (1)): novo
   `SendRequest.mention_strategy: MentionStrategy{Auto,BackgroundOnly,Activate}` (mapeado de
   `[chatgpt_web] connector_mention_strategy`); em `Auto`, falhas do menu (`connector row not
   found`, `could not highlight`, `menu closed`, `pill did not appear`) re-tentam dentro de
   `TabPool::with_activated_on_keep` (ativa → compose → restaura foco, **sem reload** para não
   perder a pill); `Activate` ativa sempre; `BackgroundOnly` nunca.
3. `chatgpt_web/mod.rs::connector_loop` — erros transitórios de `read_conversation` (timeout de
   eval em aba oculta, 5xx) já não terminam o turno (o que retirava o `turn_token` que o ChatGPT
   ainda usa e forçava um "Reconnecting"); tolera até `MAX_CONSECUTIVE_READ_FAILURES` (8) como o
   poll loop do modo `none`.
4. `connector/daemon/registry_api.rs` — (a) uma aba `chatgpt.com` emprestada pode não estar logada
   (outra janela/perfil do mesmo Chrome responde `/api/auth/session` sem token) e o reconcile
   falhava com "not logged in" repetidamente: agora cada candidata é sondada (`codex-login-probe`)
   e as sem sessão são saltadas; uma aba emprestada que perde a sessão solta o lease; (b) a aba
   dedicada criada reportava `readyState === "complete"` ainda em `about:blank` e o `fetch`
   relativo falhava com "Failed to parse URL" — `wait_loaded` exige agora `location.href` na
   nossa origem.
5. `connector/daemon/registry.rs` — mensagem do "tunnel não visível" explica o passo de partilha
   com a ChatGPT workspace/account id.
6. `driver/ops.rs::read_conversation` + `stream.rs` + `mod.rs::connector_loop` — tratamento de
   429 (acima): `RATE_LIMIT_COOLDOWN_{MIN,MAX}`, `RATE_LIMIT_SLOW_{WINDOW,POLL}`,
   `effective_poll_interval`, `next_rate_limit_cooldown`.
7. Limitação registada (não corrigida): `set_level_via_menu` não encontra o submenu de esforço na
   UI atual (slider) → `high`/`extra-high` correm como `thinking`; documentado em
   `docs/chatgpt_web_agents.md` (Model lines).

**Gates:** `cargo test -p codex-core --lib chatgpt_web::` **248 passed / 0 failed / 8 ignored**;
`cargo clippy -p codex-core -p codex-cli --lib --bins --tests` limpo; rustfmt nos ficheiros
alterados; integração `--test all -- chatgpt_web_connector` **4/4**; `cargo fmt --check` core/cli ok.

**Estado final / pendente do usuário:** o usuário voltou a entrar no chatgpt.com a meio (a sessão
tinha caído às ~12:27 UTC); no fim da sessão a conta está **limitada (429)** em
`GET /backend-api/conversation/<id>` — deixar arrefecer antes de novos turnos. Ficam por verificar ao
vivo: a entrega final em Thinking/Extra-high/Pro sem 429 (as tools de escrita funcionam nos 4
modos), o `exec resume` como processo separado, e o picker de esforço (slider) para `high`/`extra-high`.
Conversas dos testes: instant arquivada pelo Codex; thinking escondida; extra-high e pro ficaram
visíveis (PATCH deu 429) — esconder à mão ou deixar. O daemon ficou parado (autostart no próximo
turno com o binário novo). Abas dedicadas dos testes fechadas.

## 429, slider, defaults ✅ (conta `joao@joaoborges.dev`, túnel `openai`, config como está)

Objetivo do usuário: «Evitar 429, corrigir slider, e deixar funcionando por completo sem config nova».

**1. Progresso pelo DOM, API só quando precisa (`stream.rs`, `page_scripts.rs`, `ops.rs`, `api.rs`, `mod.rs`):**
- Novo script `page_scripts::dom_progress()` (síncrono, sem `fetch`): lê da aba `{url, generating (botão Stop), streaming ([data-streaming-response-status]), lastUserText, assistantTurns, assistantChars, lastAssistantDone (copy-turn-action-button), lastAssistantId}`. `ChatGptOps::dom_progress(conv)` só corre na aba **vinculada** à conversa (`TabPool::bound_tab_id`) e devolve `None` se a aba não está em `/c/<id>`.
- `stream::PollScheduler` (puro, testado): a cada tick (`poll_interval_ms`, 2,5 s) lê o DOM; mudança = progresso para o watchdog; a API é lida **só** quando (a) o DOM diz que a resposta acabou (imediato, depois a cada 10 s até a API confirmar `end_turn`), (b) o texto cresceu e passaram ≥ 30 s desde a última leitura, (c) nada mudou há ≥ 60 s (safety, ex.: Pro a correr server-side ou aba navegada). O texto do DOM **não** é emitido (perde o markdown); as deltas continuam a vir da API. `PollLoop` e `connector_loop` usam o mesmo scheduler; o card de aprovação continua a ser sondado pelo DOM.
- `api::BackendLimiter` (token bucket process-wide, 1 chamada/3 s, `ChatGptApi::with_backend_limiter()` em `ChatGptOps::api_on`): a soma de todas as turns/threads do processo nunca passa desse ritmo; o registo do conector (daemon) mantém o seu próprio ritmo.
- Âncora tolerante (`anchor_matches`): o texto do último user turn na API/DOM traz o prefixo `@Codex Native ` (pill do conector) — a comparação passou de igualdade para "contém os primeiros 80 chars do que enviámos". Antes disso um turno de conector nunca via a conclusão (lia a API a cada 30 s até ao watchdog).
- Regra de idle (`ReplyTracker::observe`): uma conversa Pro terminada mantém `async_status: 4`; agora `end_turn: true` + `finished_successfully` na última mensagem de texto conclui o turno mesmo com o flag async ligado (antes: 10 min de leituras a cada 10 s até ao watchdog).
- Medido (log `chatgpt_web backend call:`): turno `none` curto = **3 chamadas** (models, 1 GET na conclusão, PATCH arquivar) em 36–38 s; conector instant = 3; thinking = 5 (2 min, com 2 tools); pro = 5 (76 s); `exec resume` = 1. Antes: ~24/min por turno.

**2. Slider de esforço (`page_scripts::menu_select`):** o picker atual é `span[data-animated-slider-trigger] > button[aria-haspopup=menu]`; o menu (`[data-testid="composer-intelligence-picker-content"]`, `data-view=simple`) tem um único `menuitem` com `aria-keyshortcuts="ArrowLeft ArrowRight"` cujo texto irmão diz "`<label>, <n> de 5.`" — posições Instantâneo(1) · Médio(2) · Alto(3) · Extra alto(4) · Pro(5). `keydown` sintético de ArrowLeft/ArrowRight no item focado move o slider e o label do trigger acompanha; Escape/click sintéticos e até `browser_press`/`browser_click` **não fecham** o menu (só o reload, que a `with_activated_on` já faz); a seleção **persiste** através do reload, mas um `?model=` na URL repõe o default (por isso: navegar primeiro, selecionar depois). `menu_select` percorre o slider até o label casar com a regex (ArrowLeft até ao início, depois ArrowRight), mantém o submenu antigo como fallback, e agora **espera até 6 s pelo trigger** (após a navegação ele monta tarde; falhar logo herdava o nível persistido da run anterior — foi o `submenu not found` das runs anteriores). A `send` regista a nota `effort level set through the picker: <label>`.
- Verificado ao vivo (`tools = "none"`): `chatgpt-web/extra-high` → "Extra alto", depois `chatgpt-web/high` → "Alto" (movimento nos dois sentidos, label verificado, sem nota de mismatch).
- Bug apanhado no caminho: logo após o reload do picker o primeiro click sintético em Send é engolido (composer ainda a hidratar; sem Stop, sem `/c/`, texto ainda no composer). `prepare_and_send` verifica pelo `composer_state` que nada foi enviado (composer cheio, sem `/c/` em chat novo; composer cheio em continuação) e clica de novo (≤ 2×). O mesmo engolimento aparecia no `exec resume` (navegar para a conversa + compor + click) — era a causa do "confirm: the send click did not land".

**3. Sem config nova:** `ChatGptWebSettings::from_toml` resolve `tools` ausente como `connector` quando há `tunnel_id` (ou `tunnel = cloudflared|manual`), `none` caso contrário; `tools = "none"` explícito continua a valer. Doc do campo, `docs/config.md`, `docs/chatgpt_web_agents.md` e `config.schema.json` atualizados. Role `~/.codex/agents/chatgpt-pro.toml` reescrito para o modo conector (usa as tools do Codex Native quando presentes; sem conector, comportamento anterior).

**Resultados ao vivo (binário debug, `-c model_provider=chatgpt_web -m …` e nada mais de `[chatgpt_web]`):**

| caso | resultado |
|---|---|
| `high` / `extra-high` (none) | ✅ 38 s / 37 s, 3 chamadas de backend cada, picker "Alto" / "Extra alto" confirmado |
| conector `instant` | ⚠️ 71 s, 3 chamadas, resposta entregue; **`codex_apply_patch` bloqueado pela camada de segurança da OpenAI** (ver abaixo); `codex_exec` correu. Na run anterior o inverso (patch ok, exec bloqueado 2×) |
| conector `thinking` | ✅ 115 s, 5 chamadas, `mode.txt = THINKING`, `codex_exec` → THINKING, resposta entregue |
| conector `pro` | ✅ 76 s, 5 chamadas, `mode.txt = PRO`, resposta entregue (1.ª run ficou presa pelo `async_status: 4` — corrigido) |
| `exec resume --last` em **processo separado** (instant, `archive_on_shutdown=false` só para o teste) | ✅ 69 s, 1 chamada, extensão da mesma conversa, `codex_exec echo SECOND_OK` executado e reportado (precisa de `-m chatgpt-web/…` também no resume: a sessão gravou `gpt-5.6-sol`) |
| `spawn_agent` role `chatgpt-pro` a partir de `gpt-5.6-sol` | ✅ filho: `agent.txt = AGENT_OK` via Codex Native, `task_complete` aos 15:30:34, `wait_agent` do pai devolveu no mesmo segundo; ⚠️ o **pai** (provider OpenAI) não produziu mais nada nos 8 min seguintes até eu o matar — sem eventos no rollout depois do `wait_agent`; não reproduzido, fora do `chatgpt_web` |

**Facto novo (OpenAI, não nosso):** em 3 de 6 turnos de conector desta sessão uma chamada foi recusada pelo ChatGPT com o resultado de tool «Esta ferramenta foi bloqueada pelas configurações de segurança da OpenAI. Verifique novamente o que está enviando.» — ora `codex_exec` (`type mode.txt`, `Get-Content .\mode.txt`, `echo FIRST_OK`), ora `codex_apply_patch`; o pedido **nunca chega ao daemon** (sem `call request=` no `daemon.log`), não há card de aprovação no DOM (o novo log `approval card found but no known button` nunca disparou), e a mesma chamada passa noutra run (thinking e pro passaram ambas as tools). É a camada de segurança server-side para conectores em Developer Mode; o modelo relata honestamente ("blocked by the execution security layer") e o turno termina normalmente. Sem contramedida no cliente; documentado em *Limits*.

**Gates:** `cargo test -p codex-core --lib -- chatgpt_web:: config::chatgpt_web` **255 passed / 0 failed / 8 ignored** (novos: scheduler ×2, poll loop com DOM, limiter, dom_progress/slider scripts, defaults de `tools`, Pro `async_status`); `--test all -- chatgpt_web_connector` 4/4; clippy e `cargo check --workspace` abaixo; `just fmt` (churn EOL dos `.bazel`/`justfile` revertido); `config.schema.json` regenerado.

**Conversas de teste:** as das runs que completaram foram arquivadas pelo próprio Codex; as 3 abortadas/killed (instant 1.ª, pro 1.ª, resume) e a do teste manual do slider foram escondidas por `PATCH is_visible:false`. O daemon debug foi parado (`codex chatgpt-web stop`) — o binário hot-swapped autostarta o seu.
