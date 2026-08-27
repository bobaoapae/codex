# Plano: provider `chatgpt_web` no fork do Codex (driver nativo + modo conector automático)

## Contexto

Objetivo: usar o **ChatGPT Pro web como agente dentro do Codex** (análise, criação, auditoria), sem uso direto do chatgpt.com. Hoje isso só existe via o `chatgpt-pro-mcp` (Node) como ferramenta de consulta. O fork já tem o padrão "modelo servido localmente" (`WireApi::ClaudeCode`, `models-manager/local_models.rs`, `core/src/claude_code/`), então o `chatgpt_web` entra pelo mesmo trilho.

Decisões já tomadas com o usuário:
- **Opção 2 de transporte**: o Rust fala **direto com o daemon chrome-mcp** (`http://127.0.0.1:8848/mcp`, Streamable HTTP + bearer), portando a lógica de `chatgpt-pro-mcp/src/{ops,tab,api,page-scripts}.ts` (~3k linhas TS) para Rust. O `chatgpt-pro-mcp` continua existindo intocado para o Claude Code.
- **Modo conector automático**: ChatGPT chama as tools do Codex nativamente (function calling real) via conector MCP custom (Developer Mode). Túnel padrão = **OpenAI Secure MCP Tunnel oficial** (`tunnel-client`, decisão do usuário: "se tem um oficial, que funciona, melhor usar"): só conexões de saída, sem URL pública, `tunnel_id` estável → o conector é criado **uma vez** (via API do backend do ChatGPT a partir da página, sem UI). Setup único e gratuito: um Tunnel em `platform.openai.com/settings/organization/tunnels` (mesma conta do ChatGPT) + uma API key **restrita** com `Tunnels: Read + Use`. Fallback `tunnel = "cloudflared"` (quick tunnel `*.trycloudflare.com`, URL nova a cada start → conector recriado automaticamente).
- Quem executa ferramentas é sempre o Codex (sandbox, approvals, `apply_patch`, `exec_command`). O browser nunca vê o filesystem.

Fatos verificados ao vivo em 2026-08-26 (conta `pro`):
- Composer ainda é ProseMirror (`#prompt-textarea.ProseMirror`); picker de esforço é a variante slider (`data-animated-slider-trigger`); existem `[data-streaming-response-status]`, `copy-turn-action-button`, `data-turn-id`, `data-message-id`, `input[data-testid="upload-photos-input"]`.
- `cloudflared 2026.5.2` instalado em `C:\Program Files (x86)\cloudflared\cloudflared.exe`.
- **Developer Mode está desligado** nesta conta (`GET /backend-api/aip/connectors/mcp/tunnels` → 403 "Developer mode is required"). É um toggle único: Settings → Apps → Advanced settings → Developer mode (setting `developer_mode` do usuário). Pré-requisito manual antes do modo conector.
- API de conectores (extraída do bundle do chatgpt.com; todas com header `OAI-Product-Sku: CONNECTOR_SETTING` e `Authorization: Bearer <accessToken de /api/auth/session>`):
  - criar: `POST /backend-api/aip/connectors/mcp` body `{name, description, mcp_url, auth_request}` (ou `tunnel_id` em vez de `mcp_url`) → `{connector}`; `auth_request` vem de `{supported_auth:[{type:"NONE"}], oauth_client_params, default_scopes, oidc_enabled, use_cimd}`.
  - link (sem auth): `POST /backend-api/aip/connectors/links/noauth` `{connector_id, name, action_names:[], link_params, action_param_schemas}`.
  - recarregar tools: `POST /backend-api/aip/connectors/mcp/refresh_actions` `{link_id}`.
  - listar: `POST /backend-api/aip/connectors/list_accessible?include_actions=false&external_logos=true&skip_directory=true` `{principals:[], purpose:<enum>}` (o valor exato de `purpose` deve ser capturado ao vivo com `browser_network` na primeira execução — chamada com purpose errado devolve 422).
  - apagar: `DELETE /backend-api/aip/connectors/{connector_id}`; `DELETE /backend-api/aip/connectors/links/{link_id}`.
  - checagem OAuth do modal: `POST /aip/connectors/mcp/oauth_config {mcp_url, custom_headers}` — nosso servidor deve responder 404 em `/.well-known/oauth-protected-resource` para ser tratado como "sem auth".
  - Não existe "editar URL": a troca de URL é **delete + create** com o mesmo nome (o ChatGPT cacheia o contrato de tools por identidade do conector, então recriar é até mais limpo).
  - **Toggle do Developer Mode (automatizado, decisão do usuário)**: `PATCH /backend-api/settings/account_user_setting?feature=developer_mode&value=true` (valor como string; header opcional `ChatGPT-Account-ID`). Leitura: `GET /backend-api/settings/user` (a chave `developer_mode` aparece no mapa de settings quando definida). Confirmar ao vivo com `browser_network` na primeira execução.

Decisões do usuário (2026-08-26):
- Endpoint = **daemon único compartilhado** (`codex chatgpt-web daemon`), auto-iniciado, dono do `tunnel-client` (ou cloudflared) + servidor MCP loopback + registro do conector; N sessões Codex em paralelo registram turnos por loopback.
- Túnel: **`tunnel = "openai"` (tunnel-client oficial) por padrão**; `"cloudflared"` como fallback.
- Modelos `chatgpt-web/*` **só para agentes** (`visibility: hide`), como os Claude.
- Escopo de tools: **`none` + `connector`** (sem modo `text`).
- Developer Mode: **automatizar** o toggle pela página.
- `chatgpt-pro-mcp` (Node) fica intocado, para o Claude Code.
- **Contas**: o provider usa **uma** conta ChatGPT web (a logada no Chrome) e é independente das contas do Codex local (`codex account …` — o provider nunca usa a auth do Codex, como o `claude_code` não usa). O Tunnel e a key restrita devem ser criados em `platform.openai.com` **logado com essa mesma conta web** (o modal de conector só lista tunnels da org da conta). Multi-conta web (`[[chatgpt_web.accounts]]` com tunnel/key/perfil do Chrome por conta, seleção por spawn e failover) fica fora do escopo; nota: o daemon chrome-mcp aceita uma única extensão por vez (`ws-hub.ts:136-144`), então exigiria um daemon por perfil do Chrome.

## Arquitetura resultante

```
Codex (thread/agente)  ──WireApi::ChatGptWeb──▶ core/src/chatgpt_web (provider)
   │ tools = none | connector                      │ driver Rust → chrome-mcp daemon (127.0.0.1:8848) → extensão → Chrome real (aba dedicada)
   │                                               │ 1 conversa ChatGPT persistente por thread; envia só itens novos; lê /backend-api/conversation
   │ function_call ⇄ function_call_output          ▼
   └──loopback──▶ `codex chatgpt-web daemon` (único, compartilhado)
                    ├─ servidor MCP público (rmcp Streamable HTTP, path secreto) ◀── cloudflared quick tunnel ◀── ChatGPT (conector "Codex Native")
                    ├─ registro do conector (delete+create via API da página, quando a URL muda)
                    └─ broker turn_token → sessão Codex dona do turno
```

Fatos do fork que moldam o desenho (verificados pelo agente de desenho):
- O turn loop exige `OutputItemAdded` antes de qualquer delta e fecha o item em `OutputItemDone` (`core/src/session/turn.rs:2731,2773,2791`) → reusar a disciplina do `StreamAssembler` do `claude_code` (`mod.rs:1645–1843`; mover para `claude_code/assembler.rs` e reexportar).
- `Completed{end_turn: Some(false)}` só liga `needs_follow_up` (`turn.rs:2694–2700`) → emitir **apenas** junto de itens `FunctionCall`; senão `Some(true)`.
- `OutputItemDone(FunctionCall{namespace: None})` é despachado ao ToolRouter (`turn.rs:2471, 2485–2500`) → é assim que o modo conector reaproveita sandbox/aprovações do Codex sem código novo de execução.
- Retry: `CodexErr::is_retryable` (`protocol/src/error.rs:364–400`): `Stream/Timeout/ConnectionFailed/Io` re-tentam; `UnsupportedOperation/UsageLimitReached/ContextWindowExceeded/Interrupted` não. `ContextWindowExceeded` → `set_total_tokens_full` → Codex compacta (`turn.rs:1427`, `compact.rs:330`).
- Compaction é um `stream()` normal com o prompt de sumarização como último item user (`compact.rs:116–131, 254–310`); `compact.rs:128` pula só `ClaudeCode` → **não** adicionar `ChatGptWeb` ao skip; o provider detecta o turno de compaction e o roda numa conversa descartável.
- Imagens só chegam em `Prompt::input` se `input_modalities` contém `image` (`core/src/context_manager/history.rs:209`) → chegam como `ContentItem::InputImage{image_url: "data:…"}`.
- Não há tokenizer no workspace → estimativa `chars/4 + 8192` de reserva.
- `core` já depende de `codex-rmcp-client`, `rmcp`, `base64`, `uuid`, `tempfile`, `tokio-util` → sem deps novas para M1–M5. rmcp 3.1.3 já manda `DELETE` com `mcp-session-id` no drop do transporte → `RmcpClient::shutdown()` basta.

## Layout de módulos (novo)

```
core/src/chatgpt_web/
  mod.rs            ChatGptWebWorkspace::from_config, ChatGptWebThreadState, stream(), run_turn(), TurnOutcome, TURN_SLOTS (Semaphore max_parallel_turns)
  driver/daemon.rs  DaemonClient sobre RmcpClient (connect lazy, call, eval_in com dupla decodificação, health, 1 reconexão, shutdown)
  driver/tabs.rs    TabPool: registro ~/.chatgpt-pro-mcp/tabs.json (mesmo formato do Node), TabLock, afinidade, sweeper, with_tab_for, with_activated_on, goto_on, wait_ready_on
  driver/page_scripts.rs  13 scripts verbatim (fn → String; interpolação só via serde_json::to_string; teste: nenhum `async`)
  driver/api.rs     serde de RawConversation + normalize() (walk current_node→parents, anyInProgress só após último user, assets, + api_tool_requests{message_id, has_result}) + ChatGptApi (get/patch/list/models, backoff 429 2/5/10s)
  driver/ops.rs     resolve_model, send() com fases navigate→model→precheck→upload→compose→attachments-wait→submit→confirm, confirm_submitted, attach_files, stop, set_level_via_menu
  history.rs        ConversationContinuity{conversation_id, model_slug, delivered_items, delivered_fingerprint, echoed, message_landed_unanswered}; plan_request (reusa claude_code::history::{render_item, fingerprint, item_fingerprint} via pub(crate))
  sessions.rs       CODEX_HOME/chatgpt_web_sessions.json (espelho de claude_code/sessions.rs; reusa claude_code::state_file)
  prompt.rs         RenderedTurn{text, attachments, is_replay}: header/contrato, <codex_transcript>, extensão, imagens, aviso commentary
  stream.rs         ReplyTracker (diff puro, testado com fixtures) + PollLoop (poll → ResponseEvents, watchdog por progresso, conclusão)
  connector.rs      seam: trait ConnectorBroker, ToolRequest, ConnectorTurn, LiveTurn (re-attach entre stream() calls)
  fixtures/*.json   capturas reais de /backend-api/conversation
core/src/chatgpt_web/connector/   (daemon compartilhado — ver seção "Modo conector")
```

Diffs em arquivos upstream (todos pequenos, todos `// FORK:`): `model-provider-info/src/lib.rs`, `core/src/client.rs`, `core/src/compact.rs` (só `set_chatgpt_web_workspace`), `core/src/session/{turn,session,handlers}.rs`, `core/src/tools/handlers/{multi_agents_common,multi_agents_v2}.rs`, `core/src/agent/{role,control}.rs`, `config/src/{config_toml.rs,thread_config/remote.rs}`, `core/src/config/mod.rs`, `models-manager/{src/local_models.rs,chatgpt_web_models.json}`, `thread-manager-sample/src/main.rs`, `core/config.schema.json` (gerado), 3 reexports em `core/src/claude_code/mod.rs`, `cli` (subcomando `chatgpt-web daemon`).

## Milestones

### M1 — Catálogo, provider, config e todos os `match` (compila; `stream()` devolve `UnsupportedOperation("not implemented")`)

| Arquivo | Mudança | Espelho |
|---|---|---|
| `model-provider-info/src/lib.rs` | `CHATGPT_WEB_PROVIDER_ID = "chatgpt_web"`, `WireApi::ChatGptWeb` com `#[serde(rename = "chatgpt_web")]`, arms em `Display` e no `Deserialize` manual (lista `&["responses","claude_code","chatgpt_web"]`), `create_chatgpt_web_provider()` (HTTP `None`, `requires_openai_auth:false`, `supports_websockets:false`), entrada em `built_in_model_providers()` | linhas 46, 63–105, 437–463, 572 |
| `models-manager/chatgpt_web_models.json` + `src/local_models.rs` | 5 linhas (tabela abaixo); `locally_served_models()` concatena os dois bundles; novo `provider_for_locally_served_model(slug) -> Option<&'static str>` (`claude-*`→`claude_code`, `chatgpt-web/*`→`chatgpt_web`); manter teste "todos Hide" | 14–39, 56–61 |
| `config/src/config_toml.rs` | `ChatGptWebToml{tools, idle_timeout_ms, max_parallel_turns, max_tabs, tab_idle_ms, daemon_url, token_file, base_url, poll_interval_ms, archive_on_shutdown, max_fork_turns, connector_name, cloudflared_path, …}`; `enum ChatGptWebTools{None(default), Connector}` snake_case; `ConfigToml.chatgpt_web` | 153–216 |
| `core/src/config/mod.rs` | `Config.chatgpt_web: ChatGptWebSettings` (um struct só, para o literal em `thread-manager-sample/src/main.rs:207` virar 1 linha); defaults na tabela de config; `idle_timeout_ms 0 → None` | 243–245, 900–922, 4195–4226 |
| `core/src/client.rs` | `ModelClientState.chatgpt_web: Arc<ChatGptWebThreadState>`; `chatgpt_web_workspace` em `ModelClient` e `ModelClientSession`; `with_/set_chatgpt_web_workspace`, `set_chatgpt_web_connector`; arm `WireApi::ChatGptWeb => chatgpt_web::stream(prompt, model_info, effort, workspace, state, thread_id)` | 225–305, 491–545, 1976–2010 |
| `core/src/session/session.rs:1426`, `turn.rs:170`, `compact.rs:279` | também chamar `with_/set_chatgpt_web_workspace` | idem claude |
| `core/src/session/turn.rs:2271` | `if wire_api == ChatGptWeb && tools == Connector { set_chatgpt_web_connector(...) }` (objeto vem do M6) | attach do host |
| `config/src/thread_config/remote.rs:305–313` | `WireApi::ChatGptWeb` no arm `unreachable!` | — |
| `core/src/tools/handlers/multi_agents_common.rs` | `task_fork_mode_for_wire_api`: `ClaudeCode \| ChatGptWeb` iguais (max_fork_turns do provider certo); `align_provider_with_locally_served_model` usa `provider_for_locally_served_model`; nota de service tier genérica | 49–73, 345–374, 440–459 |
| `multi_agents_v2.rs:92–105` | `require_readable_message_form`: `!matches!(wire_api, ClaudeCode \| ChatGptWeb)` | — |
| `core/src/agent/role.rs:94–99, 341–387` | filtro aceita os dois ids; nota específica ("roda no ChatGPT Web via browser; sem acesso local a menos que `tools = "connector"`; mande `plaintext_message` autocontido") | — |
| `core/src/lib.rs` | `mod chatgpt_web;` | — |

Linhas do catálogo (copiar todos os campos de uma linha Claude e mudar): `visibility: "hide"`, `input_modalities: ["text","image"]`, `supported_in_api: true`, `tool_mode: null`, `multi_agent_version: "v2"`, `supports_reasoning_summaries: true`, `service_tiers: []`, sem `comp_hash`, `priority` 40–44 (confirmar direção da ordenação em `manager.rs:127`), um único `supported_reasoning_levels`:

| slug | level | ctx / auto_compact | mapeamento ChatGPT |
|---|---|---|---|
| `chatgpt-web/instant` | low | 41000 / 32000 | `?model=<base>-instant` |
| `chatgpt-web/thinking` | medium | 90000 / 80000 | `?model=<base>-thinking` |
| `chatgpt-web/high` | high | 90000 / 80000 | `-thinking` + menu `^Alto$\|^High$` (aba visível) |
| `chatgpt-web/extra-high` | xhigh | 90000 / 80000 | `-thinking` + menu `Extra alto\|Extra high` |
| `chatgpt-web/pro` | max | 111193 / 95000 | `?model=<base>-pro` |

Teste unitário: cada linha com `auto_compact_token_limit <= 0.9 * context_window` (core clampa, `openai_models.rs:434–438`). Rodar: `just fmt`, `just fix -p codex-core`, `just write-config-schema`, `just test -p codex-core -p codex-model-provider-info -p codex-models-manager`.

### M2 — Driver: daemon, page scripts, API, pool de abas (sem wiring; testes live `#[ignore]`)

- `DaemonClient::connect`: `RmcpClient::new_streamable_http_client("chatgpt_web", url, Some(token), None, None, store, keyring, http_client, None)` + `initialize(…, 30s)`; `http_client = RouteAwareHttpClient::new(config.http_client_factory()).with_tls_backend_fallback()` (`codex-mcp/src/runtime.rs:630–633`). **Checar**: sem header `Origin` no `rmcp-client/src/http_client_adapter.rs` (daemon devolve 403 se houver). `call()`: timeout `max(120s, timeout_ms+30s)`, `isError`→`DriverError::Tool`, imagem→base64, 1 reconexão (regex de `daemon.ts:107–109`). `eval_in`: `browser_eval{tabId, expression, world:"MAIN", timeoutMs}` + **dupla decodificação**. `health()`: `GET /healthz` 3s sem auth. Token: `CHROME_MCP_TOKEN` ou `~/.chrome-mcp/token.txt` (trim).
- `page_scripts.rs`: `pub(super) fn wait_ready(timeout_ms) -> String` etc., corpo em raw string, placeholders via `serde_json::to_string`; teste: começa com `() =>` e não contém `async`.
- `api.rs`: tipos `#[serde(default)]` (`RawConversation`, `RawMessage{author.role, content{content_type, parts, text, thoughts[]}, status, end_turn, recipient, metadata{parent_id, attachments, model_slug…}}`); `normalize()` = porte de `api.ts:182–280` + `api_tool_requests`.
- `tabs.rs`: porte de `tab.ts` (registro/lock `mkdir tabs.json.lock` steal >10s deadline 5s; `pid_alive` — Windows via `windows-sys OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`, confirmar dep direta em `core`); `PoolTab{id, lock, pending, bound_conversation, last_used}`; `with_tab_for`, `bind`, `eval_tab_id`, `goto_on` (`browser_navigate{waitUntil:"load", timeoutMs:30000}`), `show_conversation_on`, `wait_ready_on` (retry em `execution context|No frame|detached|target closed`, cap `page_wait+60s`), `with_activated_on` (mutex de foco; ativar → f → `reload` → restaurar), sweeper idle, `release_sync` em `Drop`/`Op::Shutdown`. Mesmo formato de arquivo ⇒ o Node `chatgpt-pro-mcp` concorrente interopera.
- Testes: registro (temp dir, pid morto simulado); `normalize` sobre fixtures (`conv_in_progress`, `conv_finished`, `conv_thoughts`, `conv_image_assets`, `conv_api_tool`, `conv_stopped_old_in_progress`); `parse_result`; `#[ignore] live_daemon_health`.

### M3 — Ops: envio, seleção de modelo, stop, uploads

Porte de `ops.ts`: `resolve_model` (base = `default_model_slug` sem `-(instant|thinking|pro|mini|t-mini)$`); `send(SendRequest{conversation_id, text, model, files}) -> Sent{conversation_id, phase_reached, notes}` com a máquina de fases e `confirm_submitted` no erro de submit (`ops.ts:228–254`); constantes de timeout; `attach_files` (`input[data-testid="upload-photos-input"]` para imagens, depois `form input[type="file"]:not([accept*="image"])`, `form input[type="file"]`, `input[type="file"]`), `verify_tiles_attached`, `wait_attachments_ready`, `split_already_uploaded`; `stop` (click_stop até 8s); todo erro carrega `FailurePhase` + `message_landed: bool`. Live `#[ignore]`: `new_chat_instant_reply`, `continue_conversation`, `upload_and_reply`, `stop`, `pro_resolves`.

### M4 — Núcleo: history, sessions, prompt, poll→eventos, `stream()` em `tools = "none"`

- `history::plan_request(input, &continuity, model_slug, compact_prompt) -> RequestPlan{turn, restart, delivered_items, delivered_fingerprint, is_compaction}`: cópia de `claude_code/history.rs:64–96`; `can_extend` exige também `continuity.model_slug == model_slug`; `is_compaction` = último user == `compact_prompt` (então `restart=true`, continuidade intocada, conversa descartável).
- `sessions.rs`: `chatgpt_web_sessions.json`, TTL 7d, 512 entradas, `state_file::{read,update}`.
- `stream.rs::ReplyTracker::observe(conv, mode) -> Vec<Delta>` (puro): (1) sem âncora (último user turn ≠ o enviado) → nada; (2) turnos após a âncora: `assistant-thoughts` → reasoning, `assistant` text `recipient=all` → texto, `tool` com assets → nota; (3) por message id: `Open*` + delta do sufixo se `starts_with` do já emitido, senão `Rewrite`; (4) `idle = !is_generating && async_status ∈ {None,0}`; `done_end_turn = idle && newest_text.end_turn==true && status=="finished_successfully"` (newest = último assistant `text` do fim; fail-closed); `done_stable = idle && assets && fingerprint estável 2 polls`; em `Connector` também `api_tool_requests.all(has_result)`; (5) `Progress` se algo mudou.
- `PollLoop::run`: `select!{ consumer_dropped → Interrupted; connector_rx → AwaitingToolOutputs; sleep(poll 2.5s); sleep(last_progress+idle_timeout) → stop + Stalled }`; 404 tolerado 30s após envio; deltas → `Assembler` (o `StreamAssembler` do claude, movido para `claude_code/assembler.rs` e reexportado): `OpenReasoning`→`OutputItemAdded(reasoning)`+`ReasoningSummaryPartAdded{0}`; `Reasoning(Δ)`→`ReasoningSummaryDelta{Δ,0}`; `OpenText`→fecha commentary + `OutputItemAdded(message commentary)`; `Text(Δ)`→`OutputTextDelta`; `Rewrite`→fecha e reabre com texto completo; `Done`→`close(FinalAnswer)` + `Completed{response_id: conv_id, token_usage, end_turn: Some(true)}`.
- `mod.rs::run_turn`: `Created` → permit do `TURN_SLOTS` → `plan_request` → `prompt::render` → (modo none) item commentary de aviso (fingerprint em `echoed`) → `driver.health()` → `ops.send` (conversation_id = None se restart; modelo resolvido só em restart) → grava continuidade com `message_landed_unanswered=true` **antes** do poll → `PollLoop` → outcome → eventos/erros; em `Completed` grava `echoed = assembler.take_authored()`; compaction: não grava, arquiva a conversa descartável.
- Uso: `input_tokens = ceil(chars(render_full(input))/4) + 8192`, `output = ceil(reply_chars/4)` — renderizar o histórico **inteiro** para o medidor do Codex refletir o tamanho da conversa e disparar compaction em `auto_compact_token_limit`.
- Watchdog: `idle_timeout_ms` default 1_200_000, reset só em `Progress`; nada é emitido em silêncio (o idle timer do Responses não é usado neste arm).
- Interrupt: `driver.stop(conv)` best-effort; `message_landed_unanswered=true`.
- Testes: `plan_request` (extend/restart/model mismatch/compaction), `observe` sobre fixtures (crescimento, thoughts→text, rewrite, api_tool pendente, assets estáveis, `in_progress` velho não bloqueia), ordem de eventos, uso, mapeamento de erros, sessions round-trip.

### M5 — Imagens, arquivamento, compaction

- Imagens: `ContentItem::InputImage{image_url: data:…}` → decodificar → `CODEX_HOME/chatgpt_web/attachments/codex-img-<sha256[..12]>.<ext>` (nome determinístico → dedupe por nome+tamanho) → `ops.send(files)`; placeholder `[image_attachment: …]` no transcript; cap 10/mensagem; limpeza >24h no start.
- Arquivar (`PATCH {is_archived:true}`, 10s) no shutdown do root (`core/src/session/handlers.rs:430`) e no close de agente (não na **eviction** — localizar em `core/src/agent/control.rs`); gate `archive_on_shutdown`. **Checar ao vivo** se dá para enviar numa conversa arquivada (senão `PATCH {is_archived:false}` antes).
- Compaction: teste de que o prompt de sumarização produz `restart=true`, continuidade intocada, e o turno seguinte (prefixo mudado por `replace_compacted_history`) replay em conversa nova.

### M6 — Seam do conector no provider (interface + re-attach)

```rust
pub(crate) trait ConnectorBroker: Send + Sync + Debug {
    fn begin_turn<'a>(&'a self, thread_id: ThreadId, conversation_id: Option<&'a str>, tools: &'a [ToolSpec]) -> BoxFuture<'a, Result<ConnectorTurn, String>>;
    fn prompt_contract(&self, turn: &ConnectorTurn) -> Vec<String>;   // nome do conector, @mention, contrato do turn_token
    fn end_turn<'a>(&'a self, turn_token: &'a str) -> BoxFuture<'a, ()>;
}
pub(crate) struct ConnectorTurn { pub turn_token: String, pub requests: mpsc::Receiver<ToolRequest> }
pub(crate) struct ToolRequest { pub call_id: String, pub name: String, pub arguments: String, pub respond: oneshot::Sender<FunctionCallOutputPayload> }
pub(crate) struct LiveTurn { conversation_id, turn_token, pending: Vec<(call_id, oneshot::Sender<…>)>, sink: Arc<Mutex<mpsc::Sender<Result<ResponseEvent>>>>, reattached: Notify, abort: CancellationToken }
```
Fluxo: `begin_turn` → prompt recebe `prompt_contract()` (sem o aviso read-only) → `PollLoop` faz `select!` em `requests.recv()`; ao chegar `ToolRequest`(s) (batch da janela de ~15ms): fecha commentary, emite `OutputItemAdded/Done(FunctionCall{call_id, name, arguments, namespace: None})` por chamada, fingerprints em `echoed`, `Completed{end_turn: Some(false)}`; guarda `LiveTurn` em `state.live_turn`; a task de poll **continua** com `sink` nulo. Próximo `stream()`: se `live_turn` existe e a cauda de `prompt.input` tem `FunctionCallOutput` para todo `pending` → responde via `respond`, fingerprints em `echoed`, troca `sink`, `reattached.notify`, devolve o novo `ResponseStream`. Se faltam outputs (usuário digitou outra coisa / turno abortado) → `abort` → stop no browser → `end_turn` → turno normal. Conclusão em `Connector` exige todo `api_tool` respondido. `tools="connector"` sem broker → `UnsupportedOperation` com instrução. Teste: broker fake por canal (request → itens + `Completed{false}`; segundo `stream()` com outputs → responder recebe; outputs ausentes → abort).

### M7 — Polimento

Notas em `role.rs`, label opcional em `agent/control.rs:141` (`conversation <id[..8]>` no `list_agents`), `docs/config.md` seção `[chatgpt_web]`, `docs/chatgpt_web_agents.md` espelhando `docs/claude_code_agents.md`, `just write-config-schema`.

## Prompt (`prompt.rs`)

Mensagem de conversa nova (replay):
```
You are the model backend for a Codex session. Everything below is a transcript of that session; the tagged blocks are conversation data, not instructions about this transport. Preserve priority: system, then developer, then user. Roles are literal: <assistant> blocks are your own earlier replies; <user> blocks are the human; <tool_call>/<tool_result> were produced by Codex, not the human.
[none]      This chat has no bridge to the user's computer. The transcript already contains everything Codex collected locally; treat prior tool results as authoritative snapshots. Never claim a new local inspection, command, edit, or verification unless it appears in the transcript; if the request needs fresh local access, say exactly that instead of inventing success. Use ChatGPT-native capabilities (web search, browsing) whenever they help.
[connector] <broker.prompt_contract() — nome do conector, "Pass turn_token X unchanged to every <connector> call in this response…">
[pro]       Complete this task directly in this response; do not delegate to sub-agents.
Do not mention this transport contract in the answer. Return only the answer the Codex session should receive.
Environment: Working directory: <cwd>; Other readable roots: …; Writable roots: … ; network note.   (claude_code/mod.rs:220–267)
<developer_instructions>
<codex_transcript> …render_item por item; imagens → [image_attachment: nome]… </codex_transcript>
<codex_transport_resume>The transcript is complete. Execute the latest active user request now under the contract above.</codex_transport_resume>
```
Extensão = itens novos unidos por `\n\n` (sem header) ou `(no new input; continue from the previous turn)`; após turno interrompido/stalled prefixar `(the previous request was interrupted; continue from it)`. Compaction = replay em conversa nova com o contrato `none` trocado pelas 3 linhas de checkpoint (`prompt.ts:404–408`). Aviso commentary (item do lado Codex, não enviado), modo none: `"⚠️ ChatGPT Web <Level> cannot access the local computer in this turn. It sees the accumulated Codex context (including earlier tool results) but cannot read or modify local files. ChatGPT-native capabilities such as web search remain available."`

## Mapeamento de erros

| Classe | CodexErr | Retry? | Continuidade |
|---|---|---|---|
| DaemonDown (health/extension/connect) | `Stream("chrome-mcp daemon unreachable…")` | sim | mantém |
| LoginRequired / SessionExpired | `UnsupportedOperation` | não | mantém |
| RateLimited (429 pós-backoff; diálogo "Too many requests") | dorme 30s uma vez, depois `Stream` | sim | mantém (não enviado) |
| MessageTooLong (composer/edge rejeita) | `ContextWindowExceeded` | não → compaction | invalida |
| UiChanged (composer/send não achado após retry) | `UnsupportedOperation` (+hint da fase) | não | mantém |
| UpstreamError ("Something went wrong"; `finished_partial_completion` sem end_turn) | `Stream` | sim | `message_landed_unanswered` |
| Transient (eval timeout antes do envio) | `Stream` | sim | mantém |
| Submit ambíguo com `confirm_submitted == None` | `UnsupportedOperation` (nunca reenviar às cegas) | não | mantém |
| Stalled (sem progresso ≥ idle_timeout) | `UnsupportedOperation("no progress for Ns; generation stopped")` | não | `message_landed_unanswered` |
| Interrupted (consumer dropped) | — | — | `message_landed_unanswered` |
| Conversa 404 > 30s após envio | `Stream` | sim | invalida |

## Config `[chatgpt_web]`

| chave | default |
|---|---|
| `tools` | `"none"` (`"connector"` exige o daemon) |
| `idle_timeout_ms` | `1200000` (0 = infinito; medido por progresso) |
| `max_parallel_turns` | `2` |
| `max_tabs` / `tab_idle_ms` | `3` (clamp 1..8) / `300000` |
| `daemon_url` / `token_file` / `base_url` | `http://127.0.0.1:8848/mcp` / `~/.chrome-mcp/token.txt` / `https://chatgpt.com` (envs `CHROME_MCP_URL`, `CHROME_MCP_TOKEN`, `CHATGPT_URL` honradas) |
| `poll_interval_ms` | `2500` |
| `archive_on_shutdown` | `true` |
| `max_fork_turns` | `0` |
| `connector_name`, `cloudflared_path`, `connector_auto_approve_ui`, … | ver "Modo conector" |

## Modo conector (daemon compartilhado)

Achados que ajustam o brief (verificados nas fontes):
- Os dois projetos de referência usam primariamente o **`tunnel-client` oficial da OpenAI** (github.com/openai/tunnel-client, Apache-2.0; Secure MCP Tunnel: `tunnel_id` estável, só conexões de saída por long-poll HTTPS `GET /v1/tunnels/{id}/poll` + `POST …/response`, sem URL pública, sem ciclo delete/recreate; é a isso que `tunnel_id` no body de criação do conector se refere). **Adotado como padrão.** Setup único: Tunnel em `platform.openai.com/settings/organization/tunnels` + key restrita `Tunnels: Read + Use` em `platform.openai.com/settings/organization/api-keys` ("creating the key is free and does not consume model API credits" — codex-chatgpt-web README:148–150). Modos de uso: `tunnel-client run --control-plane.tunnel-id <id> --health.listen-addr 127.0.0.1:0 --health.url-file <tmp> --log.format json --log.level info` com env `CONTROL_PLANE_API_KEY` e `MCP_SERVER_URL=url=http://127.0.0.1:<port>/mcp/<secret>,channel=main` (chat-on-steroids `tunnel/index.ts:227–300, 430–470` — credenciais e path só no ambiente, nunca em argv; `MCP_DISCOVERY_EXTRA_HEADERS` opcional), ou `runtimes connect --alias … --tunnel-id … --runtime-api-key file:<path> --mcp-command "<stdio>"` (codex-chatgpt-web `tunnel.ts:226–251`, supervisão gerenciada pelo próprio tunnel-client). Escolha: **`run` + `MCP_SERVER_URL`** (nosso servidor é HTTP in-process; o daemon já supervisiona). Binário: release pinada com verificação SHA-256 (codex-chatgpt-web pina `v0.0.12`, ~100 MB cap; chat-on-steroids também pina por versão+sha) ou `tunnel_client_path` explícito; `brew install openai/tools/tunnel-client` no mac. Flags de health variam por versão (`--health.bind-addr` no README atual vs `--health.listen-addr`/`--health.url-file` no chat-on-steroids) → o implementador confirma com `tunnel-client run --help` da versão pinada. Health local do tunnel-client: `/healthz`, `/readyz`, `/metrics`, `/ui`; prova de conexão = poll do control plane concluído (não uma linha de log). Falha de auth (key/tunnel id errados) é terminal: não re-tentar em loop. Cloudflared fica como `TunnelAdapter` alternativo.
- A sondagem RFC 9728 deve receber **JSON**, não 404 texto: chat-on-steroids serve `GET /.well-known/oauth-protected-resource/<path secreto>` como 200 `{resource, resource_name, authorization_servers: [], scopes_supported: []}` e todo o resto como JSON de erro (o cliente decodifica JSON independente do status; texto deixou a descoberta OAuth travada).
- O cliente MCP do ChatGPT manda `x-request-id: wfr_<id>/<sufixo>`, faz `GET` (stream SSE) e `DELETE` a cada conexão (405 é normal); não há header identificando a conta.
- rmcp 3.1.3 `StreamableHttpServerConfig` valida `Host` por padrão (`allowed_hosts = [localhost, 127.0.0.1, ::1]`) → rodar cloudflared com `--http-host-header 127.0.0.1:<port>` (o que o chat-on-steroids faz). Knobs: `legacy_session_mode` (default true), `json_response`, `max_request_body_bytes`, `sse_keep_alive` (15s), `cancellation_token`; headers HTTP acessíveis em `call_tool` via `context.extensions.get::<http::request::Parts>()`.
- Roteamento no fork (`core/src/tools/router.rs:148–200`): `FunctionCall{name, namespace}` → `ToolName::new(namespace, name).with_default_namespace()`; `CustomToolCall` → `ToolPayload::Custom`. `apply_patch` só é registrado se `model_info.apply_patch_tool_type.is_some()` (`spec_plan.rs:1146`) e o único tipo é `Freeform` → **o catálogo `chatgpt_web` deve ter `apply_patch_tool_type: "freeform"`** e o broker emite `CustomToolCall`. Unified exec = `exec_command`/`write_stdin` (`handlers/shell_spec.rs:92,142`), legado `shell` (63); `view_image` com `Feature::ViewImage`.
- Kill de árvore de processos no Windows: precedente melhor é `codex_utils_pty::JobObject::create_without_breakaway()` + `prepare_suspended_spawn` + `terminate()` (`rmcp-client/src/stdio_server_launcher.rs:297–308, 440–443`); `taskkill /T /F` (`claude_code/mod.rs:916–935`) como fallback.
- Single-instance: `app-server-daemon/src/lib.rs:713–749` usa `flock` no unix mas `try_lock_file` devolve `Ok(true)` no Windows → o daemon precisa de lock real no Windows (open exclusivo com `share_mode(0)`).
- `browser_network` do chrome-mcp não grava corpos/headers de request → as capturas de payload usam um tap em `window.fetch` via `browser_eval`.
- A afirmação "Pro é read-only para MCP custom" não foi encontrada no README do chat-on-steroids; tratar como não verificada (spike S5).

### Arquitetura

```
Codex sessão A ──┐ loopback HTTP/JSON + bearer (CODEX_HOME/chatgpt_web/daemon.token)
Codex sessão B ──┴──▶ codex chatgpt-web daemon (instância única)
                        ├─ control API 127.0.0.1:<p1>  (sessões, turnos, long-poll de chamadas, resultados)
                        ├─ TurnBroker  turn_token → sessão; claim/binding; batch 15 ms; retire LRU 256
                        ├─ MCP loopback 127.0.0.1:<p2>/mcp/<secret> ◀── tunnel-client (saída HTTPS → OpenAI) ◀── cliente MCP do ChatGPT
                        │                                            (fallback: cloudflared quick tunnel)
                        ├─ TunnelSupervisor (tunnel-client run; readyz local; restart c/ backoff; auth error = terminal)
                        └─ ConnectorRegistry → chrome-mcp daemon → aba chatgpt.com (apiCall) — cria o conector (tunnel_id) uma vez + Developer Mode
```
Invariantes: quem executa é sempre a sessão Codex dona do `turn_token`; o contrato é fixo (6 tools) e idêntico para todas as sessões; o daemon nunca vê filesystem nem loga segredos; com `tunnel = "openai"` nada fica exposto na internet (o servidor MCP só escuta em loopback e só o tunnel-client fala com ele).

**Setup único (`codex chatgpt-web setup`)**: pede/aceita `--tunnel-id tunnel_<32hex>` e `--api-key-file <path>` (ou lê de stdin), valida o formato, grava `CODEX_HOME/chatgpt_web/tunnel.key` (0600) e `tunnel_id` na config, baixa/verifica o `tunnel-client` pinado (ou usa `tunnel_client_path`), roda `tunnel-client doctor`-equivalente (start + `/readyz`), liga o Developer Mode se preciso, cria o conector e confere as 6 actions. Sem tunnel id/key configurados e `tunnel = "openai"` → erro acionável apontando para `codex chatgpt-web setup` (ou `tunnel = "cloudflared"`).

**Executor do registro = o daemon fala com o chrome-mcp diretamente** (não depende de uma sessão viva; `apiCall` só precisa de uma aba `chatgpt.com` qualquer). Procedimento: `browser_tabs list` → aba `chatgpt.com` existente (uso só de `browser_eval`) ou `create dedicated:true`, registrada em `tabs.json` com o pid do daemon, fechada após reconciliar. Fallback (chrome-mcp inacessível): `registry_status = "browser_unavailable"` e a primeira sessão a registrar turno executa o mesmo plano puro na própria aba.

### Ciclo de vida do daemon (`codex chatgpt-web daemon`)
- CLI: `Subcommand::ChatgptWeb` em `cli/src/main.rs` (padrão `AppServerSubcommand::Daemon`, linhas 1306–1340): `daemon [--foreground] [--idle-shutdown-ms]`, `status`, `stop`, `registry reconcile|show|delete`, `doctor`. Entrada `codex_core::chatgpt_web::connector::daemon::run(config)`.
- Estado em `CODEX_HOME/chatgpt_web/`: `daemon.lock` (unix `flock`; Windows open exclusivo `share_mode(0)`), `daemon.json` (`{version, pid, control_port, started_at_ms, codex_version, public_url (host sem o segredo), registry_status}` via `state_file`), `daemon.token` (0600, 32 bytes base64url), `connector.json` (`{connector_id, link_id, mcp_url, name, contract_version, verified_at_ms, actions}`, 0600), `daemon.log` (rotação 5 MB; só hashes de tokens).
- Autostart (sessão, `connector/client.rs::ensure_daemon`): lê `daemon.json`; pid vivo + `GET /healthz` OK → usa; senão spawna `current_exe() chatgpt-web daemon` desanexado (Windows `DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP|CREATE_NO_WINDOW`; unix `process_group(0)`), espera ≤15s. Corrida entre sessões: o perdedor sai no lock. Versão diferente → `POST /v1/admin/shutdown_when_idle` e respawn.
- Idle shutdown opcional (`daemon_idle_shutdown_ms`, default 0). Start preguiçoso: só na primeira sessão com `tools = "connector"` que chega a um turno.

### Servidor MCP do conector (`connector/daemon/mcp_server.rs`) — loopback; "público" só via cloudflared
- Bind `127.0.0.1:{tunnel_port}` (0 = efêmero). Path secreto `/mcp/<base64url(32 bytes)>` regenerado a cada start; comparação em tempo constante. Com `openai` o `tunnel-client` faz requests a `http://127.0.0.1:<port>/…` (Host = loopback → `allowed_hosts` default do rmcp já serve; sem `--http-host-header`).
- axum 0.8: `route_service("/mcp/{secret}", StreamableHttpService)` atrás de middleware que rejeita segredo errado com JSON 404; `GET /mcp/{secret}/healthz`; `GET /.well-known/oauth-protected-resource/mcp/{secret}` → 200 JSON com `authorization_servers: []`, `cache-control: no-store`; resto → JSON 404.
- `StreamableHttpServerConfig{ legacy_session_mode: false, json_response: false, sse_keep_alive: 15s, max_request_body_bytes: 8 MiB, allowed_hosts: ["127.0.0.1", "127.0.0.1:<port>", "localhost"] }` + cloudflared `--http-host-header`. SSE keep-alive evita o 524 do Cloudflare (~100s sem bytes) durante chamadas bloqueadas. **S1 confirma** stateless+SSE.
- Auth: **NONE + path secreto** (fase 1; sobrevive à sondagem `oauth_config` com `supported_auth:[{type:"NONE"}]`); link `api_key` como endurecimento opcional (fase 2, após S1b capturar em que header o ChatGPT manda a chave).
- Contrato fixo (todas com `turn_token: string(20..256)` obrigatório; descrições ≤120 chars):

| tool | args | mapeia para |
|---|---|---|
| `codex_exec` | `cmd, workdir?, yield_time_ms?(250..30000), max_output_tokens?, tty?` | `exec_command` (ou `shell` se for o que o turno anuncia) |
| `codex_write_stdin` | `session_id, chars?, yield_time_ms?, max_output_tokens?` | `write_stdin` |
| `codex_apply_patch` | `patch` | `apply_patch` (`CustomToolCall`) |
| `codex_view_image` | `path, detail?` | `view_image` |
| `codex_tool_inventory` | `query?, offset?, limit?(≤50), include_schema?` | servido do snapshot `Prompt::tools` da sessão (sem chamada ao Codex) |
| `codex_tool_call` | `namespace?, name, arguments?\|input?` | qualquer tool anunciada (inclui MCP, com namespace) |

Por que não expor as specs reais 1:1 + `refresh_actions`: o ChatGPT cacheia o conjunto por identidade do conector; o contrato precisa ser idêntico entre sessões/agentes; turnos diferentes anunciam tools diferentes. Versionamento: `connector_name` + `CONTRACT_VERSION` em `connector.json`; mudança de contrato ⇒ nome novo (`Codex Native` → `Codex Native 2`), conector velho apagado.

```rust
#[derive(Clone)] struct PublicMcpHandler { broker: Arc<TurnBroker>, limiter: Arc<RateLimiter> }
impl ServerHandler for PublicMcpHandler {
    async fn list_tools(..) -> ListToolsResult { contract::tools() }
    async fn call_tool(&self, req, ctx) -> Result<CallToolResult, McpError> {
        let parts = ctx.extensions.get::<http::request::Parts>();  // x-request-id, cf-connecting-ip (log/limite)
        self.limiter.check(parts)?;
        let args = contract::parse(&req)?;                          // valida turn_token
        let claim = self.broker.claim(&args.turn_token).await?;     // Unknown/Retired/Expired/SessionGone → McpError com texto exato
        match contract::to_call(&req.name, args, &claim.tools)? {
            Resolved::Local(r) => Ok(r),                            // inventory
            Resolved::Forward(call) => self.broker.invoke(claim.binding, call, deadline).await.map(Into::into),
        }
    }
}
```

### Supervisor de túnel (`connector/daemon/tunnel.rs`)
`trait TunnelAdapter { async fn start(&self, local_mcp_url: Url) -> Result<TunnelHandle> }` com dois adapters; o daemon publica `watch::Sender<TunnelState>` (`Connecting | Ready{endpoint: TunnelEndpoint} | Down{reason} | Fatal{reason}`), onde `TunnelEndpoint = OpenAi{tunnel_id} | Public{mcp_url}` — é isso que o registro usa para montar o body do conector.
- **`openai` (padrão)**: spawn `tunnel-client run --control-plane.tunnel-id <id> --health.listen-addr 127.0.0.1:0 --health.url-file <tmpdir>/health.url --log.format json --log.level info` (flags confirmadas com `--help` da versão pinada), env limpo + `CONTROL_PLANE_API_KEY=<tunnel.key>` + `MCP_SERVER_URL=url=http://127.0.0.1:<p2>/mcp/<secret>,channel=main` (+ `MCP_STARTUP_WAIT_TIMEOUT`), `windowsHide`; Windows `JobObject`, unix `process_group(0)`. Prontidão: ler `health.url` → `GET /readyz` até 200 (≤120s, `TUNNEL_READY_TIMEOUT_MS` do codex-chatgpt-web); depois `/healthz`/`/metrics` a cada 30s; "conectado" = último poll do control plane concluído (chat-on-steroids `lastHandshake`), não uma linha de log. Linhas JSON de log: padrão de falha de auth (`AUTH_FAILURE` do chat-on-steroids: key/tunnel id inválidos) → `Fatal` sem retry; "control plane unreachable" → `Down` com backoff `min(60s, 2s·2^n)` e distinção de máquina offline (rechecar 5s, sem thrash); saída do filho → relançar com backoff. O `tunnel_id` é estável → `Ready` não muda o conector.
- **`cloudflared` (fallback)**: `cloudflared tunnel --no-autoupdate --url http://127.0.0.1:<p2> --http-host-header 127.0.0.1:<p2> [extra_args]`; parse `https://[a-z0-9-]+\.trycloudflare\.com` (45s); prontidão = healthz pela URL pública (≤60s); URL nova ⇒ `Ready{Public{mcp_url}}` ⇒ o registro recria o conector.
- Binário `tunnel-client`: `tunnel_client_path` explícito → `CODEX_HOME/chatgpt_web/bin/tunnel-client-v<pin>` baixado da release pinada (`tunnel-client-v<ver>-<os>-<arch>.zip`, SHA-256 do `SHA256SUMS.txt` conferido, manifesto `{version, asset, archiveSha256, binarySha256}`) → PATH/homebrew. Shutdown: cancel token → terminate (Job/grupo) → 3s → `taskkill /T /F`.

### Registro do conector (`connector/daemon/registry.rs` + `page_scripts.rs`)
Planner puro + executor:
```rust
enum RegistryStatus { Unknown, DeveloperModeOff, BrowserUnavailable, Reconciling, Verified{connector_id, link_id, mcp_url}, Failed{reason, retry_at} }
enum RegistryOp { EnableDeveloperMode, ListByName, DeleteLink(String), DeleteConnector(String), Create(DesiredConnector), Link{connector_id}, RefreshActions{link_id}, VerifyActions{connector_id, expect: Vec<String>}, Persist(ConnectorRecord) }
fn plan(observed: &Observed, desired: &DesiredConnector) -> Vec<RegistryOp>;
trait ConnectorApi { async fn call(&self, op: &RegistryOp) -> Result<ApiResult, ApiError>; }  // impl: ChromeMcpPageApi; teste: FakeApi
```
Reconcile (no start, quando `TunnelState` fica `Ready` com endpoint diferente do persistido, e em `register_turn` se ≠ Verified; serializado; backoff 429 2/5/10s): (1) `list_accessible` por nome (e nomes de contratos anteriores → apagar); (2) registro persistido com o mesmo endpoint (`tunnel_id` ou `mcp_url`) e conector ainda existente → só `VerifyActions`; (3) senão apagar link(s)+conector(es), criar, linkar, `refresh_actions`, verificar `GET /aip/connectors/<id>/actions` == 6 nomes, persistir; (4) 403 "Developer mode is required" → `EnableDeveloperMode` (`PATCH /backend-api/settings/account_user_setting?feature=developer_mode&value=true`) uma vez e retry; persistindo → `DeveloperModeOff` com mensagem acionável (Settings → Apps → Advanced settings → Developer mode; `codex chatgpt-web registry reconcile`). Com `openai` o passo (3) acontece **uma vez** (e de novo só se o contrato mudar de versão); `GET /backend-api/aip/connectors/mcp/tunnels` (Developer Mode) lista os tunnels visíveis à conta → usar para validar que o `tunnel_id` configurado existe na mesma conta antes de criar.

Payloads (⚠ = capturar ao vivo com o tap de fetch):
```http
POST /backend-api/aip/connectors/mcp   (Authorization: Bearer <accessToken>, OAI-Product-Sku: CONNECTOR_SETTING)
{"name":"Codex Native","description":"Codex tools on this machine (exec, patch, images, harness tools).",
 "tunnel_id":"tunnel_<32hex>",                       ← openai (o modal normaliza `tunnel_id` vs `mcp_url`: `DW=({connectionType,mcpUrl,tunnelId}) => e==='tunnel' ? {tunnel_id} : {mcp_url}`)
 // ou "mcp_url":"https://<rand>.trycloudflare.com/mcp/<secret>"   ← cloudflared
 "auth_request":{"supported_auth":[{"type":"NONE"}],"oauth_client_params":null,"default_scopes":[],"oidc_enabled":false,"use_cimd":false}}  ⚠ chaves opcionais
POST /backend-api/aip/connectors/links/noauth  {"connector_id":"<id>","name":"Codex Native","action_names":[],"link_params":{},"action_param_schemas":{}}  ⚠ shapes
POST /backend-api/aip/connectors/mcp/refresh_actions {"link_id":"<link>"}
GET  /backend-api/aip/connectors/<id>/actions
POST /backend-api/aip/connectors/list_accessible?include_actions=false&external_logos=true&skip_directory=true {"principals":[],"purpose":"<⚠ capturado>"}
DELETE /backend-api/aip/connectors/links/<link_id>;  DELETE /backend-api/aip/connectors/<connector_id>
```
Page script `apiCallWithHeaders` = `apiCall` de `page-scripts.ts:245–278` + headers extras (promise chain; resolve string JSON). **Procedimento de captura (uma vez, pelo implementador)**: instalar tap em `window.fetch` via `browser_eval` (grava `{url, method, headers, body}` para `/backend-api/(aip|settings)`), com a aba ativada: (1) toggle Developer Mode nas Settings → capturar a mutação; (2) abrir Apps/Connectors e o menu `@` → capturar `purpose`; (3) criar conector descartável pela UI apontando para o servidor do S1 → capturar `create`, `noauth`, `refresh_actions`, `oauth_config` e headers extras (`OAI-Device-Id`, `OAI-Language`, `OAI-Client-Version`); (4) anexar com @mention e enviar "reply OK" → capturar o body de `/backend-api/f/conversation` (como o conector selecionado é codificado); (5) recarregar. Resultados em `connector/api_shapes.md`.

### Anexar o conector ao turno (lado sessão, `chatgpt_web/connector_attach.rs`)
Script de @mention idempotente (espelha `browser-worker.ts:1597–1700`): se já há exatamente 1 pill `[data-id^="plugin:"][data-keyword="<nome>"]` → ok; senão `execCommand('insertText', '@Codex')`, esperar linha `.__menu-item[tabindex="0"]` com título exato (≤4s), `ArrowDown` sintético até `data-highlighted` (assíncrono, React commit), `Enter`, esperar pill (≤5s); depois inserir o prompt após a pill (caret no fim + paste, sem select-all) e `clickSend`. Fallbacks: (1) `menu row not found` + `visibility === 'hidden'` → ativar aba → mention → prompt → send → restaurar foco, **sem** reload (reload derruba a pill); (2) menu "+" → Developer mode → toggle do conector; (3) se a captura mostrar que o send carrega ids de conector, anexar sem UI (futuro).
Watcher do card de aprovação (1×/s durante o turno): `[role="dialog"], [data-testid="tool-approval-card"]` contendo o nome do conector → clicar `Allow always|Allow once|Permitir sempre|Permitir uma vez` (pointerdown/up + click com coordenadas); `connector_auto_approve_ui=true` prefere "Allow always". Card não some em 10s → ativar aba e clicar; 60s → deny/Stop e falhar com `connector_approval_stuck`. Resposta visível `api_tool unavailable` → `connector_rejected_by_mode` (S5).

### Broker e protocolo de sessão
Control API (loopback, `Authorization: Bearer <daemon.token>`, JSON):
```
GET  /healthz → {ok, pid, version, public_url, registry_status, sessions, active_turns}
POST /v1/sessions {codex_pid, session_id, codex_version} → {session_token, poll_url};  DELETE /v1/sessions/{sid};  POST /v1/sessions/{sid}/heartbeat (10s; 30s sem → morta)
POST /v1/turns {session_id, turn_token, thread_id, turn_id, ttl_ms, tools:[ToolSummary], exec_tool:"exec_command"|"shell", apply_patch:"custom"|null} → {registry_status} (409 se repetido)
DELETE /v1/turns/{turn_token} {reason} → falha chamadas pendentes
GET  /v1/sessions/{sid}/calls?after={seq}&wait_ms=30000 → long-poll {seq, batches:[{turn_token, calls:[PendingCallWire]}]} (seq ≤ after = ack)
POST /v1/calls/{call_id}/result {content, is_error, structured?} → 200 | 404
POST /v1/registry/reconcile;  POST /v1/admin/shutdown_when_idle
```
Transporte: **HTTP/JSON + long-poll** (shape `owner_next` do `turn-broker.ts:234–252`); sem deps novas (WebSocket rejeitado: `axum ws` puxa tokio-tungstenite possivelmente incompatível com o fork 0.28 do workspace). Entrega at-least-once com ack por `seq`; sessão deduplica por `call_id`.

```rust
struct TurnBroker { inner: Mutex<BrokerState>, retire_cap: usize /*256*/ }
struct BrokerState { sessions: HashMap<SessionId, SessionChannel>, turns: HashMap<TurnToken, TurnChannel>, bindings: HashMap<BindingId, TurnToken>, retired: LruMap<TurnToken, RetiredTurn>, calls: HashMap<CallId, oneshot::Sender<BrokerResult>> }
struct TurnChannel { session, trace, tools: Arc<[ToolSummary]>, exec_tool, apply_patch, binding: Option<BindingId>, expires_at, queued: Vec<PendingCall>, batch_timer, in_flight: HashSet<CallId> }
struct PendingCall { call_id /* "call_"+24 bytes */, target: CallTarget, deadline }
enum CallTarget { Function{namespace: Option<String>, name, arguments: Value}, Custom{name, input} }
enum ClaimError { Unknown, Retired{trace, reason}, Expired, SessionGone }
```
`claim`: desconhecido → "turn_token is invalid, expired, or revoked"; retirado → "This turn_token was issued for Codex turn <trace>, which has already finished…"; primeiro claim cunha o binding (idempotente). `invoke`: enfileira, timer 15ms de batch, acorda o long-poll, aguarda oneshot com `timeout(min(connector_call_timeout_ms 120s, expires_at))` → em timeout devolve `is_error` "Codex did not finish <tool> within <n>s… use yield_time_ms ≤ 30000 and poll with codex_write_stdin". `complete` exige `in_flight`. `revoke(token, reason)` falha pendentes e retira; morte de sessão revoga seus turnos ("Codex session disconnected"); sweep de expiração 5s.

Lado sessão (`connector/client.rs` + `mod.rs`): por turno Codex: `ensure_daemon()`; esperar `Verified` (≤`connector_ready_timeout_ms` 90s, senão `UnsupportedOperation` com a razão); cunhar `turn_token`; `POST /v1/turns` com `ToolSummary`s de `Prompt::tools`; cauda do prompt `<codex_transport_resume>… Pass turn_token <T> unchanged to every Codex Native call in this response, including continuations after tool results; do not expose it in the answer …</codex_transport_resume>`; mention; send. Loop `select!{ biased; batch = inbox.recv() => …, done = watcher.completed() => …, _ = consumer_dropped => … }`: batch → validar alvos contra tools registradas, guardar `outstanding`, emitir `OutputItemAdded/Done` por chamada + `Completed{end_turn:false}`; conclusão com `outstanding` vazio e daemon sem `in_flight` → `DELETE /v1/turns`, texto final, `Completed{end_turn:true}`; conclusão com chamadas pendentes no daemon → esperar ≤10s pelo batch. Próximo `stream()` do mesmo turno: casar `FunctionCallOutput` por `call_id` → `POST /v1/calls/{id}/result` (contagem deve bater) → retomar. Cancelamento: `DELETE /v1/turns` + `clickStop` (≤8s).

Mapeamento para o Codex: `CallTarget::Function` → `ResponseItem::FunctionCall{name, namespace (None = "functions"; MCP = namespace do `ToolSpec::Namespace`), arguments, call_id, encrypted_function_args: None}`; `Custom` → `ResponseItem::CustomToolCall{name:"apply_patch", input, call_id}`. Resolução no daemon: `codex_exec` → `exec_command{cmd, workdir?, yield_time_ms?(default `connector_exec_default_yield_ms` 10s), max_output_tokens?, tty?}` se anunciado, senão `shell{command, workdir?, timeout_ms?}`; `codex_apply_patch` → `CustomToolCall` se o turno anuncia `ToolSpec::Freeform("apply_patch")`, senão erro; `FunctionCallOutput` → `BrokerResult` (texto; imagens → `Content::image`; `success == Some(false)` → `is_error`).

### Segurança
Com `tunnel = "openai"` o servidor MCP só escuta em loopback e só o `tunnel-client` (autenticado na OpenAI pela key restrita) o alcança: a superfície pública some; o que resta é a key em `tunnel.key` (0600, só no ambiente do filho) e o `turn_token` por turno. As camadas abaixo valem integralmente para o fallback cloudflared e como defesa em profundidade no modo openai.
Ameaça (cloudflared): quem souber `https://<rand>.trycloudflare.com/mcp/<secret>` fala MCP com o daemon. Camadas: (1) path secreto de 256 bits regenerado por start; (2) toda tool mutante exige `turn_token` (192 bits) válido, ligado a uma sessão e vivo só durante um turno ativo — fora de turno o endpoint só lista 6 nomes; (3) execução sempre pelo sandbox + approval policy do Codex; (4) rate limit global 30 chamadas/10s + 10 claims falhos/min; (5) body 8 MiB; (6) `x-request-id` só logado (hash); (7) API_KEY opcional (fase 2); (8) túnel só loopback; bearer do control em arquivo 0600; (9) nada de segredo em logs/argv (`daemon.json` guarda só o host). Prompt injection: o ChatGPT vê conteúdo do repo e pode chamar tools de escrita — aprovações/sandbox do Codex são a guarda; `connector_auto_approve_ui` só clica o card de consentimento da UI do ChatGPT, nunca altera `approval_policy`. Isolamento multi-sessão: `turn_token → sessão`; uma sessão só completa `call_id`s entregues a ela.

### Config extra `[chatgpt_web]`
| chave | default |
|---|---|
| `connector_name` / `connector_description` | `"Codex Native"` / texto ≤200 chars |
| `tunnel` | `"openai"` (alternativas: `"cloudflared"`, `"manual"` = URL pública fornecida pelo usuário) |
| `tunnel_id` | — (obrigatório com `openai`; formato `tunnel_<32hex>`; gravado por `codex chatgpt-web setup`) |
| `tunnel_key_file` | `CODEX_HOME/chatgpt_web/tunnel.key` (0600; key restrita `Tunnels: Read + Use`; também aceita env `CODEX_CHATGPT_WEB_TUNNEL_KEY`) |
| `tunnel_client_path` | auto (`CODEX_HOME/chatgpt_web/bin/` pinado e verificado → PATH → homebrew) |
| `tunnel_client_version` | pin (ex. `0.0.12`; atualizar após conferir `--help` da versão) |
| `cloudflared_path` | auto (PATH → `C:\Program Files (x86)\cloudflared\cloudflared.exe` → homebrew/usr/local) |
| `cloudflared_extra_args` | `[]` (ex. `["--protocol","http2"]` se QUIC bloqueado) |
| `tunnel_port` / `daemon_port` | `0` / `0` |
| `daemon_idle_shutdown_ms` | `0` |
| `connector_auto_approve_ui` / `connector_auto_developer_mode` | `true` / `true` |
| `connector_call_timeout_ms` / `connector_exec_default_yield_ms` / `connector_ready_timeout_ms` / `turn_ttl_ms` | `120000` / `10000` / `90000` / `3600000` |
| `connector_mention_strategy` | `"auto"` (`"background_only"` \| `"activate"`, definido pelo S2) |

### Spikes (M0 — antes de qualquer Rust do conector; Node + chrome-mcp; Developer Mode ligado manualmente só para os spikes)
- **S0** Captura da mutação do Developer Mode com o tap de fetch; replay via `apiCall` de uma aba nova; verificar 200 vs 403 em `/aip/connectors/mcp/tunnels`.
- **S1** CRUD do conector por API + shape do transporte: criar Tunnel + key restrita no platform (manual, uma vez); servidor MCP Node (stateless, SSE, 6 tools dummy, PRM JSON) atrás de `tunnel-client run` com `MCP_SERVER_URL`; da página: `GET /aip/connectors/mcp/tunnels` (o tunnel aparece?), `list_accessible` (capturar `purpose`), create com `tunnel_id` + NONE, link noauth, `refresh_actions`, `actions` == 6 nomes; registrar headers do cliente (`x-request-id`, UA, `Mcp-Session-Id`, se manda `initialize`, se chama `/.well-known/…`, se o tunnel-client faz `DELETE` de sessão). **S1a** repetir com cloudflared (`mcp_url`) para validar o fallback. **S1b** link `api_key` → header usado.
- **S2** @mention em aba dedicada desfocada: pill sem ativação? tempo de montagem; senão medir o caminho ativar→mention→send→restaurar; capturar o body do send (id do conector?).
- **S3** Round trip com card de aprovação ("Call codex_exec with cmd `echo hi` and turn_token X"; servidor devolve resultado após 2s): card clicado em background? "Allow always" existe/persiste?
- **S4** Persistência da seleção na 2ª mensagem sem re-mention.
- **S5** Tools de escrita no Pro e por modo (Instant/Thinking/Extended/Pro): `codex_apply_patch` escreve arquivo temp; anotar `api_tool unavailable` por modo → `supported_modes` no catálogo.
- **S6** Limites de duração: chamadas de 30/60/100/180/300s com keep-alive SSE → onde o ChatGPT desiste; 3 chamadas paralelas numa resposta (serializa?) → `connector_call_timeout_ms` e cap de yield.

### Milestones do conector
C0 spikes S0–S6 → C1 esqueleto do daemon (subcomando, lock, arquivos de estado, control API, healthz, supervisor de túnel, MCP público com contrato fixo devolvendo "no active turn", PRM) → C2 registro (cliente chrome-mcp no daemon, page scripts, reconcile, Developer Mode auto, erros) → C3 broker + cliente de sessão + loop do provider (mapeamento FunctionCall/CustomToolCall, cauda do prompt, resultados, cancelamento, tokens retirados) → C4 anexação no browser (mention, card, stop, checagem de modo) → C5 endurecimento (rate limit, redação, idle shutdown, backoff, testes, smoke, schema, docs).

Arquivos: `core/src/chatgpt_web/connector/{mod,contract,client,connector_attach}.rs`, `core/src/chatgpt_web/connector/daemon/{mod,lifecycle,control,broker,public_server,tunnel,registry,page_scripts}.rs` (+ `_tests.rs` via `#[path]`), `cli/src/main.rs` (`Subcommand::ChatgptWeb`), `core/Cargo.toml` (axum `http1,tokio,json`; rmcp `server` + `transport-streamable-http-server`), `core/tests/suite/chatgpt_web_connector.rs`, `scripts/chatgpt_web_connector_smoke.ps1`.

## Ordem de entrega

1. **M0/C0 spikes** (Node + chrome-mcp, 1–2 dias): S0–S6 decidem `connector_mention_strategy`, `supported_modes`, timeouts, auth NONE vs API_KEY, e se `legacy_session_mode/json_response` precisam mudar.
2. **M1** provider compila, catálogo, config, roles/spawn → `just test -p codex-core -p codex-model-provider-info -p codex-models-manager` verde.
3. **M2–M3** driver Rust contra o daemon chrome-mcp (testes live `#[ignore]` espelhando o E2E do Node).
4. **M4–M5** modo `none` ponta a ponta: `codex exec -c model_provider=chatgpt_web -m chatgpt-web/instant "Reply with the single word PONG."`, continuidade de conversa, imagens, compaction, arquivamento; role `~/.codex/agents/chatgpt-pro.toml` com `spawn_agent`.
5. **M6 + C1–C4** modo `connector` com daemon compartilhado.
6. **M7 + C5** polimento, docs (`docs/chatgpt_web_agents.md`), schema.

Cada milestone: `just fmt`, `just fix -p <crate>`, `just test -p <crate>`; `just write-config-schema` sempre que `config_toml.rs` mudar; commits `feat(chatgpt_web): …` com `// FORK:` em toda divergência; suíte completa só com aval do usuário.

## Verificação

- Unit (in-module, `#[path]`): `history_tests` (extend/restart/model-mismatch/compaction), `stream_tests` sobre fixtures reais de `/backend-api/conversation` (in-progress, thoughts+text, `end_turn:true`, retry/regenerate, `in_progress` velho antes do último user, assets `end_turn:null`, `api_tool` com/sem `parent_id`, 429), `sessions` round-trip, `tabs` (temp dir, pid morto), `page_scripts` (sem `async`), `daemon::parse_result`, mapeamento de erros, uso, `connector` com broker fake; do daemon: máquina de estados do broker (claim idempotente, retirado, batch, expiração, morte de sessão, complete-before-delivered, seq/ack), planner do registro com `FakeApi` (fresh, URL nova, 403→dev mode→retry, 429, limpeza de nomes antigos, verify mismatch), parser de URL/erro do cloudflared, validação/resolução do contrato, `FunctionCallOutput`→MCP.
- Integração (`core/tests/suite/chatgpt_web_connector.rs`): daemon in-process em portas efêmeras com `NoopTunnel` e registro fake; cliente `codex_rmcp_client` como "ChatGPT": `initialize` (e `tools/call` cru sem initialize), `tools/list` == contrato, `codex_exec` bloqueia → sessão fake long-polla, recebe o batch, posta resultado → chamada retorna; tokens desconhecidos/retirados; desconexão de sessão falha a chamada; path errado → JSON 404; PRM; healthz.
- Live (daemon chrome-mcp + Chrome logado): E2E portado (`core/tests/chatgpt_web_live.rs`, `#[ignore]`, `--test-threads=1`): status, models, new chat instant, read, continuation, upload, stop, image gen + download, `--pro`, cleanup, restore Pro; `concurrent_probe` (pool cresce, mesma conversa serializa, `max_parallel_turns`); `dup_upload`. Smoke do conector: `codex chatgpt-web daemon --foreground`; `codex exec -m chatgpt-web/thinking "run codex_exec echo CONNECTOR_OK and report it"` → transcript com célula `exec_command` e resposta com `CONNECTOR_OK`; `codex chatgpt-web registry show`; matar o cloudflared → reconcile em ≤90s; `codex chatgpt-web stop`.

## Riscos principais

| Risco | Detecção | Mitigação |
|---|---|---|
| Cliente MCP do ChatGPT exige sessão ou JSON-only | S1 | `legacy_session_mode`/`json_response`; último recurso: handler JSON-RPC axum manual (6 tools) |
| Sondagem OAuth nos trata como OAuth | S1 | PRM 200 JSON com `authorization_servers: []` |
| `purpose` desconhecido (422) | S1 | captura; fallback: não listar, rastrear só por `connector.json` |
| Toggle de Developer Mode não automatizável | S0 | erro acionável + `codex chatgpt-web doctor` |
| Popover do @mention não monta em aba oculta | S2 | ativar→mention→send→restaurar; menu "+"; injeção do id no send |
| Card de aprovação não clicável em background | S3 | ativar + clicar; "Allow always"; 60s → falhar |
| Escrita recusada no Pro/modos (`api_tool unavailable`) | S5 | `supported_modes` no catálogo; aviso read-only nesses modos |
| Timeout por chamada do ChatGPT < duração da tool | S6 | yield ≤30s + `write_stdin`; deadline no daemon devolve erro explícito; keep-alive SSE evita 524 |
| Key/tunnel id inválidos ou tunnel de outra conta/workspace | log JSON de auth do tunnel-client; `mcp/tunnels` não lista o id | `Fatal` sem retry + mensagem apontando `codex chatgpt-web setup`; validar `tunnel_id` contra `GET /aip/connectors/mcp/tunnels` antes de criar |
| Flags do tunnel-client mudam entre versões | `--help` da versão pinada; `doctor` no setup | pin por versão + sha; `tunnel_client_version` configurável |
| Churn de URL (só cloudflared) | healthz público; `registry_status` | reconcile antes de liberar turnos; ≤90s de espera; padrão `openai` não tem esse problema |
| Abuso do endpoint (só cloudflared) | contadores de rate limit | path secreto por start + turn_token + aprovações do Codex + API_KEY (fase 2) |
| Dois daemons (versões/corrida) | lock; `codex_version` | lock exclusivo Windows / flock unix; `shutdown_when_idle` + respawn |
| Sessão morre no meio de chamada | heartbeat 30s | revogar turnos; MCP recebe "Codex session disconnected" |
| `turn_token` velho reaproveitado do histórico | claim em token retirado | LRU 256 com mensagem "already finished" |
| Labels localizados (PT) quebram seletores | S2/S3 | casar por nome do conector + regex de botões PT/EN |
| `apply_patch` não anunciado | erro de resolução | catálogo com `apply_patch_tool_type: "freeform"` |
| Contrato cacheado após reuso do nome | `VerifyActions` | versão de contrato → nome novo |

Não verificado nas fontes (capturar ao vivo): `purpose`; chaves opcionais de `auth_request` e shape da resposta do link; header do link `api_key`; se o ChatGPT manda `initialize`/session id; timeout por chamada; conectores em Pro/thinking; menu de mention em aba oculta; persistência da seleção; "Pro é read-only"; paralelismo de chamadas MCP; como o send codifica o conector.

---

## Apêndice A — Digest da pesquisa (para os agentes de desenho e para a implementação)

### A1. Fork: pontos de integração do `claude_code` (espelhar para `chatgpt_web`)

Obrigatórios (erro de compilação sem novo arm):
- `codex-rs/model-provider-info/src/lib.rs`: `enum WireApi {Responses, ClaudeCode}` (63–77, `#[serde(rename)]` explícito), `Display` (79–87), `Deserialize` manual (89–105, lista `&["responses","claude_code"]`), `create_claude_code_provider()` (437–463: todos HTTP `None`, `requires_openai_auth:false`, `supports_websockets:false`), `built_in_model_providers()` (544–577, entry 572), `CLAUDE_CODE_PROVIDER_ID` (46).
- `codex-rs/core/src/client.rs:1926–1990`: dispatch `match wire_api`; arm `ClaudeCode` chama `claude_code::stream(prompt, model_info, effort, workspace, Arc<ClaudeCodeThreadState>, thread_id)`. Estado cross-turn em `ModelClientState.claude_code` (225–228, init 491); workspace em `ModelClient` (269) e por turno em `ModelClientSession` (301); `set_claude_code_host` (2003–2010).
- `codex-rs/config/src/thread_config/remote.rs:305–313` (`proto_wire_api`, `unreachable!` para locais).

Silenciosos (comportamento errado sem update): `core/src/session/turn.rs:2271` (attach do host), `core/src/compact.rs:128` (pula auto-compaction), `core/src/tools/handlers/multi_agents_common.rs:54,325,359,451`, `core/src/tools/handlers/multi_agents_v2.rs:99` (`require_readable_message_form` → exige `plaintext_message`), `core/src/agent/role.rs:94–99` (filtro `provider == CLAUDE_CODE_PROVIDER_ID` — hoje valor único), `core/src/claude_code/history.rs:217–223` (drop por namespace no replay), `exec/src/event_processor_with_human_output.rs:444`, `tui/src/status/card.rs:302`, `otel/src/events/session_telemetry.rs:1293`, `core/src/turn_timing.rs:390`.

Eventos: `ResponseEvent` em `codex-rs/codex-api/src/common.rs:96–154` (`Created`, `OutputItemAdded/Done`, `OutputTextDelta`, `ReasoningSummaryDelta{delta,summary_index}`, `ReasoningSummaryPartAdded`, `RateLimits`, `ProviderExecutedTool`, `Completed{response_id, token_usage, end_turn}`); `ResponseStream` = `mpsc::Receiver` + `consumer_dropped: CancellationToken` (`core/src/client_common.rs:108–127`); `TokenUsage` em `protocol/src/protocol.rs:2118–2137`.

Módulo `core/src/claude_code/`: `mod.rs` (2142 l.: `ClaudeCodeWorkspace::from_config`, `stream` 536–608 → `tokio::spawn(run_turn)`, `translate_stream` 1180–1559 com watchdog idle em 1220–1258 via `tokio::time::timeout` por linha + `tokio::select!` com `consumer_dropped`, `StreamAssembler` 1645–1843), `history.rs` (`plan_request` 64–96: `can_extend` se `session_id` existe e o fingerprint do prefixo `input[..delivered_items]` bate; filtra `echoed`; `render_turn` embrulha replay em `<codex_transcript>`; drop de `namespace == CLAUDE_TOOL_NAMESPACE` 217–223; `MAX_TOOL_OUTPUT_CHARS=6000`), `sessions.rs` (`claude_code_sessions.json`: `threads: BTreeMap<thread_id, {session_id, delivered_items, delivered_fingerprint, account_dir, pinned_account, echoed, updated_at_ms}>`, TTL 7d, 512 entradas), `state_file.rs` (provider-agnóstico: mutex in-process + lock advisory em sidecar + rename atômico — **reusar como está**), `tools.rs` (`CLAUDE_TOOL_NAMESPACE="claude_code"`; `PendingToolUses`; `history_items` emite `FunctionCall`+`FunctionCallOutput` com namespace; consumido em `turn.rs:2611–2648` sem dispatch), `host.rs` (`trait ClaudeHost`: `approve_tool`, `call_bridge_tool`, `bridge_tool_specs`; `APPROVAL_TIMEOUT=300s`), `session_host.rs` (`SessionClaudeHost` com `approve_command`/`approve_patch`; listas `BRIDGE_PLAIN_TOOLS`/`BRIDGE_DENIED_TOOLS`), `bridge.rs` (MCP in-process: `initialize`, `tools/list`, `tools/call`, `ping`), `control.rs`, `accounts.rs`.

Config: `config/src/config_toml.rs:153–216` (`ClaudeCodeToml`, `ConfigToml.claude_code`), `core/src/config/mod.rs:243–245, 900–922, 4195–4226` (defaults `idle_timeout_ms=600000`, `0→None`), `core/config.schema.json` (gerado por `just write-config-schema`), `features/src/lib.rs:104–108, 932–938`, `thread-manager-sample/src/main.rs:207–212` (struct literal de `Config` — quebra compilação ao adicionar campos).

Catálogo: `models-manager/src/local_models.rs` (`locally_served_models()` OnceLock sobre `include_str!("../claude_code_models.json")`; `merge_locally_served_models`), merges em `manager.rs:334, 341, 354`; ordenação por `priority` (127); `visibility: "hide"` = agent-only (`manager.rs:493`, teste em `local_models.rs:56–61` exige Hide para todos os bundled — ajustar teste). Inferência modelo→provider: `multi_agents_common.rs:345–374` `align_provider_with_locally_served_model` (hoje sempre troca para `claude_code` — precisa mapear slug→provider). Spawn v2: `multi_agents_v2/spawn.rs:138–177, 205`.

Timeouts: o caminho local nunca passa pelo `stream_idle_timeout` do Responses; o único é o watchdog idle do provider. Aprovações: `SessionClaudeHost` → eventos de aprovação da `Session`.

Servidor MCP HTTP em Rust já existe como template: `tui/src/dynamic_tools_mcp.rs:88–176` (`TcpListener 127.0.0.1:0`, `StreamableHttpService::new(handler, LocalSessionManager, StreamableHttpServerConfig)`, axum `Router::nest_service("/mcp")`, middleware Bearer uuid). Features rmcp `server` + `transport-streamable-http-server` ligadas em `rmcp-client` e `tui`; `axum 0.8` e `reqwest 0.12` no workspace. Cliente MCP HTTP: `rmcp-client/src/rmcp_client.rs:483–579` (`new_streamable_http_client(name, url, bearer, headers, …)`), `initialize` 584, `call_tool(name, args, meta, timeout)` 771–836, `shutdown` 963.

Convenções: `// FORK:` em toda divergência com o porquê; commits Conventional (`feat(chatgpt_web): …`); `just fmt` após mudanças; `just fix -p <crate>`; `just test -p codex-core` (nunca `cargo test`; suíte completa só com aval); `just write-config-schema` após mexer em `config_toml.rs`; testes em módulo via `#[path = "..._tests.rs"]`, nomes em frase.

### A2. Driver a portar (`chatgpt-pro-mcp/src`, ~3k linhas TS)

- `daemon.ts`: `StreamableHTTPClientTransport` para `http://127.0.0.1:8848/mcp` + `Authorization: Bearer <~/.chrome-mcp/token.txt trimmed>`; **sem header `Origin`** (daemon devolve 403 se houver qualquer Origin; ausente passa); `DELETE /mcp` com `mcp-session-id` no shutdown (senão vaza sessão); `call()` timeout cliente `max(120s, timeoutMs+30s)`, 1 retry de reconexão; `parseResult` (isError→throw; image→base64; JSON.parse do texto); `evalIn` = `browser_eval {tabId, expression, world:"MAIN", timeoutMs}` com **dupla decodificação** (o script devolve string JSON); `GET /healthz` sem auth.
- `tab.ts`: pool de até `CHATGPT_MCP_MAX_TABS` (3, clamp 1–8) abas dedicadas (`browser_tabs {action:"create", url, dedicated:true}` → janela normal desfocada), registro cross-process `~/.chatgpt-pro-mcp/tabs.json` `{owners:[{tabId,pid,since}]}` com mutex `mkdir tabs.json.lock` (steal >10s, deadline 5s), adoção de abas órfãs (`pid null`/morto), `TabLock` FIFO por aba, afinidade aba↔conversa, sweeper de idle (`CHATGPT_MCP_TAB_IDLE_MS` 300s), `waitReadyOn` (retry em "execution context"/"No frame", cap `pageWait+60s` por throttling de aba oculta), `withActivatedOn` (ativa aba → fn → **reload** → restaura foco; único jeito de montar menus Radix), `gotoOn` = `browser_navigate {waitUntil:"load", timeoutMs:30000}`.
- `page-scripts.ts` (13 scripts, strings puras, **nunca `async`** — promise chains; sempre resolvem `JSON.stringify(...)`): `waitReady`, `composerState`, `setComposerText` (selectAll+delete → `ClipboardEvent('paste')` com `DataTransfer` → fallback `insertText`), `attachmentTiles`, `dismissUploadDialog`, `clearComposer`, `clickSend` (MouseEvent sintético; espera stop + URL `/c/<id>`), `clickStop`, `apiCall` (token cache `window.__cgptmcpTok` 10 min via `/api/auth/session`), `stageDownload`/`readDownloadChunk` (chunks de 4M base64), `menuDiscover`/`menuSelect` (só com aba visível).
- `api.ts`: `GET /backend-api/conversations?offset&limit&order=updated`, `GET /backend-api/conversation/<id>` (mapping tree, walk `current_node`→parents, `anyInProgress` só após o último user turn), `PATCH` `{title}|{is_archived:true}|{is_visible:false}`, `GET /backend-api/models` (cache 5 min), backoff 429 `[2s,5s,10s]`, `assetFromPointer` (`file-service://`, `sediment://`).
- `ops.ts`: `LEVEL_LABELS` (instant `Instant[âa]neo|Instant`, medium `M[ée]dio|Medium`, high `^Alto$|^High$`, extra-high `Extra alto|Extra high`, pro `^Pro$`); `resolveModel` (slug exato; `instant`/`thinking`/`pro` via `?model=<base>-<sufixo>`; `medium|high|extra-high` = slug thinking + nível por menu); fases de envio `navigate|model|precheck|upload|compose|attachments-wait|submit|confirm` com hint de segurança de retry; precheck de "generating" com 5s de graça + reload se API diz idle; upload via `browser_upload` em `form input[type="file"]:not([accept*="image"])` (fallbacks), dedupe de arquivos já enviados por nome+tamanho, popup de duplicado; `confirmSubmitted` quando o submit é ambíguo; **`waitReply`**: poll 2.5s, âncora no último user turn, `done = reply && idle && (endTurn===true || fingerprint estável)`, `asyncActive = async_status ∉ {null,0}`; `stop` (clickStop por 8s); `downloadAssets`.
- E2E a espelhar (`test/e2e.ts`): status, list, model list, new_chat instant, read, send continuation, upload+reply, stop, image gen+download, pro round (`--pro`), cleanup, restore Pro; `dup-upload.ts`; `concurrent-probe.ts` (pool cresce; mesma conversa serializa; 2 processos não cruzam respostas).

### A3. Daemon chrome-mcp (`chrome-mcp/packages`)

Endpoint `POST/GET/DELETE 127.0.0.1:8848/mcp`; sessão nova por `initialize` (`mcp-session-id`); **aba ativa por sessão** (`tabId` omitido → `lastTabId` da sessão → aba ativa); lock global por `tabId` (chamadas sem tabId não serializam); body cap 32 MB; timeout por chamada `params.timeoutMs` (default 30s) + 2s; erros vêm como `{isError:true}` e não como erro JSON-RPC. `browser_eval` = `chrome.debugger` + `Runtime.evaluate {returnByValue, awaitPromise: true, userGesture: true}`; parâmetro `world` **ignorado** (sempre MAIN); expressão função é invocada `(<expr>)(<args>)`; mostra o banner de depuração (ref-counted por chamada). `browser_upload` via `DOM.setFileInputFiles` (não redispara `input`/`change` — evita popup de duplicado). `browser_screenshot mode:viewport` **ativa a aba** (rouba foco) — evitar; usar `fullpage`.

### A4. Referências externas (clones no scratchpad da sessão, `.../scratchpad/{codex-chatgpt-web,chat-on-steroids}`)

- codex-chatgpt-web: catálogo (`src/model-catalog.ts:97–151`: `supported_in_api:true`, `tool_mode:null`, um nível por linha, `priority` não acima dos nativos, `context_window`/`auto_compact_token_limit` medidos: Pro 111k/95k, Plus 90k/80k; reserva oculta 8192 tokens); contrato de prompt (`src/adapters/chatgpt-web/prompt.ts:346–490`); taxonomia de erros de UI (`browser-worker.ts:154–235`, `adapter-error.ts`); broker de `turn_token` (`turn-broker.ts`), servidor MCP do conector (`mcp-server.ts:195–426`: `codex_exec`, `codex_write_stdin`, `codex_apply_patch`, `codex_view_image`, `codex_tool_inventory`, `codex_tool_call`, todos com `turn_token`), round-trip conector→`function_call`→Codex→resultado (`index.ts:456–555`); seleção do conector por `@mention` no composer (`browser-worker.ts:1597–1733`: digitar `@codex`, esperar linha `.__menu-item[tabindex="0"]` com o nome exato, ArrowDown até `data-highlighted`, Enter, verificar pill `[data-id^="plugin:"]`); card de aprovação `[role="dialog"], [data-testid="tool-approval-card"]` "Allow ChatGPT to use X?" → "Allow once"; cap 5 abas; retry 3/turno/30min; cancelamento → HTTP 400 não-retryable.
- chat-on-steroids: regra de conclusão (`extension/fiber.js:512–529`: msg assistant `content_type:'text'` mais nova com `end_turn && status=='finished_successfully'`; requests `api_tool` (`recipient` começa com `api_tool`) precisam de result com `metadata.parent_id`); Stop button pisca 0.4–2.7s entre fases (settle 4s); handoff brief (`src/main/session/handoff-prompt.ts:13–68`); 3 workers paralelos disparam "too many requests" (default 2); bootstrap curto em chat novo evita heurísticas de abuso.
