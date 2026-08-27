# Plano: paridade nativa dos agentes Claude no Codex + melhorias do core guiadas por 30 dias de sessões
> Plano descritivo gerado em 2026-08-26 a partir da análise de 30 dias de sessões (27/07–26/08) e do mapa do código do fork.
> Fundamentos e raciocínio por fase: [FUNDAMENTOS.md](./FUNDAMENTOS.md).


## Context

O fork (`C:\Users\Joao\RustProjects\codex`) roda o Claude Code CLI como provider local (`claude_code`) para subagentes `claude-opus`/`claude-sonnet`. Objetivo do usuário: **"Claude funcionar igual os agentes nativos do Codex, o mais próximo possível, para poder ser usado melhor pela thread principal."** Entregável: plano de implementação no fork (Rust + prompts internos + roles/config); caminho MCP legado `claude_agents` desativado.

Pedidos adicionais (26/08, durante o planejamento): cards "Enviado por ChatGPT de outra tarefa" cada vez mais frequentes no pai (subagentes reportando via `codex_app.send_message_to_thread`) e o Desktop pedindo permissão a cada chamada dessa tool.

### Evidência (27/07–26/08: 2.607 rollouts, 130 raízes, 1.852 filhos Luna / 748 filhos Claude)
| métrica | Luna/OpenAI | Claude (`claude_code`) |
|---|---|---|
| sessões com turno abortado | 5–17% por role | **opus 46%, sonnet 24%** |
| sessões sem `task_complete` | 17 | **48** |
| linhas próprias no rollout (mediana) | 173–320 | **26 / 58** (zero tool calls gravadas) |
- `wait_agent`+`wait` nas raízes: 28.986 chamadas; 13.942 "wait timed out"; 593 `interrupt_agent` (70% mirando filhos Claude).
- Preâmbulo injetado no filho Claude: mediana 37k chars, p90 149k, máx 1,39M (AGENTS.md inteiro, `<recommended_plugins>`, `<model_switch>`, developer message de colaboração + `<multi_agent_mode>` contraditório).
- Caminho MCP `claude_agents`: 118 `claude_spawn` / 6.246 `claude_wait` (14,1 h) em 05–15/08; **zero desde 15/08** → retirar é seguro.
- Falhas de spawn: "agent thread limit reached" 112×; "Service tier `priority` is not supported for model `claude-opus-5`" 4×; `apply_patch verification failed` 768×.
- Instruções que o usuário cola em todo prompt: "thread principal só orquestra" 102×, "siga o plano" 64×, "preserve worktree sujo, sem reset/checkout, use apply_patch" 54×, "documente em disco" 50×, "pare com hash/gate/certificação" 47×, "indicador X/Y some após compactação" 14×, "pode parar de me perguntar".
- Diagnóstico dos cards (já feito): Desktop 26.820 injeta `mcp_servers.codex_app={…, omit_tools_from:["deferred","code_mode"], tools:{send_message_to_thread:{approval_mode:"prompt"},…}}` ⇒ tools viram `DirectModelOnly` para todo thread, inclusive subagentes; `approval_mode: prompt` é sempre bloqueante (`core/src/mcp_tool_call.rs:2222-2234`) e config de usuário só pode **apertar** (`config/src/mcp_types.rs` `restrict_to`); `disabled_tools` sobrevive ao deep-merge (`config/src/overrides.rs::apply_toml_override`).

### Fatos verificados no código/CLI que moldam o desenho
- Driver `core/src/claude_code/mod.rs`: `build_claude_command` :700-759; stdin recebe uma linha e é fechado :521-526; `translate_stream` :796-1047 transforma `tool_use` em linha de reasoning (`describe_tool_use` :1194-1224) e **descarta** `tool_result` (:1027 `_ => {}`); `prompt.tools`/`prompt.base_instructions` ignorados (:366).
- `claude.exe` 2.1.246 tem o protocolo de controle stdio (strings confirmadas no binário): `--permission-prompt-tool stdio` → `can_use_tool` control_request; `initialize{appendSystemPrompt, sdkMcpServers}`; `mcp_message` bidirecional (servidor MCP hospedado in-process pelo cliente); `get_usage`; `--include-partial-messages`; `end_session`; `tool_use_result` estruturado. Sem prompt surface, decisões "ask" em `--permission-mode auto` são **terminais** — causa raiz do "dotnet requires approval".
- Cada fragmento injetado carrega `content_item_kinds` (`context-fragments/src/fragment.rs:34-51`; preservado em `context_manager/updates.rs:12-30`; só limpo no caminho Responses em `client.rs:998-1007`) → filtro por tipo substitui o stripping guloso de `history.rs:267-288`.
- `ResponseEvent` (`codex-api/src/common.rs:97`) é enum com matches exaustivos; `OutputItemDone(FunctionCall)` é despachado pelo `ToolRouter` (`tools/router.rs:148-165`, `stream_events_utils.rs:297-328`) — logo itens já executados pelo Claude precisam de variante própria.
- `wait_agent` (`multi_agents_v2/wait.rs:136-148`) devolve só `{message, timed_out}` e acorda com mail de **qualquer** agente (`session/input_queue.rs:98-119`). Defaults: min 10 s / default 30 s / max 3600 s (`core/src/config/mod.rs:229-231`).
- `claude_code_models.json`: `service_tiers: []` (→ erro duro em `multi_agents_common.rs:429-445`), `multi_agent_version: "v1"`, `visibility: "hide"`.
- `update_plan` não persiste nada (`tools/handlers/plan.rs:89-92`); `compact.rs::build_compacted_history` :644-731 descarta o plano.
- `claude_code_accounts.json`/`claude_code_sessions.json`: temp `json.tmp.<pid>` compartilhado por 10 agentes no mesmo processo + `remove_file`→`rename` (`accounts.rs:355`, `sessions.rs:78`) → perda de escrita. Primitivo pronto: `message-history/src/lib.rs:159-180` (`File::try_lock` + retry).
- Compactação em thread Claude: o prompt de sumarização chega via `input` (`compact.rs:129-133`), mas `compact.rs:265` cria `ModelClientSession` sem `set_claude_code_workspace` e a reescrita do prefixo invalida o fingerprint (`history.rs:67-70`) → replay integral.

## Decisões de desenho
- **D1 — stdio bidirecional com o CLI.** Manter stdin aberto durante o turno e falar o protocolo de controle (`control_request`/`control_response`/`control_cancel_request`). Habilita aprovações, bridge MCP, usage e system prompt sem flags gigantes na linha de comando.
- **D2 — `ResponseEvent::ProviderExecutedTool` (fork-only).** Itens que o Claude já executou são gravados e exibidos, nunca despachados.
- **D3 — `ClaudeHost` por sampling request.** Aprovações e tools da bridge precisam de `Arc<Session>`, `StepContext`, `SharedTurnDiffTracker`, `CancellationToken` — todos em escopo em `session/turn.rs:2199-2243`.
- **D4 — bridge MCP in-process** via `sdkMcpServers: ["codex"]` + `mcp_message`; sem porta/token/processo órfão. Chamadas despachadas pelo `ToolRouter` real com `ToolCallSource::DirectPlaintextMessage` (elimina `message` vs `plaintext_message` para o filho).
- **D5 — filtro de contexto por `content_item_kinds`**, não por texto; role instructions vão em `appendSystemPrompt`.
- **D6 — só código fork-only com `// FORK:`**; pontos de toque em arquivos upstream mínimos e comentados.

## Ordem de execução (cada fase é entregável isolado)

### Fase 0 — hoje, sem Rust: config, AGENTS.md, roles, retirada do MCP
1. `~/.codex/config.toml`:
   ```toml
   # FORK: o plugin do Desktop declara approval_mode = "prompt"; config só pode apertar.
   [plugins."codex-app-tools@openai-bundled".mcp_servers.codex_app]
   disabled_tools = ["send_message_to_thread", "create_thread", "fork_thread", "handoff_thread", "automation_update"]

   [features.multi_agent_v2]
   min_wait_timeout_ms = 15000
   default_wait_timeout_ms = 120000
   ```
   (Se o override do Desktop entrar como `mcp_servers.codex_app`, usar `[mcp_servers.codex_app] disabled_tools = [...]`.) Trade-off explícito: global — a raiz também perde `create_thread`/`fork_thread` até a Fase 7.1. Verificar com `codex debug config`.
2. `~/.codex/AGENTS.md`: remover §"Caminho MCP (`claude_agents`)" e §"Regras de uso do caminho MCP"; remover itens 0/0b (já impostos em código) e substituir por uma linha ("use `plaintext_message`; `fork_turns` não se aplica: a tarefa deve ser autossuficiente"); adicionar §"Como os agentes reportam" (resposta final ou `send_message` `target: ".."`; nunca `codex_app`; `create_thread`/`fork_thread`/`handoff_thread` são do usuário; não interromper agente com atividade recente; `wait_agent` longo, sem polling; progresso em `agent_docs/latest_session_work.md` + `update_plan` após compactação); adicionar itens 8 ("não pergunte o que o plano já responde") e 9 ("worktree suja é o normal: nunca reset/checkout/clean/stash/commit; edições via apply_patch") ao "Lema de execução".
3. Roles `~/.codex/agents/`: `executor_luna/executor_sol/tester/doc-writer.toml` ganham parágrafo "report only via final answer or `send_message` `..`; never `codex_app` tools; never touch git state"; `claude-opus/claude-sonnet.toml` ganham 3 bullets equivalentes em PT (reporte só na resposta final; não use `codex_app`; working tree compartilhada e suja; sem histórico do pai → pergunte ao pai em vez de inventar premissa). `explorer.toml` fica.
4. Retirar o bridge: apagar `~/.codex/claude-agents/` (server.mjs, .bak, logs, state) e backups `config.toml.bak-*` que o referenciam ficam como estão; `docs/claude_code_agents.md` ganha seção "Reporting and orchestration" e perde qualquer menção ao MCP. (Nenhum código Rust referencia `claude_agents` — verificado.)

### Fase 1 — robustez de spawn (~80 linhas, sem risco)
- `core/src/tools/handlers/multi_agents_common.rs`: `apply_spawn_agent_service_tier` (:402-455) → para `WireApi::ClaudeCode` ou tier não suportado, **descartar com nota** em vez de erro; `validate_spawn_agent_reasoning_effort` (:524-544) → `clamp_spawn_agent_reasoning_effort` (maior nível suportado ≤ pedido; `Ultra|Max→max`, `None|Minimal→low`, `Custom→default`) nos 3 call sites; `task_fork_mode_for_wire_api` (:39-48) passa a devolver nota quando ignora `fork_turns`.
- `multi_agents_v2/spawn.rs:353-363`: `SpawnAgentResult.notes: Vec<String>`; atualizar `spawn_agent_output_schema_v2` (`multi_agents_spec.rs`).
- `core/src/agent/role.rs:368-373`: nota do service tier vira "Do not pass `service_tier` for this role" (hoje convida o modelo a passar).
- `multi_agents_spec.rs:20-22` `LOCAL_AGENT_PLAINTEXT_MESSAGE_DESCRIPTION`: "Required for every Claude agent type". (`message` é cifrado para o backend OpenAI — auto-conversão é impossível; verificado.)
- `claude_code/mod.rs:733-735`: `fn claude_effort(effort) -> Option<&str>` com o mesmo clamp para `--effort`.
- Testes inline em `multi_agents_common.rs` (`an_unsupported_service_tier_is_dropped_with_a_note`, `an_unsupported_reasoning_effort_clamps_down_not_up`, `clamping_is_skipped_for_fallback_model_metadata`) e `spawn.rs` (`spawning_a_claude_agent_with_fork_turns_returns_a_note`).

### Fase 2 — contexto limpo para o filho Claude (independente)
- `claude_code/history.rs::render_item` (:141-161): filtrar `content` por `internal_chat_message_metadata_passthrough.content_item_kinds`; **drop**: `agents_md.instructions`, `plugins.*`, `apps.instructions`, `model_switch.*`, `multi_agent.mode_instructions|usage_hint|role_instructions`, `personality.spec_instructions`, `model.base_instructions`, `generic.developer_instructions`, `token_budget.*`, `rollout_budget.*`, `environments.instructions`, `apply_patch.legacy_exec_command_warning`, `unified_exec.*`, `realtime_conversation.*`; **keep**: `user.text|image`, `multi_agent.inter_agent_*`, `multi_agent.subagent_notification`, `compaction.summary`, `hooks.additional_context`, `generic.turn_aborted`, `unknown`. Kinds exatos = `content_kind()` em `core/src/context/`.
- `strip_codex_harness_sections` (:267-288) vira fallback estreito para rollouts antigos sem kinds: marcador só em linha própria e corte até a tag de fechamento, nunca até o fim da mensagem.
- `fn claude_system_prompt(&ClaudeCodeWorkspace) -> String` em `mod.rs`: (1) `developer_instructions` do role (passar `Config::developer_instructions` para `ClaudeCodeWorkspace`), (2) bloco fixo de protocolo ("You are a subagent inside Codex; parent `<agent_path>`; report via final message; use `mcp__codex__send_message` for mid-task messages"), (3) cwd, roots (`--add-dir`) e quais são writable, modo de aprovação. Entregue via `initialize.appendSystemPrompt` (Fase 3) — não como flag (limite de linha de comando no Windows).
- `fork_turns` para Claude: `[claude_code] max_fork_turns` (default 0 = comportamento atual); honrar `LastNTurns(n.min(max))`, continuar forçando `None` para `FullHistory`.
- Testes: substituir em `history_tests.rs` os testes de stripping por equivalentes por kind (construindo via `build_rendered_message`), regressão "role instruction após `## Memory` sobrevive", orçamento de tamanho (< ~4k chars para bundle realista).

### Fase 3 — transporte do protocolo de controle (fundação das fases 4–6)
- Novo `core/src/claude_code/control.rs`: `ControlChannel { tx_stdin, pending: HashMap<request_id, oneshot>, next_id }`, `InboundControl { CanUseTool, McpMessage{server,message}, HookCallback, Unknown }`, `CanUseTool { request_id, tool_name, input, tool_use_id, permission_suggestions, decision_reason(_type), blocked_path, … }`, `ToolPermissionDecision { Allow{updated_input}, Deny{message, interrupt} }`; `request()/respond_success()/respond_error()/cancel()`.
- `mod.rs`: substituir o writer fire-and-forget (:521-526) por task de escrita com `mpsc::Sender<String>`; fechar stdin só após o frame `result` (ou `consumer_dropped`); enviar `end_session` antes de fechar se o CLI não sair sozinho (manter `kill_process_tree` :617-636 e idle watchdog como backstop). Em `translate_stream` (antes de :873): arms `control_request`/`control_response`. Primeira linha em stdin: `initialize{appendSystemPrompt, sdkMcpServers:["codex"]}` com espera limitada (~10 s); falha é não-fatal (segue sem bridge).
- `build_claude_command`: `--permission-prompt-tool stdio` quando aprovações interativas (Fase 4); `--include-partial-messages` (Fase 8.7).
- Feature flag `Feature::ClaudeCodeControlProtocol` (padrão em `features/src/lib.rs:105`), default **on** no fork após validação, com fallback automático ao caminho atual se `initialize` falhar.
- Testes: matriz de flags em `mod.rs` `mod tests` (:1244); fixture de `control_request` → bytes de `control_response`.

### Fase 4 — aprovações e mapeamento de sandbox (depende da 3)
- Novos `core/src/claude_code/host.rs` (`trait ClaudeHost { approve_tool, call_bridge_tool, bridge_tool_specs, env_brief }`) e `session_host.rs` (`SessionClaudeHost { session, step_context, tracker, cancel }`).
- `approve_tool`: `Bash` → `Session::request_command_approval` (`session/mod.rs:2474-2557`); `Edit|Write|MultiEdit|NotebookEdit` → `request_patch_approval` (:2563-2599); demais → command approval com comando sintetizado. `Approved|ApprovedForSession → Allow` (+ `updatedPermissions` a partir de `permission_suggestions` para o CLI parar de perguntar); `Denied|Abort → Deny`. Timeout limitado no host → `Deny{interrupt:false}` (evita deadlock pai↔filho; `request_command_approval` já devolve `Abort` se o canal cair).
- Substituir `permission_mode_for` (:216-221) por função de (sandbox, approval) lendo `config.permissions.legacy_sandbox_policy(&cwd)`:

  | sandbox | approval | `--permission-mode` | extras |
  |---|---|---|---|
  | DangerFullAccess / ExternalSandbox | Never | `bypassPermissions` | — |
  | WorkspaceWrite | Never | `acceptEdits` | `--add-dir` por root |
  | WorkspaceWrite | OnRequest/OnFailure/UnlessTrusted | `auto` | `--permission-prompt-tool stdio`, `--add-dir` |
  | ReadOnly | qualquer | `plan` | `--tools Read,Glob,Grep,WebFetch,WebSearch,TodoWrite,Task` |

  Nunca combinar `bypassPermissions` com prompt tool (o CLI suprime `can_use_tool`).
- `ClaudeCodeWorkspace` (:76-103) ganha `writable_roots`, `sandbox`, `host: Option<Arc<dyn ClaudeHost>>`; `permission_mode` passa a ser calculado. Wire-up em `session/turn.rs::try_run_sampling_request` (~:2230) guardado por `wire_api == ClaudeCode`; manter :170 como fallback.
- Testes: tabela 8 combinações; fake host → `can_use_tool` → `Allow/Deny` correto.

### Fase 5 — paridade de atividade de tools (depende da 3; independente da 4)
- `codex-api/src/common.rs:97`: `ResponseEvent::ProviderExecutedTool(Box<ProviderExecutedTool { call_id, turn_items: Vec<TurnItem>, history_items: Vec<ResponseItem>, applied_patch_delta: Option<AppliedPatchDelta> }>)`.
- `session/turn.rs` (match em ~:2317): gravar `history_items` via `record_completed_response_item` (`stream_events_utils.rs:77`), emitir `emit_turn_item_started/completed` (`session/mod.rs:2274`), aplicar `applied_patch_delta` no `turn_diff_tracker` (+ `EventMsg::TurnDiff`); **sem** `in_flight`/`needs_follow_up`. Arms no-op em `compact.rs::drain_to_completed` e `SessionTelemetry::record_responses`. Eventos legados (`ExecCommandBegin/End`, `PatchApplyBegin/End`) e `ThreadItem::CommandExecution|FileChange|McpToolCall|DynamicToolCall` (`app-server-protocol/src/protocol/v2/item.rs:277-345`) saem de graça via `Session::send_event`.
- Novo `core/src/claude_code/tools.rs`: `PendingToolUses` (cap 256, limpo no `result`); em `tool_use` → item *started*; em frame `user` com `tool_result` → casar `tool_use_id`, ler `tool_use_result` e emitir *completed*. Mapeamento: `Bash`→`CommandExecutionItem` (stdout/stderr/interrupted; exit = `is_error?1:0`, `ExecCommandSource::Agent`, `parse_command`); `Edit/MultiEdit`→`FileChangeItem` + `AppliedPatchFileChange::Update` (aplicar old→new em `originalFile`, respeitando `replaceAll`); `Write`→`Add|Update`; `NotebookEdit`→`FileChangeItem` sem delta; `mcp__<server>__<tool>`→`McpToolCallItem`; demais (`Read/Glob/Grep/Task/TodoWrite/WebFetch/…`)→`DynamicToolCallItem{namespace:"claude"}`. History: `FunctionCall{namespace:Some("claude_code")}` + `FunctionCallOutput` truncado por `truncate_middle` (`history.rs:290-301`). Apagar `describe_tool_use`. `apply-patch/src/lib.rs:252`: `AppliedPatchDelta::from_changes` público.
- Não devolver ao Claude o próprio rastro: fingerprints em `StreamAssembler::authored` (`mod.rs:1060-1090`) + `render_item` devolve vazio para namespace `claude_code` (replay). Custo: um replay único por thread vivo no upgrade.
- Última atividade para o pai: `Session.last_activity: watch::Sender<Option<AgentActivity>>` (padrão de `agent_status`, `session/mod.rs:2266-2268`) atualizado em `send_event`; `ListedAgent.last_activity` (`agent/control.rs:104-107`); `wait_agent` no timeout devolve "…`/root/x` last ran `cargo test` 40s ago" (linha de maior alavancagem contra os 46% de aborts).
- Testes: fixtures `core/src/claude_code/fixtures/*.jsonl` (capturar uma vez com `claude -p --verbose --output-format stream-json --input-format stream-json`: `bash_success`, `bash_denied`, `edit_then_write`, `mcp_tool_call`, `permission_ask`, `usage_limit_error`, `partial_messages`); novo `translate_tests.rs` (`#[path]` como `history_tests.rs`) com snapshot da sequência de eventos; asserção em `stream_events_utils_tests.rs` de que `FunctionCall` com namespace `claude_code` nunca chega ao `ToolRouter`; `turn_tests.rs` end-to-end (provider fake → `ItemStarted/ItemCompleted(CommandExecution)` + `TurnDiff`).

### Fase 6 — bridge MCP in-process (depende de 3 + 4)
- Novo `core/src/claude_code/bridge.rs`: servidor MCP mínimo sobre `mcp_message` (`initialize` → `{protocolVersion:"2025-06-18", capabilities:{tools:{}}}`, `tools/list` ← `Prompt::tools` (`client_common.rs:25`; `ToolSpec::Function` → `{name, description, inputSchema}`), `tools/call`, `notifications/*` → ack).
- Allow-list: `collaboration/{send_message, followup_task, list_agents, wait_agent, spawn_agent, interrupt_agent}`, `update_plan`, `claude_accounts*`, todas as tools MCP da sessão (chrome, chatgpt, node_repl…). Deny: `shell`, `unified_exec`, `apply_patch`, `read_file`, `view_image`, `tool_search`.
- Dispatch pelo router real: `step_context.tool_router.dispatch_tool_call_with_code_mode_result(...)` (`tools/router.rs:204`) com `ToolCallSource::DirectPlaintextMessage` (`tools/context.rs:44`); `Semaphore(4)`; `--allowedTools mcp__codex` para não disparar `can_use_tool`.
- Testes: fake CLI com frames `initialize/tools/list/tools/call(list_agents)`; tool negada → erro JSON-RPC sem despacho.

### Fase 7 — orquestração: tools do Desktop escopadas, espera informativa, prompts
1. **`root_only_tools`** (substitui o `disabled_tools` global da Fase 0): campo em `McpServerConfig`/`RawMcpServerConfig` (`config/src/mcp_types.rs` ~:238/:355, destructuring exaustivo) e `PluginMcpServerConfig` (`config/src/types.rs:865`); `core-plugins/src/loader.rs` (`apply_plugin_mcp_server_policy` :961 e união aditiva em :1425-1476 como `disabled_tools`); `core/src/tools/spec_plan.rs::apply_mcp_tool_exposure_policy` (:180-249): se `turn_context.session_source.is_non_root_agent()` e tool ∈ `root_only_tools` → `ToolExposures::ALL` omitido (→ `Hidden`); guarda extra em `mcp_tool_call.rs` ~:148 ("available only to the root thread"). Config: `[plugins."codex-app-tools@openai-bundled".mcp_servers.codex_app] root_only_tools = ["send_message_to_thread","create_thread","fork_thread","handoff_thread","automation_update"]`. Testes em `spec_plan_tests.rs` (hidden para subagente, `Direct` para raiz, composição com `omit_tools_from`) e `merge_tests.rs`.
2. **`tool_approval_overrides`** (opcional, para usar `send_message_to_thread` da raiz sem diálogo): `HashMap<String, AppToolApproval>` lido só do config.toml do usuário, consultado primeiro em `codex-mcp/src/server.rs::tool_approval_mode` (:395); plugins nunca escrevem esse campo; branch `CODEX_APPS_MCP_SERVER_NAME` (`mcp_tool_call.rs:183-185`) intocado. Config: `[mcp_servers.codex_app.tool_approval_overrides] send_message_to_thread = "approve"`.
3. **Prompts em código, aditivos** (`core/src/session/multi_agents.rs::resolve_usage_hints` :98-129 — hoje texto configurado *substitui* o bundled): `FORK_MULTI_AGENT_V2_REPORTING_HINT_TEXT` (raiz e subagente: reportar só na resposta final ou `send_message` `..`; nunca `send_message_to_thread`; não criar/forkar threads do Desktop) e `FORK_MULTI_AGENT_V2_PATIENCE_HINT_TEXT` (só raiz: checar atividade antes de `interrupt_agent`; `wait_agent` uma vez por rodada com timeout longo). Novas chaves `features.multi_agent_v2.{root_agent,subagent}_usage_hint_suffix` (nunca substituem). Testes: `fork_reporting_hint_survives_a_configured_usage_hint_text`, `patience_hint_is_root_only`.
4. **Relógio de atividade** (`agent/registry.rs`: `last_activity: Mutex<HashMap<ThreadId, AgentActivity{at_ms,label}>>`; `agent/control.rs::record_agent_activity/agent_activity`; `session.rs::send_event` chama quando `is_non_root_agent()`; `fork_activity_label(&EventMsg)` ao lado do match exaustivo em `turn.rs:1820-1868`). Integra com a Fase 5.
5. **`list_agents` rico** (`ListedAgent` + `agent_role`, `model`, `account`, `idle_seconds`, `last_activity`; `list_agents_output_schema` em `multi_agents_spec.rs:490-517`; descrição "use before interrupting").
6. **`wait_agent`**: `targets: Option<Vec<String>>` (filtra por autor via novo `InputQueue::pending_mailbox_authors`), `agents: Vec<WaitAgentSnapshot>` no resultado, `wait_output_schema_v2` (:549-565) e descrição (:323) alinhados; defaults `MIN 15_000` / `DEFAULT 120_000` (`core/src/config/mod.rs:229-231`, espelhos em `multi_agents_common.rs:32-34`). Teste `wait_output_schema_matches_wait_agent_result` (evita spec≠impl recorrer após sync).

### Fase 8 — contas Claude: correção e visibilidade
1. **`state_file.rs`** (novo): `update<T>(path, f)`/`read<T>` com `<name>.lock` + `File::try_lock` retry (8×25 ms, como `message-history/src/lib.rs:159-180`), temp com pid **e** contador atômico, `rename` com retry (sem `remove_file`). Reescrever `accounts.rs:341-373, :684, :712, :818, :293` e `sessions.rs:67-149` como closures. Testes: 16 threads inserindo chaves distintas sem perda; arquivo nunca ausente.
2. `classify_failure` (`accounts.rs:78-105`): ler `subtype`/`error` estruturados do frame `result` antes do substring; `FailureClass::Transient` (não account-level); `warn!` no fallback.
3. `session_lost` (`mod.rs:663-668`): `--resume` só com `invalid|unknown|failed`; teste "help text mentioning --resume does not discard the session".
4. **`RateLimitSnapshot` por turno Claude**: após `result`, `get_usage` via controle (Fase 3) — fallback ao `UsageSnapshot` cacheado — e `ResponseEvent::RateLimits{primary: 5h/300 min, secondary: 7d/10080 min, limit_name: email da conta}` (`protocol.rs:2213-2262`; consumido em `turn.rs:2550-2555`). Nunca fabricar zeros.
5. **`codex account claude list|use`**: fachada `core/src/claude_accounts_api.rs` (`list(&Config)`, `select(&Config, alias)` sobre `resolve_account_alias` `mod.rs:165` e `select_account` `accounts.rs:818`); variante `Claude(ClaudeArgs)` em `cli/src/account_cmd/mod.rs:51`, render em `render.rs`. Testes em `cli/tests/account.rs` (suíte demora ~20 min de codegen — rodar destacada).
6. Conta em `list_agents`/`wait_agent` (Fase 7.5/7.6) e no `/status` da TUI (`tui/src/app/agent_status_feed.rs:173-180`).

### Fase 9 — anti-cerimônia no core
1. **Plano sobrevive à compactação**: `plan.rs:89-92` → `session.record_last_plan(args)`; `Session.last_plan: Mutex<Option<UpdatePlanArgs>>`; `compact.rs::build_compacted_history` (:644-656, após o push do summary :727-729) anexa `ContextualUserFragment` "Current plan (carried across compaction…)" com status por passo; 3 call sites; chave `tools.update_plan_survives_compaction` (default true). `plan_spec.rs:7-43`: "Keep every step that was ever in the plan; never shorten the list". Testes em `compact.rs` + `app-server/tests/suite/v2/compaction.rs`.
2. **`FORK_MULTI_AGENT_V2_DELIVERY_HINT_TEXT`** (raiz): plano é o contrato; sem auditoria/certificação/fingerprint/gate não previstos; validação = testes focados existentes + no máximo um gate amplo; IDs de task ≤ 2 níveis; não perguntar o que o plano já responde. Chave `features.multi_agent_v2.delivery_discipline_hint` (default true).
3. **Preservação de worktree como default do executor**: texto no role built-in `executor` (`core/src/agent/role.rs:384+ mod built_in`) + hint de subagente.
4. **Guarda de chats paralelos**: contador por sessão de `create_thread`/`fork_thread` bem-sucedidos em `mcp_tool_call.rs` (~:148); acima de `features.multi_agent_v2.max_desktop_threads_per_session` (default 4; 0 desliga) prefixa aviso no resultado, nunca bloqueia.
5. Compactação em thread Claude: `compact.rs:265` chamar `set_claude_code_workspace`; e curto-circuitar `run_inline_auto_compact_task` para `ClaudeCode` (o CLI compacta sozinho; se precisar de teto, `--autocompact`).

### Fase 10 — opcionais (decidir depois da 5)
- `claude_code_models.json`: `multi_agent_version: "v2"` (hoje o filho Claude recebe namespace v1) — só faz sentido junto com a bridge (Fase 6).
- `visibility: "hide"` → visível, para escolher Claude como modelo do thread raiz no Desktop/TUI (`models-manager/src/local_models.rs:56-62`, `app-server/src/models.rs:19`). Fora do pedido atual ("usado pela thread principal"), mas trivial.
- `--include-partial-messages` → `stream_event` `text_delta`/`thinking_delta` em `translate_stream` (deduplicar pelos frames `assistant` completos) para o filho parecer vivo entre células.

## Verificação end-to-end
- **Fase 0 (Desktop 26.820)**: raiz spawna `explorer` + `claude-opus` com tarefa que normalmente gera relatório de progresso → zero diálogos "Allow the codex_app MCP server…", zero cards "Enviado por ChatGPT de outra tarefa"; `grep codex_delegation` no rollout do pai não encontra tráfego subagente→pai.
- **Fase 1**: `spawn_agent(agent_type:"claude-opus", service_tier:"priority", reasoning_effort:"max", fork_turns:"3", plaintext_message:…)` sucede na 1ª chamada com 3 `notes`.
- **Fase 2**: `RUST_LOG=codex_core::claude_code=debug` mostra `turn_text` de um filho novo < 4k chars; role instruction após `## Memory` presente.
- **Fases 3–5**: no Desktop, um filho `claude-sonnet` que roda `cargo test` e edita 2 arquivos exibe células de exec com saída, diff por arquivo e `TurnDiff`; com `approval_policy=on-request` a UI de aprovação do Codex abre para o `Bash` do Claude e "negar" chega ao Claude como deny; `interrupt_agent` mata a árvore de processos; `wait_agent` expirado cita a última atividade.
- **Fase 6**: filho Claude chama `mcp__codex__send_message` → pai recebe como `InterAgentCommunication` normal (aparece em `/agents`), sem card de "outra tarefa".
- **Fase 7**: da raiz `create_thread` funciona; de um subagente a tool não existe (chamada forçada → "available only to the root thread"); `list_agents` mostra role/model/idle/conta; `wait_agent(targets:["explorer"])` não acorda com mail de outro agente.
- **Fase 8**: 8 filhos Claude simultâneos + `codex account claude list` → contas e usage intactos (repetir com 2 processos Codex); status line com `five-hour-limit`/`weekly-limit` durante turno Claude.
- **Fase 9**: sessão com `update_plan` de 6 passos + `/compact` → modelo segue em "passo 4/6"; rollout pós-compactação contém "Current plan (carried across compaction…)".
- Gates por fase: `cargo test -p codex-core --lib claude_code::`, `cargo test -p codex-core --lib tools::`, `cargo test -p codex-config`, `cargo test -p codex-core-plugins`, `cargo test -p codex-app-server --test suite -- v2::multi_agent`, `cargo test -p codex-cli --test account` (destacado), `just fmt && just clippy` (Windows: `RUST_MIN_STACK=8388608`, V8 prebuilt via `RUSTY_V8_*`). Deploy: hot-swap de `codex.exe` no vendor dir do npm (rename-while-running), reiniciar o Desktop.

## Riscos e mitigações
| risco | mitigação |
|---|---|
| `--permission-prompt-tool stdio`/`sdkMcpServers` são internos do CLI e podem mudar | feature flag + fallback automático ao caminho atual se `initialize` falhar; fixtures capturadas da versão instalada; pinar versão do CLI no doc |
| stdin aberto muda o fim de processo do CLI | `end_session` + `kill_process_tree` + idle watchdog existentes |
| formas de `tool_use_result` por tool não documentadas | mappers tolerantes (`Option`), degradam para `DynamicToolCallItem` com JSON bruto — nunca perdem a célula |
| itens executados pelo provider inflam estimativa de tokens no Codex | truncar com `MAX_TOOL_OUTPUT_CHARS` (`history.rs:23`) + pular compactação Codex em threads Claude (9.5) |
| mudança de fingerprint força um replay por thread vivo | único, limitado; cache de sessões expira em 7 dias |
| deadlock de aprovação pai↔filho | timeout no host → `Deny{interrupt:false}`; `Abort` quando o canal cai |
| conflito no sync upstream | tocar só `codex-api/src/common.rs` (1 variante), `session/turn.rs` (1 arm + attach), `apply-patch/src/lib.rs` (1 ctor), `spec_plan.rs::apply_mcp_tool_exposure_policy`, `codex-mcp/src/server.rs::tool_approval_mode`, `compact.rs::build_compacted_history`, `multi_agents_common.rs::apply_spawn_agent_service_tier`; resto em `core/src/claude_code/` (fork-only); todos com `// FORK:` e campos `#[serde(default)]` |

## Ordem recomendada
0 → 1 → 2 → 3 → 5 → 4 → 6 → 7 → 8.1 → 8.4/8.5 → 9.1 → 9.2–9.4 → 8.2/8.3 → 10. Fases 0–2 cabem em um dia e removem as falhas mais frequentes; 3+5 são o payload de visibilidade; 4+6 fecham a paridade; 7–9 são o ganho do core apontado pelas sessões.
