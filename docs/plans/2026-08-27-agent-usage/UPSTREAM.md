# Upstream scout — sync de 2026-08-27

Estado lido em 2026-08-27 (`git fetch origin`). Investigação somente-leitura; nada foi mergeado.

| Item | Valor |
|---|---|
| Fork (local `main`) | `a20ccc428` — `feat(chatgpt_web): DOM-driven polling…` |
| Último merge de upstream | `7527c5e7c` (2026-08-26), base = `725b3a44f` |
| Upstream `origin/main` HEAD | `5f49aba87` — "Require explicit requests for spawn model overrides (#41165)", 2026-08-27 |
| Commits novos upstream | **100** (`725b3a44f..5f49aba87`), 677 arquivos, +34.619 / −3.761 |
| Commits do fork desde a base | 9 (`ac249a819` claude parity + 8× `feat(chatgpt_web)`) |
| Conflitos previstos (`git merge-tree`) | **1 arquivo** (`codex-rs/core/src/client.rs`, 2 hunks triviais) |
| Quebras de compilação previstas pós-merge | **2 arquivos do fork** (3 pontos), todos mecânicos |
| CI upstream | vermelho crônico, só-CI (mesma assinatura desde 08-11) + 2 ruídos novos, nada funcional |
| Alvo recomendado | **`5f49aba87` (HEAD upstream)** |

---

## 1. Novidades (o que vale pegar / anunciar)

Ordenadas por relevância para este fork (multi-agente com provedores `claude_code` / `chatgpt_web`).

### Multi-agente, prompts e instruções

- **`spawn_agent` só sobrescreve `model` sob pedido explícito do usuário** — #41165 (`5f49aba87`). A descrição da tool perdeu a brecha "or there is a clear task-specific reason". Só toca 1 linha em `multi_agents_spec.rs` e o teste `spawn_agent_description.rs`; não colide com os blocos FORK (`plaintext_message`, `account`, `notes`).
- **Modo persistente** (`model_reasoning_effort = "persistent"`) — #40799 (`3e4707b34`), #41050 (`f1433fc71`), #40942 (`0e9a2bae5`). Novo valor de esforço que vai na wire como `disabled`; quando ativo, o core injeta como *world state* um template de "proactivity / follow-up" (`codex-rs/core/templates/persistent_mode.md`: continuar após a resposta final, não duplicar mensagens entre `send_user_message_async` e `final`, cadência de polling 1–3 min, não ampliar escopo sem aprovação) e liga por padrão o reminder de hora + `clock.sleep`. **Para o fork:** `ModelMessages` ganhou `persistent_instructions: Option<String>` — o catálogo local (`claude_code_models.json` / `chatgpt_web_models.json`) pode sobrescrever ou desligar esse texto para os modelos Claude/ChatGPT-web; e `agent/control/spawn.rs` agora reconhece `PersistentModeState::matches_text` ao herdar histórico em forks (bloco onde o fork também mexe).
- **Descrição de `send_user_message_async` reescrita** — #41070 (`218a3e50a`): quando usar (pergunta, bloqueio, achado que muda a direção, resposta a status) vs. commentary, e deixa explícito que não encerra o turno nem espera resposta.
- **Orçamento de tokens resolvido pelo modelo ativo de cada step** — #41162 (`8228e9b86`): `ModelInfo::usable_context_window()`, guidance de janela de contexto trocada/retirada quando o modelo muda no meio do turno. Relevante porque agentes Claude/ChatGPT-web têm janelas diferentes do modelo pai.
- **Metadados de turno com `window_number` e `forked_from_ordinal_exclusive`** — #40987 (`0d654e653`); subagentes que herdam contexto reportam `parent_thread_id` sem emitir linhagem de fork. Toca `session/session.rs`, `session/mod.rs` e `rollout` — vizinho do `fork_turns` do fork, mas mergeia limpo.
- **`turn/start` aceita `toolOutput` autônomo** (function_call_output sem `call_id`) — #41002 (`e56e4922e`) + #40991 (`b9c4b9a0c`); novo `TurnInput::FunctionCallOutput`, `has_user_input()` virou `has_pending_input()` em `input_queue.rs` (arquivo que o fork também altera; merge limpo). O TUI passa a delegar `create_thread`/`send_message_to_thread` com autoridade de tool em vez de input de usuário — #41046 (`72c96598c`). É a mesma mecânica dos "cards de delegação" do Desktop.
- **Skills invocadas pelo usuário no turno raiz são confiadas nos workers delegados** (Guardian) — #41118 (`694edc23b`).
- **Assinatura de `ToolHandler::handle` mudou** — #41020 (`81e180044`): `fn handle<'a>(&'a self, invocation) -> ToolExecutorFuture<'a> where Invocation: 'a`. 39 handlers upstream atualizados; **o handler `claude_accounts.rs` do fork precisa do mesmo ajuste** (ver §3).

### Modelos / catálogo / cliente

- `ReasoningEffort::Persistent` no protocolo e no SDK TS (#40799); `ModelMessages.persistent_instructions` (#41050) e `ModelMessages.confirmation_policies { browser_use, computer_use }` (#41072, `b592a0bfe`, encaminhadas às MCP tools de ator). Nenhum modelo novo em `models.json` neste intervalo.
- **Responses Lite com IDs estáveis de prefixo** — #40962 (`e77773085`): ids v5 derivados de `thread_id` + payload, para follow-ups WebSocket incrementais.
- **Erro de rate-limit em streaming classificado** (`rateLimitExceeded`, retryable, preserva `retry-after`) — #40931 (`e0c727de0`).
- **`usage_metadata.amount` exposto** em `rawResponse/completed` — #41087 (`2c4a95736`); passa por `client.rs`, `compact.rs`, `session/turn.rs` (arquivos que o fork toca; merge limpo).
- Endpoints Responses reais nos spans de tracing — #40906 (`bde9db137`).

### Config / features (`config.schema.json` +134 linhas)

- `features.guardianv2.free_guardian` (roteia Guardian para `/guardian` e `/guardian-classifier` sem cobrar) — #40892 (`62fb56ee5`). Adiciona `ModelProviderInfo::supports_codex_backend_routes()` e `ModelClient.free_guardian_enabled` (**é este campo que conflita com o fork em `client.rs`**).
- `features.guardianv2.persist_scores` (opt-in, grava scores nos rollouts) — #40911 (`5b92c2d2f`).
- Guardian v2 agora revisa por padrão só computer-use e inclui imagens no transcript (`review_scope.computer_use_only`, `transcript.include_images`) — #40846 (`9dea1f709`).
- `features.write_stdin_approval` (novo flag, off) — aprovação para escrever em terminal escalado — #40978 (`a57b39835`). Muda `request_command_approval` (**afeta `claude_code/session_host.rs` do fork**, ver §3).
- `compaction_image_budget` promovido a **Stable / ligado por padrão** — #40994 (`528fd7ace`).
- Plugins respeitam config em camadas (system + projeto confiável), cache LRU de 8 dirs — #40954 (`6ac012a0d`).
- Workspace roots resolvidos pelo ambiente entram em `EnvironmentConfig` — #40912 (`7625bd566`); `resolve_permission_profile`/`compile_permission_profile` exportados — #40989.
- Keymap Vim: `gg`/`G` (#40958) e `f`/`F`/`t`/`T` (#40785) com entradas configuráveis.

### TUI

- **`/recap`** manual + recap automático de conversas ociosas — #40705 (`6988d390b`), #40697, #40696.
- Preview inline de diff limitado a 12 linhas (conteúdo completo no transcript) — #41143 (`a847c71a1`).
- Hyperlinks OSC 8 preservados em linhas quebradas do composer — #40720; estado do overlay de transcript preservado — #40751; detalhes de "misalignment" retomáveis via app-server — #40952.

### MCP / plugins / skills

- OAuth enterprise: resolução de identidade IdP + troca ID-JAG por bearer MCP — #40739, #40722.
- Trusted access context para MCP (metadata, plugin MCP elegível, Guardian) — #40992, #41005, #40982; provenance para extensões — #40976; roots de plugin congelados na atribuição — #41117; revisão síncrona obrigatória para ações MCP sensíveis — #41094.
- Saída de tool MCP preservada como content items — #40737; permissões de attachment — #40728; ids de item de origem na metadata — #40866.
- History/notes: argumentos sensíveis marcados como encriptados (`x-openai-encrypted-tool-arguments`) — #41041; compatíveis com Bridge — #40775; políticas de truncamento encaminhadas — #41062.
- Catálogo de skills com aliases de path para encolher o prompt — #41011; telemetria de skill com atribuição de plugin — #40724.

### Sandbox / segurança

- Handoff de listener do proxy gerenciado por socketpair anônimo (Linux) — #40999; policy de filesystem URI-native (`PathUri`) — #41001; scratch macOS só para processos — #40961; limpeza do helper do sandbox Windows endurecida — #40808; telemetria de scan world-writable no Windows — #40983.
- Credenciais removidas de URLs de remote Git na metadata — #40713; chaves Bedrock redigidas no debug — #40706; limite de tamanho para input revisado de terminal — #41159; aprovações usam as settings do step que emitiu a ação — #40821.
- Worktrees gerenciados ganham `codex-thread.json` com dono — #40716.

### Code mode / exec-server / infra

- OpenTelemetry no code-mode host (#40760) e contexto de trace via gRPC (#41017); completude de metadata das chamadas (#41058); plugins de browser bundlados podem rodar hook de cleanup (#40993).
- exec-server: refresh explícito da conexão remota (#40710), testes em ambiente sandboxed (#40717), teste estável apontando para **Codex 0.150.1** (#41030 — i.e. upstream publicou 0.150.x neste intervalo).
- Bazel: repositórios pinados de releases (#40718), compat tests do exec-server (#40736), debuginfo (#40864).

Sem `CHANGELOG.md` upstream neste range (o arquivo tem 94 bytes) e sem novos subcomandos de CLI (`cli/src/main.rs` inalterado; só `doctor.rs`, `marketplace_cmd.rs`, `plugin_cmd.rs` tocados por #40954).

---

## 2. Commits por área (100)

Legenda: **[fork]** = toca arquivo/área que o fork também altera.

### Multi-agente / roles / delegação (8)
- `5f49aba87` #41165 — spawn_agent: `model` só sob pedido explícito. **[fork]** `multi_agents_spec.rs` (1 linha, fora dos blocos FORK).
- `694edc23b` #41118 — skills confiadas do turno raiz propagadas a workers delegados (evidência Guardian). **[fork]** `agent/control/user_authorization.rs`, `codex_thread.rs`.
- `72c96598c` #41046 — prompts de delegação do TUI mantêm autoridade de tool.
- `0d654e653` #40987 — `window_number` + `forked_from_ordinal_exclusive` na turn metadata; `parent_thread_id` para subagentes com contexto herdado. **[fork]** `session/session.rs`, `session/mod.rs`.
- `e56e4922e` #41002 / `b9c4b9a0c` #40991 — `toolOutput` autônomo em `turn/start` / roteamento. **[fork]** `session/turn.rs`, `session/input_queue.rs`.
- `218a3e50a` #41070 — descrição de `send_user_message_async`.
- `81e180044` #41020 — capacidades de extensão escopadas ao lifetime da invocação → nova assinatura de `ToolHandler::handle`. **[fork]** todos os handlers `multi_agents*/`, `plan.rs`, `spec_plan.rs` (merge limpo) + **`claude_accounts.rs` (quebra, só-fork)**.
- `8228e9b86` #41162 — token budget por modelo do step. **[fork]** `session/mod.rs`, `turn_context.rs`.

### Prompts / instruções / templates (5)
- `f1433fc71` #41050 — instruções de desenvolvedor do modo persistente (template + world state). **[fork]** `agent/control/spawn.rs` (herança em fork), `models-manager/model_info.rs`.
- `3e4707b34` #40799 — `ReasoningEffort::Persistent`. **[fork]** `client.rs` (merge limpo nesse hunk).
- `0e9a2bae5` #40942 — clock tools/reminder ligados no modo persistente.
- `7c3747941` #41011 — aliases de path no catálogo de skills.
- `5ca417529` #40709 — payload de instruções do host renomeado `UserInstructions` → `Instructions`. **[fork]** `session/mod.rs`/`session.rs` (sem uso no código do fork).

### Modelos / catálogo / cliente (6)
- `b592a0bfe` #41072 — `confirmation_policies` no `ModelMessages`, encaminhadas às MCP tools de ator.
- `e77773085` #40962 — IDs estáveis do prefixo Responses Lite. **[fork]** `client.rs`.
- `2c4a95736` #41087 — `usage_metadata` em eventos de conclusão. **[fork]** `client.rs`, `compact.rs`, `session/turn.rs`, `codex-api/common.rs`.
- `e0c727de0` #40931 — classificação de rate-limit em streaming.
- `bde9db137` #40906 — endpoint real nos spans. **[fork]** `client.rs`.
- `daa3eaf10` #40967 — scoring Guardian para modelos computer-use obrigatórios; `02c9f83f7` #40735 — accessor de model info na telemetria de skills.

### Config / features (7)
- `62fb56ee5` #40892 — `free_guardian` + `supports_codex_backend_routes()`. **[fork] CONFLITO em `client.rs`**; toca `model-provider-info/src/lib.rs` (merge limpo, sem novo `match WireApi`).
- `5b92c2d2f` #40911 — `persist_scores`.
- `9dea1f709` #40846 — defaults Guardian v2 (computer-use + imagens).
- `a57b39835` #40978 — `write_stdin_approval`; `request_command_approval(kind: ExecApprovalKind, …, cwd: PathUri)`. **[fork] quebra `claude_code/session_host.rs`**.
- `528fd7ace` #40994 — `compaction_image_budget` Stable/on. **[fork]** `features/src/lib.rs` (merge limpo).
- `6ac012a0d` #40954 — plugins com config em camadas. **[fork]** `core-plugins/loader.rs`.
- `7625bd566` #40912 — workspace roots do ambiente; `6e008417b` #40989 — resolução de permission profile na core API. **[fork]** `agent/control/spawn.rs`, `session/session.rs`.

### Guardian V2 (20) — nenhum toca código do fork além dos já listados
`453a9bcc6` #41158 lag padrão menor · `4f2a1d866` #41152 fail-closed em compactações do pai sem limite · `5ed334a29` #41151 render de ações em módulo · `e8b938b02` #41146 outcomes tipados · `6c59264b1` #41108 timeouts de teste · `e9a446d79` #41100 métricas de decisão · `89650c66f` #41094 revisão síncrona p/ MCP sensível · `307ce6cda` #41023 analytics do reviewer · `b68acc4d4` #41006 skills invocadas confiadas · `f74bcd281` #40964 prompts de revisão síncrona · `102ae5e2e` #40985 prewarm de WebSocket sem bloquear · `d61ba72f2` #40982 contexto confiável p/ MCP configurado · `d4998d611` #40901 ações revisadas com risk score · `10d5a603a` #40884 persistir sem restaurar scores · `039eb58a0` #40848 histórico do pai read-only p/ reviewer · `a9ed4f154` #40844 classificação preditiva · `dc08ace78` #40742 sessões isoladas de reviewer · `bce96bcb4` #41159 input de terminal revisado limitado · `a26f1806a` #40821 settings do step emissor nas aprovações · `e24190caa` #40771 env do turno no sandbox.

### Tools: exec / MCP / code-mode / skills / plugins (24)
- MCP: `aa89cf62b` #41117 · `ae357e725` #41005 · `a98b94625` #40992 · `21ff2e802` #40976 · `f5420174d` #40866 · `04907ab95` #40807 · `75cb7c903` #40737 · `4213b38f3` #40728 · `9b4a0f8a0` #40739 · `f6805328c` #40722 · `00b7152a6` #40748 · `ac644ed11` #40966 / `42624fd63` #40719 (bounds em schemas). **[fork]** `codex-mcp/*`, `mcp_tool_call.rs`, `config/mcp_types.rs` — merge limpo.
- History/notes: `57e2edc6e` #41041 · `25a6e316c` #40775 · `4cb8d8679` #41062 · `37f4bb94c` #40787.
- Code mode: `d5caceccb` #41058 · `eed1dee69` #41017 · `3ba7b6941` #40760 · `0340e12f5` #40993.
- Skills: `df9f537a6` #41150 · `c51e7b373` #40724.
- exec-server: `eb49f491c` #40710 · `32fd05631` #40717 · `399be2d6b` #40712 · `5af697998` #41030 · `07d260c62` #40979.

### TUI / app-server (9)
`a847c71a1` #41143 · `7276d6708` #40952 · `2764e8362` #40751 · `d6174a879` #40720 · `98ee29c7c` #40785 · `d47e5cc0e` #40958 · `6988d390b` #40705 · `40ba7da7b` #40697 · `7c1e36c23` #40696.

### Sandbox / segurança (9)
`f3741880f` #40999 · `292601407` #41001 · `7f8239736` #40961 · `21c58c90f` #40808 · `37a514982` #40983 · `1bc02aea5` #40713 · `13fe2bcb7` #40706 (**[fork]** `login/auth/bedrock_api_key.rs` — o `manager.rs` com o bloco FORK não foi tocado) · `23cedf480` #40716 · `bce96bcb4` #41159.

### CI / Bazel / telemetria / misc (7)
`74772623d` #40864 · `de70ec840` #40718 · `62aacbb2c` #40736 · `62bfa41a8` #40723 · `0b94751cc` #40726 · `346c4db7c` #40714 · `5af697998`/`07d260c62` (bump do teste estável).

### Áreas do fork NÃO tocadas pelo upstream neste range
`context/multi_agent_role_instructions.rs`, `agent/role.rs`, `agent/registry.rs`, `agent/control.rs`, `session/multi_agents.rs`, `config/config_toml.rs`, `config/types.rs`, `models-manager/local_models.rs`, `models-manager/manager.rs`, `login/auth/manager.rs` (bloco FORK vault), `core/config/mod.rs` só ganhou métodos (nenhum campo novo em `Config` → `thread-manager-sample` continua compilando). Nenhum `match` exaustivo novo sobre `WireApi` (a classe de quebra E0004 recorrente não se aplica).

---

## 3. Conflitos previstos e regra de resolução

`git merge-tree --write-tree HEAD origin/main` → exit 1, **um único arquivo em conflito**.

### 3.1 `codex-rs/core/src/client.rs` (conflito textual, 2 hunks)

Ambos os lados acrescentam campos no mesmo ponto de `struct ModelClient` e do construtor `ModelClient::new`:

```
<<<<<<< HEAD
    /// Workspace for the `claude_code` provider; unused by every other wire API.
    claude_code_workspace: Option<ClaudeCodeWorkspace>,
    /// FORK: workspace for the `chatgpt_web` provider; likewise unused elsewhere.
    chatgpt_web_workspace: Option<ChatGptWebWorkspace>,
=======
    free_guardian_enabled: bool,
>>>>>>> origin/main
```
e, no construtor, `claude_code_workspace: None, chatgpt_web_workspace: None` vs `free_guardian_enabled: false`.

**Regra:** manter os dois lados (append-append) — campos do fork seguidos de `free_guardian_enabled: bool` / `false`. Upstream: #40892 (`62fb56ee5`). Fork: `aa952f75e`, `d5299a688`, `ac249a819`, `b2d54bf8f`.

O restante do diff upstream em `client.rs` (+120/−42: `ResponsesEndpoint`, `responses_endpoint()`, `uses_codex_backend()`, `connect_websocket(.., endpoint)`, IDs v5 do prefixo Lite, `usage_metadata`, `Persistent → "disabled"`) mergeia limpo e não cruza o `match` do fork em `stream()` (`WireApi::ClaudeCode` / `WireApi::ChatGptWeb`, ~linha 1995), que fica antes/fora dos caminhos Responses alterados.

### 3.2 Quebras de compilação previstas (merge limpo, mas API mudou) — ambas só-fork

1. **`codex-rs/core/src/tools/handlers/claude_accounts.rs:149` e `:212`** — dois `impl ToolHandler` com a assinatura antiga `fn handle(&self, invocation: ToolInvocation) -> ToolExecutorFuture<'_>`. Upstream #41020 mudou o trait para `fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a> where ToolInvocation: 'a;` → E0195 até ajustar. Os outros handlers que o fork altera (`multi_agents/spawn.rs`, `multi_agents_v2/{spawn,wait,send_message,followup_task}.rs`, `plan.rs`, `spec_plan.rs`) são atualizados pelo próprio merge.
2. **`codex-rs/core/src/claude_code/session_host.rs:87`** — chama `Session::request_command_approval(turn_context, call_id, None, None, command, cwd, …)`. Upstream (#40978 + #41001) inseriu `kind: ExecApprovalKind` como 2º argumento e trocou `cwd: AbsolutePathBuf` por `cwd: PathUri`. Correção: passar `ExecApprovalKind::Command` e `cwd.into()` (ou `PathUri::from_abs_path(&cwd)`), como faz `tools/approvals.rs` upstream. `request_patch_approval` (linha 112) não mudou.

Risco residual baixo: `session/session.rs` upstream reescreveu `materialized_permission_profile`/`effective_permission_profile` e `current_window()` passou a devolver tripla — o fork só chama `with_claude_code_workspace`/`with_chatgpt_web_workspace` (linha ~1426) e `archive_chatgpt_web_conversation` (~1604) nesse arquivo, sem depender dessas funções. `has_user_input()` → `has_pending_input()` em `input_queue.rs` não é usado por código do fork.

---

## 4. Estado do CI upstream e alvo de merge

Check-runs contados com `gh api --paginate …/check-runs?per_page=100` (obrigatório — commits têm 100–500+ runs):

| sha | PR | falhas | total |
|---|---|---|---|
| `5f49aba87` (HEAD) | #41165 | 34 | 196 |
| `8228e9b86` | #41162 | 31 | 160 |
| `bce96bcb4` | #41159 | 32 | 139 |
| `453a9bcc6` | #41158 | 32 | 110 |
| `4f2a1d866` | #41152 | 32 | 155 |
| `5ed334a29` | #41151 | 32 | 117 |
| `df9f537a6` | #41150 | 32 | 108 |
| `e8b938b02` | #41146 | 35 | 128 |
| `a847c71a1` | #41143 | 34 | 143 |
| `694edc23b` | #41118 | 34 | 516 |
| `aa89cf62b` | #41117 | 32 | 106 |
| `6c59264b1` | #41108 | 32 | 229 |

Uniformemente vermelho, sem commit verde — **é a assinatura crônica só-CI** já catalogada na memória (`codex-fork-sync-workflow`), reverificada contra a fonte de `origin/main` e os logs de HEAD:

1. `use wiremock::matchers::body_json;` **continua** em `codex-rs/core/tests/suite/openai_file_mcp.rs:47` → `error: unused import` sob `-D warnings` → derruba todos os `rust-ci-full / Lint/Build` (6), `Build nextest archive` (5), `Platform result` (5), `Bazel clippy` (2×2) e `Full CI results`. Log do job 98570267901 confirma `could not compile codex-core (test "all") due to 1 previous error`. Builds locais não afetados (`[workspace.lints] rust = {}`).
2. Entrada obsoleta `"codex-rs/code-mode/Cargo.toml"` **continua** em `MANIFEST_FEATURE_EXCEPTIONS` (`.github/scripts/verify_cargo_workspace_manifests.py:29`) e o crate segue sem `[features]` → `repo-checks / build-test` (log 98569914366: "remove the stale `[features]` exception").
3. `sdk / sdks` (jest TypeScript) — irrelevante para Rust.
4. `Bazel test on windows-latest (native main)` — crônico.

Ruídos novos, também só-CI:
- `Codespell` — `codex-rs/exec-server/src/no_follow/unix.rs:130: WRONLY ==> WRONGLY` (falso positivo da flag `O_WRONLY`), presente em todos os commits do range.
- `rust-ci / Argument comment lint - Linux` (+ `rust-ci / CI results (required)`) só em HEAD: download do Bazel 9.0.0 devolveu HTTP 403 → infra, não código. Explica os 34 vs 31–32.

**Alvo recomendado: `5f49aba87` (HEAD upstream).** Não há commit verde para escolher; o vermelho é idêntico ao que já mergeamos em 08-15 e 08-17 e nenhuma causa é funcional. Pegar HEAD leva #41165 (spawn model) e #41162 (token budget por step), ambos úteis para o fork. Gate real, como sempre: `cargo check --workspace --all-targets` (com override rusty_v8) + testes com `RUST_MIN_STACK=8388608`.

---

## 5. Esforço estimado do sync

| Etapa | Estimativa |
|---|---|
| `git merge origin/main` + resolver `client.rs` (keep both) | 5 min |
| Ajustar `claude_accounts.rs` (2 assinaturas) e `session_host.rs` (`ExecApprovalKind::Command`, `PathUri`) | 10–15 min |
| `cargo check --workspace --all-targets` (1ª passada + possíveis restos: E0195/E0061 óbvios) | 10–20 min |
| Testes direcionados: `codex-core` (`claude_code`, `chatgpt_web`, `multi_agents`, `spawn_agent_description`), `codex-models-manager`, `codex-cli` sem a suíte `account` | 15–30 min |
| Build release dos 4 bins + hot-swap no vendor do npm | ~25–30 min (quase todo tempo de máquina) |
| **Total** | **~1h15–1h45 de relógio, ~30 min de atenção** |

Pontos a validar depois do merge com um turno real (não só compilação):
- Agente Claude (`model_provider = claude_code`) com aprovação de comando — passa pelo `request_command_approval` alterado.
- `spawn_agent` v2 com `plaintext_message`/`account` — o teste upstream `spawn_agent_description.rs` mudou a frase de `model`; confirmar que as asserções do fork em `multi_agents_spec_tests.rs` ainda batem.
- Se for testar `model_reasoning_effort = "persistent"` com modelos Claude/ChatGPT-web, decidir se `persistent_instructions` deve ser `null`/custom no catálogo local — o template padrão referencia `functions.send_user_message_async` e `clock.sleep`, que podem não existir nesses provedores.
- Após o sync, atualizar a memória `codex-fork-sync-workflow` (nova classe de quebra: assinatura de `ToolHandler::handle` em handlers só-fork; `request_command_approval` com `ExecApprovalKind`/`PathUri`).

---

## Sync 2026-08-27 — executado

- Merge de `origin/main` `5f49aba87` → commit **`93b8c85af`** ("Merge remote-tracking branch 'origin/main'"); único conflito textual em `codex-rs/core/src/client.rs` (append-append: campos `claude_code_workspace`/`chatgpt_web_workspace` do fork + `free_guardian_enabled` do upstream — mantidos os dois lados, struct e construtor).
- Adaptações em **`f018b8f30`** `fix(fork): adapt fork code to upstream 5f49aba87`:
  1. `tools/handlers/claude_accounts.rs` — `ToolHandler::handle<'a>(&'a self, …) -> ToolExecutorFuture<'a> where ToolInvocation: 'a` (#41020), nos dois handlers.
  2. `claude_code/session_host.rs` — `request_command_approval(turn, ExecApprovalKind::Command, …, cwd.into())` (#40978 / #41001).
  3. `claude_code/mod.rs` e `chatgpt_web/mod.rs` — `ResponseEvent::Completed { …, usage_metadata: None }` (#41087), 5 pontos.
  4. `ReasoningEffort::Persistent` (#40799) tratado como `Custom` (sem profundidade) em `claude_effort` e `reasoning_effort_rank` (`multi_agents_common.rs`).
  5. `codex-mcp/src/connection_manager_tests.rs` (teste do fork) — `McpServerMetadata { tool_approval_overrides, root_only_tools }` novos campos.
- Bundles `claude_code_models.json` / `chatgpt_web_models.json`: **sem alteração** — `ModelMessages.persistent_instructions` é `Option` com default e os bundles parseiam (models-manager 54/54). O template do modo persistente só é injetado com `model_reasoning_effort = "persistent"`, que nenhuma linha local anuncia; se um dia for usado com esses provedores, definir `persistent_instructions` no bundle (o default cita `send_user_message_async`/`clock.sleep`).
- `spawn_agent` (#41165): a nova frase "set `model` only when an explicit override is needed" mergeou fora dos blocos FORK (`notes`, `plaintext_message`, `account`); `agent/role.rs` não foi tocado pelo upstream.
- Gates (`RUST_MIN_STACK=8388608`): `cargo check --workspace --all-targets` verde; clippy core/cli/config/models-manager/model-provider-info `--lib --bins --tests` **0 warnings**; model-provider-info 30/30; models-manager 54/54; config 284/284; `codex-core --lib` **2595 passed / 3 failed / 11 ignored** — as 3 são as pré-existentes (`agents_md_paths_preserve_symlinked_cwd` privilégio de symlink; `environment_selection::blocking_snapshot_waits_for_starting_environment` e `session::turn::tests::post_sampling_token_estimate_is_disabled_by_always_on_sinks` flakes de execução paralela); `--test all -- chatgpt_web_connector` 4/4; `codex-cli --lib` 13/13; `codex-app-server --test all -- v2::multi_agent` 17/17.
- Não feito aqui: build release + hot-swap (fica para depois da revisão de config). Validar com um turno real: agente Claude com aprovação de comando (passa pelo `request_command_approval` alterado) e `spawn_agent` v2 com `plaintext_message`.
- Nota operacional: `just fmt` roda o buildifier e reescreve todos os `.bazel/.bzl/justfile` para LF; com `core.autocrlf=true` isso deixa modificações "fantasma" que `git checkout --` não limpa (conteúdo normalizado igual) — reconverter para CRLF (`sed -i 's/\r$//; s/$/\r/'`) ou ignorar.
