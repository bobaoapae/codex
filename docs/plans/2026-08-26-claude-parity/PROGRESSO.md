# Progresso da execução — plano de paridade Claude

> Diário de execução do [PLANO.md](./PLANO.md). Uma seção por fase, com o que foi feito,
> onde, e o gate que passou.

## Fase 0 — config, AGENTS.md, roles, retirada do MCP ✅

- `~/.codex/config.toml` (backup `config.toml.bak-claude-parity-20260826`):
  - `[plugins."codex-app-tools@openai-bundled".mcp_servers.codex_app] disabled_tools`
    com `send_message_to_thread`, `create_thread`, `fork_thread`, `handoff_thread`,
    `automation_update`.
  - `min_wait_timeout_ms = 15000` / `default_wait_timeout_ms = 120000` **dentro** do
    `[features.multi_agent_v2]` já existente (TOML não aceita tabela duplicada).
- `~/.codex/AGENTS.md` (backup `.bak-claude-parity-20260826`): seções do caminho MCP
  removidas; itens 0/0b viraram uma linha; nova seção "Como os agentes reportam";
  itens 8 e 9 no "Lema de execucao".
- Roles: parágrafo de reporting/Git em `executor_luna`, `executor_sol`, `tester`,
  `doc-writer` (EN) e `claude-opus`, `claude-sonnet` (PT). `explorer` intacto.
- `~/.codex/claude-agents/` renomeado para `claude-agents.retired-20260826/`
  (nenhum código Rust ou config viva o referencia).
- `docs/claude_code_agents.md`: nova seção "Reporting and orchestration".
- Comentários em `core/src/claude_code/accounts.rs` deixaram de citar o bridge.

## Fase 1 — robustez de spawn ✅

`codex-rs/core/src/tools/handlers/multi_agents_common.rs`:

- `task_fork_mode_for_wire_api` devolve `(fork_mode, Option<nota>)`.
- `validate_spawn_agent_reasoning_effort` → `clamp_spawn_agent_reasoning_effort`
  (clamp só para baixo; `Custom` e lista vazia caem no default do modelo).
- `apply_spawn_agent_service_tier` descarta com nota em vez de erro; atalho para
  `WireApi::ClaudeCode`; nota isolada em `unsupported_service_tier_note` (pura, testável).
- `notes: &mut Vec<String>` atravessa `apply_requested_spawn_agent_model_overrides`,
  `apply_spawn_agent_role` e `apply_spawn_agent_service_tier`.

Outros arquivos: `multi_agents_v2/spawn.rs` (`SpawnAgentResult.notes`),
`multi_agents/spawn.rs` (v1 coleta e descarta), `multi_agents_spec.rs`
(`spawn_agent_output_schema_v2` + descrição de `plaintext_message`),
`agent/role.rs` (nota do service tier vira "Do not pass `service_tier`"),
`claude_code/mod.rs` (`claude_effort` clampa `--effort`).

Testes: 4 novos em `multi_agents_common.rs`; 3 testes upstream de service tier
reescritos para a nova semântica (descarte, não erro); schema esperado ganhou `notes`.

Gate: `cargo test -p codex-core --lib tools::` → 376 passed, 0 failed.

## Fase 2 — contexto limpo para o filho Claude ✅

- `claude_code/history.rs`: `render_item` filtra `content` por
  `internal_chat_message_metadata_passthrough.content_item_kinds`
  (`DROPPED_CONTENT_KIND_PREFIXES`, com famílias por prefixo; `unknown` sobrevive).
  `strip_codex_harness_sections` virou fallback estreito para rollouts antigos:
  corta só até a tag de fechamento, nunca até o fim da mensagem.
- `claude_system_prompt(&ClaudeCodeWorkspace)` em `mod.rs`: protocolo de subagente +
  cwd/roots/writable/rede/permission mode + `developer_instructions` do role.
  Entregue via `initialize.appendSystemPrompt` (Fase 3).
- `[claude_code] max_fork_turns` (default 0) em `config_toml.rs` + `Config`;
  `task_fork_mode_for_wire_api` passa a clampar `LastNTurns` e a recusar `FullHistory`.

Testes: `history_tests.rs` — 3 testes de marcador substituídos por
`harness_kinds_are_dropped_but_role_instructions_survive`,
`a_role_instruction_after_the_memory_section_survives`,
`unannotated_messages_fall_back_to_tag_stripping`,
`a_realistic_bundle_leaves_a_small_turn` (< 4k chars),
`claude_own_tool_trace_is_not_replayed_to_claude`.

## Fase 3 — transporte do protocolo de controle ✅

- Novo `claude_code/control.rs`: `ControlChannel` (request/respond_success/
  respond_error/respond_tool_permission/cancel/resolve_response),
  `InboundControl`, `CanUseTool`, `ToolPermissionDecision`, `initialize_payload`.
- `mod.rs`: writer task com `mpsc::Sender<String>` (stdin aberto o turno inteiro),
  handshake `initialize` com timeout de 10 s e fallback silencioso, `end_session`
  antes de fechar stdin, arms `control_request`/`control_response` no `translate_stream`.
- `Feature::ClaudeCodeControlProtocol` (`claude_code_control_protocol`), default **on**.

Testes: `control_tests.rs` (6).

## Fase 5 — paridade de atividade de tools ✅

- `codex-api`: `ResponseEvent::ProviderExecutedTool(Box<ProviderExecutedTool>)` +
  `ProviderExecutedToolPhase` + `ProviderExecutedFileChange{,Kind}` (tipos simples,
  sem nova dependência no crate).
- `apply-patch`: `AppliedPatchDelta::from_changes` público.
- Novo `claude_code/tools.rs`: `PendingToolUses` casa `tool_use` com `tool_result`
  e emite `CommandExecutionItem` / `FileChangeItem` / `McpToolCallItem` /
  `DynamicToolCallItem`; `Edit`/`MultiEdit`/`Write` reconstroem old→new a partir de
  `originalFile`. History como `FunctionCall`/`FunctionCallOutput` no namespace
  `claude_code`; `describe_tool_use` apagado.
- `session/turn.rs`: arm que grava history, emite started/completed e alimenta o
  `turn_diff_tracker` (`provider_executed_patch_delta`) — sem `in_flight`.
- `history.rs::render_item` devolve vazio para o namespace `claude_code` (sem replay
  do próprio rastro); `StreamAssembler::send_provider_tool` fingerprinta os itens.
- Arms novos em `otel/session_telemetry.rs` e `turn_timing.rs`.

Testes: `tools_tests.rs` (9).

## Fase 4 — aprovações e mapeamento de sandbox ✅

- Novos `claude_code/host.rs` (`trait ClaudeHost`, `APPROVAL_TIMEOUT` 300 s) e
  `session_host.rs` (`SessionClaudeHost`).
- `permission_mode_for(sandbox, approval)` com a tabela de 8 combinações;
  `uses_permission_prompt`; `READ_ONLY_TOOLS`; `writable_roots`.
- `build_claude_command`: `--permission-prompt-tool stdio` só em `auto` e só com host;
  `--tools` em `plan`.
- `ClaudeCodeWorkspace` ganhou `writable_roots`, `sandbox`, `host`, `cwd_uri`.
- Wire-up: `client.rs::set_claude_code_host` chamado em `try_run_sampling_request`
  quando `wire_api == ClaudeCode`.

Testes: tabela de permission mode + flags de linha de comando em `mod.rs`;
`session_host_tests.rs` (4).

## Fase 6 — bridge MCP in-process ✅

- Novo `claude_code/bridge.rs`: `McpBridge` sobre `mcp_message`
  (`initialize`/`tools/list`/`tools/call`/`ping`), `Semaphore(4)`,
  erro de tool reportado como `isError` dentro de um result de sucesso.
- `session_host.rs`: allow-list (`BRIDGE_COLLABORATION_TOOLS`, `BRIDGE_PLAIN_TOOLS`,
  `BRIDGE_DENIED_TOOLS`), `bridge_tool_name` ⇄ `bridge_exposed_name` (round-trip),
  dispatch por `dispatch_tool_call_with_code_mode_result` com
  `ToolCallSource::DirectPlaintextMessage`.
- `initialize` declara `sdkMcpServers: ["codex"]`; `--allowedTools mcp__codex`.

Testes: `bridge_tests.rs` (5) + 2 de allow-list em `session_host_tests.rs`.

Gates até aqui: `cargo test -p codex-core --lib claude_code::` → 62 passed;
`cargo test -p codex-core --lib tools::` → 386 passed.

## Fase 7 — orquestração ✅

1. **`root_only_tools`**: campo em `McpServerConfig`/`RawMcpServerConfig`
   (`config/src/mcp_types.rs`) e `PluginMcpServerConfig` (`config/src/types.rs`);
   união aditiva em `core-plugins/src/loader.rs` (ambos os caminhos);
   `spec_plan.rs::tool_is_root_only` (puro, testável) → `ToolExposures::ALL` omitido,
   isto é `Hidden`, para `session_source.is_non_root_agent()`; backstop em
   `mcp_tool_call.rs` ("available only to the root thread") via
   `PreparedMcpCall::is_root_only`.
2. **`tool_approval_overrides`**: `HashMap<String, AppToolApproval>` em
   `McpServerConfig` (só config do usuário; `PluginMcpServerConfig` não o tem),
   consultado **primeiro** em `McpServerMetadata::tool_approval_mode`.
3. **Hints em código, aditivos** (`session/multi_agents.rs`):
   `FORK_MULTI_AGENT_V2_REPORTING_HINT_TEXT` (raiz + subagente),
   `FORK_MULTI_AGENT_V2_PATIENCE_HINT_TEXT` e
   `FORK_MULTI_AGENT_V2_DELIVERY_HINT_TEXT` (só raiz, esta última sob
   `features.multi_agent_v2.delivery_discipline_hint`, default true);
   `append_sections` + novas chaves `{root_agent,subagent}_usage_hint_suffix`.
   `without_fork_hint_sections` deixa o strip de fork reconhecer hints gravados
   por builds com seções diferentes.
4. **Relógio de atividade**: `AgentRegistry.last_activity` + `AgentActivity`;
   `AgentControl::record_agent_activity`/`agent_activity`;
   `Session.is_subagent` (cacheado) chama em `deliver_event_raw`;
   `turn.rs::fork_activity_label`.
5. **`list_agents` rico**: `ListedAgent` ganhou `agent_role`, `model`, `account`
   (via `claude_code::thread_account_label`), `last_activity`, `idle_seconds`;
   schema atualizado.
6. **`wait_agent`**: parâmetro `targets` (filtra por autor via
   `InputQueue::pending_mailbox_authors`), `agents: Vec<WaitAgentSnapshot>` no
   resultado e no schema, mensagem de timeout lista o que cada agente estava
   fazendo. Defaults `MIN 15_000` / `DEFAULT 120_000`.

Testes novos: `root_only_tools_are_hidden_from_subagents_only` (spec_plan),
`fork_hint_tests` (6, em `multi_agents.rs`),
`a_user_override_outranks_the_declared_tool_approval` (codex-mcp),
`root_only_tools` no teste de política de plugin (core-plugins).
Testes upstream ajustados: hints agora comparados por prefixo + seções;
`wait_agent` comparado por campo; schema/descrição de `wait_agent`;
nota de service tier em `role_tests`; `config.schema.json` regenerado.

Gates: `cargo test -p codex-core --lib` → 2290 passed, 3 failed (as três são
débito conhecido e não relacionado: symlink sem privilégio, e duas que passam
isoladas — flake de execução paralela). `-p codex-mcp` 195, `-p codex-config` 284,
`-p codex-core-plugins` 404 — todos verdes.

## Fase 8 — contas Claude ✅

1. **`state_file.rs`** (novo): `read`/`update` com duas camadas de exclusão —
   mutex por caminho dentro do processo (dez agentes num Codex só) e lock de
   arquivo `<name>.lock` entre processos (8×25 ms, best-effort). Temp com pid **e**
   contador atômico; `rename` sem nunca desvincular o destino. `accounts.rs`
   (`merge_into`, `record_failure`, `mark_success`, `select_account`) e
   `sessions.rs` (`store`) reescritos como closures.
2. `classify_result_failure`: lê `subtype`/`error.type` do frame `result` antes do
   substring; novo `FailureClass::Transient` (interrupt/max_turns/cancel) que
   **não** é account-level; `warn!` no fallback desconhecido.
3. `session_lost`: `--resume` sozinho não descarta mais a sessão — precisa vir com
   `invalid|unknown|failed`.
4. **`RateLimitSnapshot` por turno Claude**: `get_usage` via controle →
   `usage_snapshot_from_control`; fallback para `cached_usage`;
   `UsageSnapshot::to_rate_limit_snapshot` (5h/300min, 7d/10080min,
   `limit_name` = e-mail da conta, `limit_id = "claude_code"` para não colidir com
   a janela OpenAI). Nunca fabrica zeros: janela desconhecida fica `None`.
5. **`codex account claude list|use`**: fachada `core/src/claude_accounts_api.rs`;
   `AccountSubcommand::Claude` em `cli/src/account_cmd/mod.rs` + `render_claude_table`.
6. Conta em `list_agents`/`wait_agent` (Fase 7) e na status line via `limit_name`.

Testes: `state_file_tests.rs` (4, incluindo 16 escritores concorrentes e "o
arquivo nunca some"); `help_text_mentioning_resume_does_not_discard_the_session`.

## Fase 9 — anti-cerimônia no core ✅

1. **Plano sobrevive à compactação**: `Session.last_plan` + `record_last_plan`
   (gravado por `tools/handlers/plan.rs`); novo `context/carried_plan.rs`
   (`CarriedPlan`, kind `plan.carried_across_compaction`, mantém passos concluídos);
   `build_compacted_history_with_plan` anexa depois do summary.
   Chave `tools.update_plan.survives_compaction` (default true).
2. `FORK_MULTI_AGENT_V2_DELIVERY_HINT_TEXT` (raiz), sob
   `features.multi_agent_v2.delivery_discipline_hint` — feito na Fase 7.3.
3. Preservação de worktree: no role built-in `worker` e na seção "Git state" do
   hint de reporting.
4. **Guarda de chats paralelos**: `Session.desktop_threads_created` +
   `note_desktop_thread_count` em `mcp_tool_call.rs`; acima de
   `features.multi_agent_v2.max_desktop_threads_per_session` (default 4; 0 desliga)
   prefixa aviso no resultado — nunca bloqueia.
5. Compactação em thread Claude: `compact.rs` passou a chamar
   `set_claude_code_workspace`, e `run_inline_auto_compact_task` retorna cedo para
   `WireApi::ClaudeCode` (o CLI compacta sozinho; a compactação do Codex
   invalidaria o fingerprint e forçaria replay integral).

Testes: `a_plan_survives_compaction`, `an_empty_plan_adds_nothing`.

## Fase 10 — opcionais ✅ (com uma decisão registrada)

- `claude_code_models.json`: `multi_agent_version` v1 → **v2**. Não é cosmético:
  a bridge resolve `send_message` no namespace de colaboração v2.
- **Bug encontrado ao fazer isso**: `bridge_tool_name` fixava `"collaboration"`,
  mas `features.multi_agent_v2.tool_namespace` renomeia o namespace (a config
  deste usuário usa `collab_agents`). Corrigido com `collaboration_namespace()`
  + teste `the_bridge_follows_a_renamed_collaboration_namespace`.
- `--include-partial-messages` + arm `stream_event` → `OutputTextDelta` /
  `ReasoningContentDelta` (só pintam; os frames `assistant` completos continuam
  sendo o que constrói os itens).
- `visibility: "hide"` **mantido**: o pedido é usar Claude como subagente da
  thread principal, não como modelo da raiz. Trocar é um `sed` quando quiser.

## Correção da Fase 7.2 durante a integração

`tool_approval_overrides` só existia em `McpServerConfig`, e um
`[mcp_servers.codex_app]` avulso não tem transporte — `"invalid transport"`,
config não carrega. O campo foi adicionado também a `PluginMcpServerConfig`
(que é a política **do usuário** para o servidor de um plugin, nunca o manifesto
do plugin), e `loader.rs` o propaga nos dois caminhos.

## Config final do usuário (Fase 0 → 7.1)

`[plugins."codex-app-tools@openai-bundled".mcp_servers.codex_app]` passou de
`disabled_tools` (global, tirava as tools da raiz também) para `root_only_tools`
+ `tool_approval_overrides.send_message_to_thread = "approve"`.

## Deadlock encontrado na revisão final (e corrigido)

O desenho original da Fase 3 dizia `request()/respond_*()/cancel()`, e foi o que
implementei: `request` escrevia o frame e **esperava** a resposta. Só que a
resposta chega no stdout do CLI, e a única task que lê stdout é o
`translate_stream`. Consequência real, em três lugares:

- `initialize` era aguardado **antes** de `translate_stream` começar → ninguém
  rotearia a resposta → 10 s de timeout e fallback silencioso em **todo** turno;
- `get_usage` era aguardado **dentro** do arm `result`, isto é, dentro do próprio
  loop que entregaria a resposta → mesmo timeout;
- `end_session` era aguardado **depois** do loop → mais 10 s por turno.

Correção: `request()` virou `send_request()` (fire-and-forget, devolve o
`request_id`), `resolve_response()` devolve `ControlOutcome { request_id, result }`,
e o `translate_stream` reconhece a resposta do `initialize` pelo id — se o CLI a
recusa, o bridge é desligado para o resto do turno e o turno segue. O
`get_usage` saiu do caminho do turno: a janela de uso vem do snapshot em cache
(que a seleção de conta já atualiza por TTL), o que também evita competir com a
saída do processo logo após o `result`. `cancel()` e o parser de `get_usage`
foram removidos por ficarem sem chamador.

Isto não apareceria em teste unitário nem em `cargo check`; apareceu ao reler o
fluxo inteiro procurando exatamente por esse tipo de acoplamento.

## Gates finais

| gate | resultado |
|---|---|
| `cargo test -p codex-core --lib` | 2305 passed, 3 failed |
| `cargo test -p codex-config` | 284 passed |
| `cargo test -p codex-core-plugins` | 404 passed |
| `cargo test -p codex-mcp` | 195 passed |
| `cargo test -p codex-features` | 39 passed |
| `cargo test -p codex-app-server --test all -- v2::multi_agent` | 17 passed |
| `cargo test -p codex-cli --test account` | 4 passed |
| `cargo clippy --workspace --all-targets` | limpo |
| `cargo fmt --all -- --check` | limpo |

As 3 falhas de `codex-core` são débito conhecido e não relacionado:
`agents_md_paths_preserve_symlinked_cwd` (precisa de Developer Mode para criar
symlink) e mais duas que passam isoladas — flake de execução paralela
(`post_sampling_token_estimate_is_disabled_by_always_on_sinks`,
`blocking_snapshot_waits_for_starting_environment`). Verificadas uma a uma.

## O que falta, e por quê

- **Verificação end-to-end no Desktop** (fases 3–7 do "Verificação end-to-end" do
  plano): exige uma sessão viva com o binário novo. O código está pronto e os
  gates passam; a validação com CLI e Desktop reais é o próximo passo.
- **Deploy** (hot-swap do `codex.exe` no vendor dir do npm): substitui o binário
  que o Desktop do usuário roda. Deixado para decisão do usuário — não foi feito.

## Verificação com CLI real (`codex exec`, binário do fork)

Três turnos Claude de verdade, e cada um encontrou ou confirmou algo:

**1. read-only.** `--sandbox read-only` → `--permission-mode plan` + tool set
reduzido. O agente relatou que não tinha Bash e leu o arquivo com `Read`. Mapa da
Fase 4 confirmado.

**2. workspace-write — dois bugs encontrados, os dois corrigidos.**

- `OutputTextDelta without active item` (panic). As deltas parciais da Fase 10
  eram enviadas antes de qualquer item estar aberto. Corrigido: `push_text_delta`
  / `push_reasoning_delta` abrem o item como `push_text` faz, e `painted_via_deltas`
  impede que o bloco completo repinte o que as deltas já mostraram.
- **A tabela de permissão do plano estava errada.** `workspace-write` + `never`
  → `acceptEdits` reintroduzia exatamente a falha que o comentário do código
  original alertava: headless, `acceptEdits` aprova edições e **recusa todo o
  resto**. Observado: `Write` passou, `cat probe.txt` voltou "This command
  requires approval". Trocado por `bypassPermissions` (comportamento pré-fork).
  `acceptEdits` também não confina nada — quem confina é `--add-dir`.

  Depois da correção, o mesmo prompt produziu: célula de exec
  `bash -lc 'cat probe.txt' … succeeded: hello`, célula de patch para `made.txt`,
  e o `TurnDiff` no fim. É o critério de verificação da Fase 5, com CLI real.

**3. spawn.** `spawn_agent(agent_type:"claude-sonnet", service_tier:"priority",
reasoning_effort:"max", fork_turns:"3", plaintext_message:…)` — os três
argumentos que antes matavam a chamada — **sucesso na primeira chamada**:

```json
{"task_name":"/root/pong_probe","nickname":"Hubble","notes":[
 "`service_tier` was dropped: a Claude agent runs on the local CLI, which has no service tiers.",
 "`fork_turns` was ignored: this Claude agent starts from task-only context, so the brief must be self-contained."]}
```

Duas notas, não três: `reasoning_effort: "max"` é de fato suportado por
`claude-sonnet-5`, então nada foi ajustado — que é o comportamento correto. O
filho respondeu `PONG` pelo canal inter-agente normal, sem card do `codex_app`.

Falta verificar em sessão interativa: aprovações (`auto` + prompt tool) e a
bridge MCP (`mcp__codex__send_message` do filho), que precisam de TUI/Desktop.

## Deploy (26/08/2026 22:51)

Build de release, hot-swap no vendor dir do npm:

- `codex.exe` → `…\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\`
  (300,3 MB). Backup do anterior: `codex.pre-claude-parity-20260826-225154.exe`,
  no mesmo diretório. Restaurar = `Rename-Item` de volta.
- `codex-code-mode-host.exe` **não** foi trocado: este trabalho não o toca e o
  build de release é byte a byte igual ao que já estava lá (md5 conferido).

Verificado depois do swap: md5 do binário implantado bate com o do build, e um
turno Claude real pelo binário do vendor produziu a célula de exec com a saída
(`bash -lc 'cat probe.txt' … succeeded: hello`).

**Falta reiniciar o Codex Desktop** para que ele passe a usar o binário novo — o
processo em execução continua com o anterior (que segue no disco sob o nome de
backup, então nada quebra enquanto isso).
