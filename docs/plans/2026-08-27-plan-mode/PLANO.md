# Plano — melhorar o Plan mode do Codex (fork): P1–P6

## Context

A análise de 27/08 (`docs/plans/2026-08-27-plan-mode/ANALISE.md`) mostrou que o Plan mode do
Codex pergunta menos e detalha menos do que o do Claude Code por desenho do harness, não do
modelo: o prompt `plan.md` foi aparado pelo upstream («concise by default», «≤3 paths», «If
unanswered, proceed with the recommended option»), a tool `request_user_input` está limitada a
1–3 perguntas/2–3 opções, as instruções entram uma única vez no contexto, o TUI abre
«Implement this plan?» ao primeiro `<proposed_plan>` sem caminho de revisão, o preset de Plan
fixa esforço Medium, e o plano morre com a sessão (sem ficheiro). Na simulação, o Codex tomou
48 decisões sem perguntar e só as converteu em perguntas depois de um empurrão explícito.

Objetivo: fechar estas lacunas no fork com o mínimo de toque em ficheiros quentes do upstream,
mantendo TUI e Desktop (app-server) cobertos, e entregar código + testes + build + hot-swap
(sem commit).

Decisões confirmadas com o dono (27/08):

| # | Decisão |
|---|---|
| Âmbito | P1–P6 neste plano (P7 `codex exec --plan` fica fora) |
| P1 entrega | novo builtin no fork **e** override opcional `$CODEX_HOME/plan_mode.md` |
| P1 checkpoint | obrigatório: antes do 1.º `<proposed_plan>` do thread, perguntar toda a preferência/tradeoff resolvida por assunção (em lote, recomendada primeiro); só decisões técnicas derivadas do código ficam como assunções |
| Entrega | código + testes dos crates tocados + `cargo build --release` + hot-swap do `codex.exe`; **sem commit** |
| P6 ficheiros | um por thread; revisões reescrevem (`revision++`, `updated_at`); corpo idêntico não escreve |
| P6 metadados | YAML front matter (`title, thread_id, turn_id, cwd, model, created_at, updated_at, revision`) |
| P6 carregar | contexto oculto (delimitador do IDE context) + 3 ações: Implement / Attach to next message / **Revise in Plan mode** |
| P6 app-server | `plan/list` + `plan/read` **estáveis** (sem `experimentalApi`), marcados «Fork extension» |

## P1 — Template de Plan mode do fork (+ override por ficheiro)

### P1.a Texto (edições a `codex-rs/collaboration-mode-templates/templates/plan.md`)

Manter tudo o que é regra de modo (mode rules, mutação, «Two kinds of unknowns», formato do
bloco `<proposed_plan>`). Mudanças, em inglês (é o modelo que lê):

1. **Após «PHASE 3 — Implementation chat»**, nova secção:
   ```
   ## PHASE 4 — Review (before you finalize)
   * Re-read the critical files you identified; confirm every path, symbol and line you intend
     to cite in the plan against the code, not from memory.
   * Check the plan against the user's original request: goal, in/out of scope, constraints.
   * Ask the user any remaining preference questions now (see Decision checkpoint).
   ```
2. **Substituir** a frase «If unanswered, proceed with the recommended option and record it as an
   assumption in the final plan.» (secção «Two kinds of unknowns», item 2) por:
   ```
   * Only technical choices fully determined by the code may be recorded as assumptions.
   ```
   e acrescentar, depois de «## Asking questions», a secção:
   ```
   ## Decision checkpoint (mandatory before the first plan)

   Before emitting the first `<proposed_plan>` of a thread, list privately every
   preference/tradeoff you resolved by assumption (naming, retention, visibility, API
   stability, UX behavior, scope boundaries, anything a product owner could reasonably
   decide differently). If there is at least one, you MUST call `request_user_input` with the
   highest-impact ones first (batch related decisions in one call, recommended option first,
   at most 4 per call; repeat if more remain). Only when zero such decisions remain may you
   emit the plan. Decisions the user already answered never get re-asked; unanswered
   secondary ones are listed under "Assumptions" with the alternative you rejected.
   ```
3. **Regra de fim de turno**, acrescentar em «## Asking questions»:
   ```
   * A Plan-mode turn ends only with (a) a `request_user_input` call, (b) a `<proposed_plan>`
     block, or (c) a direct answer to a simple question. Never end a turn with prose that
     describes what you would ask or do next.
   ```
4. **Formato do plano** — substituir os três parágrafos «concise by default … If the user asks
   for more detail, then expand.» por:
   ```
   plan content should be human and agent digestible. The final plan is plan-only and includes:

   * A clear title and a short **Context** section (the problem, what prompted it, the intended
     outcome).
   * **Decisions**: the user-confirmed decisions, then technical assumptions each with the
     rejected alternative in one line.
   * **Implementation**, grouped by subsystem/behavior in execution order. Name the critical
     files and symbols to modify with their paths (`crate/src/file.rs:line` when you verified
     the line); for a pattern repeated across many files describe it once and list 2–3
     representative paths. Reference existing functions/utilities to reuse, with paths.
   * Important changes or additions to public APIs/interfaces/types (signatures, wire shapes).
   * **Verification**: how to test end-to-end (commands, expected observable results) plus the
     unit/integration tests to add or update, with their files.
   * **Assumptions and risks**, including anything deferred out of scope.

   Prefer precise, verified references over prose. Compress unaffected behavior and
   repeated repo facts; do not enumerate every file or line when a pattern description
   suffices. Expand detail wherever an implementer could otherwise make a wrong choice.
   ```
5. **Multi-agente** (nova secção antes de «## Finalization rule»):
   ```
   ## Using sub-agents while planning

   When sub-agent tools are available, use them for exploration (parallel read-only
   explorers, each with a distinct focus) and, for non-trivial designs, for 2–3 independent
   design perspectives (e.g. minimal change vs clean architecture). Give each agent the
   requirements and the paths you already found. Verify their claims against the code before
   relying on them; sub-agents never call `request_user_input` — you do.
   ```
6. Manter «Do not ask "should I proceed?"» e «at most one `<proposed_plan>` block per turn».

Atualizar os testes que fixam o texto: `codex-rs/models-manager/src/collaboration_mode_presets_tests.rs`
(compara `developer_instructions` com a constante `PLAN` — continua a passar por ser a mesma
constante), e correr `cargo test -p codex-core collaboration` (testes de
`world_state/collaboration_mode_tests.rs` usam strings próprias).

### P1.b Override opcional `$CODEX_HOME/plan_mode.md`

`builtin_collaboration_mode_presets()` é pura (só `include_str!`) e `codex-models-manager` não
conhece o codex home. Os dois consumidores que têm `Config` em scope são o ponto certo:

- `codex-rs/models-manager/src/collaboration_mode_presets.rs`: nova função pública
  `plan_mode_instructions(codex_home: Option<&Path>) -> String` — lê `codex_home/plan_mode.md`
  (`std::fs::read_to_string`, sync; ficheiro pequeno, lido só ao construir a máscara); conteúdo
  `trim()` não vazio ⇒ devolve-o (`tracing::info!` uma vez por processo via `std::sync::Once`);
  ausente ⇒ builtin `COLLABORATION_MODE_PLAN`; erro de leitura ⇒ `warn!` + builtin. Manter
  `builtin_collaboration_mode_presets()` intacta (os testes de `collaboration_mode_list.rs:52`
  comparam com ela).
- app-server: `codex-rs/app-server/src/request_processors/turn_processor.rs:380-395`
  `normalize_collaboration_mode` — quando `developer_instructions` vem `None` e `mode == Plan`,
  preencher com `plan_mode_instructions(Some(&self.config.codex_home))` (cobre Desktop e
  qualquer cliente app-server; `self.config.codex_home` já é usado em `:1572`).
- TUI: `codex-rs/tui/src/collaboration_modes.rs:7` `filtered_presets(_model_catalog)` passa a
  receber `&Config` (todos os call sites são métodos de `ChatWidget` com `self.config`:
  `settings.rs:150/314/651`, `turn_runtime.rs:254`, `input_flow.rs:264`) e reescreve
  `mask.developer_instructions` do preset Plan com `plan_mode_instructions(Some(&config.codex_home))`.

Gotcha a documentar (não corrigir agora): `core/src/context/world_state/collaboration_mode.rs:26-38`
dá precedência a `model_messages.collaboration_modes.plan` do catálogo remoto sobre
`settings.developer_instructions`; hoje o catálogo não envia nada para nenhum modelo
(verificado em `~/.codex/models_cache.json`), mas se passar a enviar, o override deixa de
valer — acrescentar um `warn!` nesse ramo quando `developer_instructions` difere do builtin.

Documentar em `docs/config.md` (secção «Fork») e no `docs/plans/2026-08-27-plan-mode/ANALISE.md`
(estado: implementado).

## P3 — Lembrete por turno em Plan mode

Padrão a copiar: `core/src/session/rollout_budget.rs:8-23` (fragmento por passo) e
`core/src/session/time_reminder.rs:97-139` (chamado em `core/src/session/turn.rs:370-377`).

- Novo `codex-rs/core/src/context/plan_mode_reminder.rs`: `PlanModeReminder` implementa
  `ContextualUserFragment` (`context-fragments/src/fragment.rs:64`): role `developer`, kind
  `collaboration_mode.plan_reminder`, marcadores `<plan_mode_reminder>…</plan_mode_reminder>`,
  corpo fixo (≈45 tokens):
  ```
  Plan mode is still active (full instructions earlier in this conversation): read-only
  except exploration; end this turn only with request_user_input, a <proposed_plan>, or a
  direct answer; unresolved preferences/tradeoffs must be asked, never assumed.
  ```
  Exportar em `core/src/context/mod.rs` (ao lado de `CurrentTimeReminder`, `:63`).
- Novo `codex-rs/core/src/session/plan_reminder.rs`: `maybe_record_plan_mode_reminder(sess,
  turn_context, step_context, recorded: &mut bool)` — só quando
  `step_context.settings.selected_collaboration_mode().mode == ModeKind::Plan` (caminho
  step-scoped, como `world_state.rs:204`) e ainda não emitido **neste turno** (`bool` local ao
  loop de `run_turn`, `turn.rs:314`, para não repetir a cada tool-call). Chamar em
  `turn.rs:371`, logo após `maybe_record_current_time_reminder`.
- `core/src/agent/control/spawn.rs:118`: acrescentar `PlanModeReminder::matches_text` ao filtro
  do histórico herdado por subagentes (evita N cópias).
- Compaction: o fragmento é developer message normal; nada a fazer.

Testes: unit em `core/src/context/plan_mode_reminder_tests.rs` (render/markers/matches_text);
teste de sessão em `core/src/session/turn_tests.rs` ao lado de
`plan_mode_uses_contributed_turn_item_for_last_agent_message` (`:62`): em Plan mode o prompt do
1.º passo contém exatamente 1 `<plan_mode_reminder>` e o 2.º passo do mesmo turno nenhum; em
Default mode nenhum.

## P4 — Popup «Implement this plan?» com caminho de revisão

Ficheiros: `codex-rs/tui/src/chatwidget/plan_implementation.rs` (builder `:28-114`),
`turn_runtime.rs:253-266`, `app_event.rs`, `app/event_dispatch.rs`.

- Novo `AppEvent::SetComposerText { text: String }` (não existe nenhum evento que preencha o
  composer); handler em `event_dispatch.rs` →
  `self.chat_widget.bottom_pane.set_composer_text(text, Vec::new(), Vec::new())`
  (`bottom_pane/mod.rs:843-853`, já move o cursor para o fim e pede redraw) — expor via wrapper
  `ChatWidget::set_composer_text` ao lado de `insert_str` (`chatwidget.rs:1734`).
- `selection_view_params(...)` ganha um parâmetro `plan_mask: Option<CollaborationModeMask>`
  (a máscara **ativa**, para preservar o esforço efémero) e dois itens **depois** de «No, stay
  in Plan mode» (manter os índices 0–2 — os testes `plan_mode.rs:129/151` usam Down+Enter por
  índice):
  3. «Ask me the open decisions first» — descrição «The model lists what it assumed and asks
     you the product decisions before you approve.»; ação `AppEvent::SubmitUserMessageWithMode {
     text: PLAN_DECISION_AUDIT_MESSAGE, collaboration_mode: plan_mask }` com
     ```
     Before I approve: list every design decision you resolved by assumption; for each, the
     alternative you rejected and whether it should have been a question. Ask me the ones that
     are mine to make via request_user_input, then re-emit the complete <proposed_plan>.
     ```
     (`submit_user_message_with_mode`, `input_flow.rs:254`, já aceita a mesma máscara com turno
     a correr — `plan_mode.rs:758-785`).
  4. «Revise the plan…» — descrição «Stay in Plan mode and tell the model what to change.»;
     ação `AppEvent::SetComposerText { text: "Revise the plan: " }`, `dismiss_on_select: true`.
  Ambos desativados (`is_disabled` + `disabled_reason`) quando `plan_mask` é `None`.
- `open_plan_implementation_prompt` passa `self.active_collaboration_mask.clone()`.

Testes (`tui/src/chatwidget/tests/plan_mode.rs`): regenerar os 3 snapshots
`plan_implementation_popup{,_context_usage,_no_selected}` (`cargo insta` → rever `.snap.new`);
novos: item 3 emite `SubmitUserMessageWithMode` com `ModeKind::Plan` e o texto fixo; item 4
emite `SetComposerText` e o composer passa a conter «Revise the plan: »; ambos desativados sem
máscara Plan.

## P5 — Esforço do preset de Plan herda a sessão

- `codex-rs/models-manager/src/collaboration_mode_presets.rs:25`: `reasoning_effort:
  Some(Some(ReasoningEffort::Medium))` → `None` (= «não pinar»). `effective_reasoning_effort`
  (`tui/src/chatwidget/settings.rs:406-415`) já faz `unwrap_or(current_effort)` ⇒ herda.
- `tui/src/chatwidget/settings.rs:163-178` `set_reasoning_effort`: o guard que não toca na
  máscara Plan passa a aplicar-se **só quando há override** (`self.config.plan_mode_reasoning_effort.is_some()`);
  sem override, a máscara Plan segue o esforço global.
- `tui/src/chatwidget/model_popups.rs:307-329`: o ramo `None =>` deixa de dizer «built-in Plan
  default (no reasoning)» e passa a «the session's reasoning effort».
- `app-server/README.md:254`: «the Plan preset selects medium reasoning effort» → «the Plan
  preset inherits the thread's reasoning effort unless a client overrides it».
- Manter o override `plan_mode_reasoning_effort` e a lógica de Ultra efémero
  (`session_flow.rs:92-94`, `thread_routing.rs:1580`, `config_persistence.rs:763-821`) como
  estão — com o preset a herdar, o dono pode remover `plan_mode_reasoning_effort = "high"` do
  `config.toml` (nota no fim; não mexer no config).

Testes a atualizar: `models-manager/src/collaboration_mode_presets_tests.rs:9-12` (`None`);
`tui/src/chatwidget/tests/plan_mode.rs:1665-1679` (`set_reasoning_effort` passa a atualizar a
máscara Plan sem override), `:673-682` (copy do popup), `:288-349` (ajustar o esforço esperado
em Plan mode = global), `:1682+` (guard só com override); `tui/src/app/tests.rs:8335-8341`
(literal, provavelmente sobrevive).

## P6 — Persistir planos (`~/.codex/plans/`), `/plans` no TUI, `plan/list`+`plan/read` no app-server

Base: o plano da simulação `docs/plans/2026-08-27-plan-mode/simulacao/claude-plano-r2.md`
(anchors verificados nesta sessão), **simplificado** para a primeira versão:

- **Sem `saved_path` no `PlanItem`/`ThreadItem::Plan`** (evita tocar `protocol/src/items.rs`,
  `v2/item.rs`, schema de `ThreadItem`, thread-store, `dynamic_tools.rs:1380`,
  `resume_picker.rs:6338`, 4 ficheiros de `exec/`, `plan_item.rs:66`). A dica no TUI é uma linha
  fixa; a cobertura Windows fica no teste de app-server via `plan/list`+`plan/read`.
- `PlanReadResponse { plan, markdown }` (aninhado, como `ProjectReadResponse`), sem `flatten`.
- `PlanListParams { cursor, limit }` apenas; cursor = último `id` devolvido; filtro «este
  projeto» é client-side.
- Picker sem tabs, sem view de loading; `updated_at` em `YYYY-MM-DD HH:MM` local.
- Contexto do plano prependido **antes** do IDE context (sem merge de delimitadores).
- `read_plan -> io::Result<Option<SavedPlan>>`; validação do id no handler.

### P6.1 Crate novo `codex-rs/plans` (`codex-plans`, sem dependência de `codex-core`)

- `plans/Cargo.toml` (modelo: `codex-rs/memories/write/Cargo.toml`, **sem** `codex-core`):
  deps `chrono`, `codex-protocol` (ThreadId), `codex-utils-absolute-path`, `codex-utils-path`
  (`write_atomically`, `utils/path-utils/src/lib.rs:122`), `serde` (derive), `serde_yaml`
  (workspace `"0.9"`, `Cargo.toml:423`), `tokio` (fs, rt), `tracing`; dev `pretty_assertions`,
  `tempfile`, `tokio` (macros, rt-multi-thread). `plans/BUILD.bazel`:
  `codex_rust_crate(name = "plans", crate_name = "codex_plans")`.
- `codex-rs/Cargo.toml`: `"plans"` em `[workspace] members` (após `"otel"`, l.96) e
  `codex-plans = { path = "plans" }` em `[workspace.dependencies]` (após `codex-otel`, l.233).
  `core/Cargo.toml` e `app-server/Cargo.toml`: `codex-plans = { workspace = true }`.
- Módulos (testes irmãos `#[path = "x_tests.rs"]`):
  - `lib.rs` — API pública + `plans_dir(codex_home) = codex_home.join("plans")`.
  - `front_matter.rs` — `PlanFrontMatter { title, thread_id, turn_id, cwd, model, created_at,
    updated_at, revision }`; `render_document(fm, body)` (`---\n` + yaml + `---\n\n` + body) e
    `parse_document(&str) -> Option<(PlanFrontMatter, String)>` (regra de split de
    `skills/src/parser.rs:200-221`).
  - `naming.rs` — `extract_title(md, now)` (1.º `#` → 1.ª linha não vazia → «Plan YYYY-MM-DD»,
    ≤80 chars), `slugify(title)` (`[a-z0-9]+` unidos por `-`, ≤48, vazio ⇒ `plan`),
    `file_stem_for(now_local, slug) = "%Y-%m-%dT%H-%M-%S-{slug}"` (sem `:`, Windows-safe).
  - `store.rs` — `save_plan_at(req, now)`, `list_plans`, `read_plan`, `is_valid_plan_id`.
- API:
  ```rust
  pub struct SavePlanRequest { codex_home: AbsolutePathBuf, thread_id: ThreadId, turn_id: String,
                               cwd: Option<AbsolutePathBuf>, model: Option<String>, markdown: String }
  pub struct SavedPlanPath { id: String, path: AbsolutePathBuf, revision: u32, written: bool }
  pub struct SavedPlanSummary { id, path: AbsolutePathBuf, title, thread_id: Option<String>, turn_id: Option<String>,
                                cwd: Option<String>, model: Option<String>, created_at: DateTime<Utc>, updated_at: DateTime<Utc>, revision: u32 }
  pub struct SavedPlan { summary: SavedPlanSummary, markdown: String }
  pub async fn save_plan(SavePlanRequest) -> io::Result<SavedPlanPath>;
  pub async fn list_plans(&AbsolutePathBuf) -> io::Result<Vec<SavedPlanSummary>>;   // updated_at desc, id desc
  pub async fn read_plan(&AbsolutePathBuf, id: &str) -> io::Result<Option<SavedPlan>>;
  pub fn is_valid_plan_id(&str) -> bool;   // não vazio, só [A-Za-z0-9._-], ≠ "." / ".."
  ```
- `save_plan` (um por thread): lista o diretório (front matter de cada `*.md`; inválidos ⇒
  `warn!` e ignora); entrada com o mesmo `thread_id` (a mais recente por `updated_at`): corpo
  igual ⇒ `written: false`; diferente ⇒ reescreve o **mesmo path** com `revision+1`,
  `updated_at = now`, `title/turn_id/cwd/model` novos, `created_at` preservado. Senão ficheiro
  novo `file_stem_for(now, slug)`; colisão ⇒ sufixo `-2`, `-3`. Escrita
  `spawn_blocking(write_atomically)`. Front matter em RFC3339 UTC; API em unix seconds.
- Testes unitários (`front_matter_tests.rs`, `naming_tests.rs`, `store_tests.rs` com `TempDir`
  e `now` fixo): round-trip; título/slug (fallbacks, truncagem multibyte); criar ⇒ `revision 1`;
  mesma thread ⇒ bump e `created_at` preservado; corpo igual ⇒ `written == false`; threads
  distintas ⇒ ficheiros distintos; colisão ⇒ `-2`; ordenação; ignora não-`.md`/inválidos;
  `read_plan` com `""`, `..`, `a/b`, `a\b` ⇒ `None`/inválido.

### P6.2 Core — gravar no hook

`codex-rs/core/src/session/turn.rs:2059-2085` `maybe_complete_plan_item_from_message`: após
`strip_citations` (l.2075), guard `if state.plan_item_state.completed { return; }` e depois
(≈12 linhas, comentário `// FORK:`):
```rust
if let Err(err) = codex_plans::save_plan(codex_plans::SavePlanRequest {
    codex_home: turn_context.config.codex_home.clone(),      // Config.codex_home (config/mod.rs:1114)
    thread_id: sess.thread_id,
    turn_id: turn_context.sub_id.clone(),
    cwd: Some(turn_context.config.cwd.clone()),              // cwd do turno (turn_context.rs:916-920); não usar o deprecated turn_context.cwd
    model: Some(turn_context.model_info().slug.clone()),
    markdown: plan_text.clone(),
}).await { warn!("failed to persist proposed plan: {err}"); }
state.plan_item_state.complete_with_text(sess, turn_context, plan_text).await;   // assinatura inalterada
```
Nunca falha o turno. Sem mudanças em `protocol/src/items.rs`.

### P6.3 App-server — `plan/list` + `plan/read` (estáveis, «Fork extension»)

- `app-server-protocol/src/protocol/v2/plan.rs` (novo; `mod plan;`/`pub use plan::*;` em
  `v2/mod.rs` entre `permissions` e `plugin`):
  `PlanSummary { id, title, path: String, thread_id: Option<String>, cwd: Option<String>, model: Option<String>,
  #[ts(type = "number")] created_at: i64, #[ts(type = "number")] updated_at: i64, revision: u32 }`,
  `PlanListParams { #[ts(optional = nullable)] cursor: Option<String>, limit: Option<u32> }` (Default),
  `PlanListResponse { data: Vec<PlanSummary>, next_cursor: Option<String> }`, `PlanReadParams { id: String }`,
  `PlanReadResponse { plan: PlanSummary, markdown: String }`. Derives/`rename_all = "camelCase"`/
  `#[ts(export_to = "v2/")]` conforme `AGENTS.md:260-307` (raiz do repo).
- `protocol/common.rs` `client_request_definitions!` — antes de `SkillsList` (l.809), sem
  `#[experimental]`:
  `PlanList => "plan/list" { params: v2::PlanListParams, serialization: global_shared_read("plans"), response: v2::PlanListResponse }`
  e `PlanRead => "plan/read" { … response: v2::PlanReadResponse }` (doc comment «Fork extension»).
- Handler novo `app-server/src/request_processors/plan_processor.rs`
  (`PlanRequestProcessor { config: Arc<Config> }`): `plan_list` — `codex_plans::list_plans(&config.codex_home)`
  (erro ⇒ `internal_error`), `limit` default 50 / máx 200 (`clamp`, como `projects.rs:73-77`),
  cursor = último id (`skip_while(id != cursor).skip(1)`), `next_cursor` só se sobrar;
  `plan_read` — `!is_valid_plan_id` ⇒ `invalid_params`, `None` ⇒ `invalid_params("plan not
  found: {id}")` (precedente `projects.rs:103`), `Io` ⇒ `internal_error`.
- Wiring: `request_processors.rs` (`mod plan_processor;` + `pub(crate) use`);
  `message_processor.rs` import l.27, campo l.142, construção l.413-419
  (`PlanRequestProcessor::new(Arc::clone(&config))`), literal `Self { .. }` l.564, arms após
  `ClientRequest::SkillsList` l.1353 (match exaustivo).
- Schema/TS (a receita `just write-app-server-schema` está partida): de `codex-rs/`,
  `python app-server-protocol\scripts\write_schema_fixtures.py` e `… --experimental`; validar
  com `just test -p codex-app-server-protocol`.
- `app-server/README.md`: bullets `plan/list`/`plan/read` após `skills/list` (l.255) + template
  de handoff de 6 linhas para `turn/start`:
  `"# Saved plan: {title} ({path})\n\n{markdown}\n\n## My request for Codex:\nImplement this plan."`.
- Testes (`app-server/tests/suite/v2/`, harness `TestAppServer`, `mcp.request::<T>`):
  `plan_item.rs` (existente, corre em Windows): após o turno de plan, `plan/list` devolve 1
  entrada com `threadId == thread.id` e `plan/read` devolve `markdown == "# Final plan\n- first\n- second\n"`,
  `title == "Final plan"`; novo `plan_list.rs` (modelo `collaboration_mode_list.rs`; fixtures
  escritos no `TempDir`): newest-first + paginação (`limit: 1` → `nextCursor` → 2.ª página);
  `plan/read` sem front matter; id inválido ⇒ `-32602`; desconhecido ⇒ `-32602`.

### P6.4 TUI — `/plans`, carregar plano, dica «Plan saved»

Módulos novos: `tui/src/chatwidget/saved_plans.rs` (estado, popup de ações, injeção) e
`tui/src/app/plans_picker.rs` (RPCs; `mod plans_picker;` em `app.rs` l.227, antes de
`platform_actions`). Edições mínimas:

- `slash_command.rs`: `Plans,` na l.43; arm em `description` (l.88, exaustivo): «browse saved
  plans and load one into this session»; `| SlashCommand::Plans` no grupo `=> false` de
  `available_during_task` (l.224). **Não** adicionar a `supports_inline_args`.
  `slash_dispatch.rs:1128-1195` `queued_command_drain_result` (exaustivo): `Plans` no grupo
  `QueueDrain::Stop` junto a `Plan` (l.1171). Arm de dispatch após `Plan` (l.305-307):
  `blocks_direct_input` ⇒ `PARENT_OWNED_INPUT_MESSAGE`; senão `AppEvent::OpenPlansPicker`.
- `app_event.rs` (junto a `OpenResumePicker`, l.415):
  `enum SavedPlanAction { Implement, AttachToNextMessage, Revise }`, `OpenPlansPicker`,
  `PlansPickerLoaded { request_id: Uuid, result: Result<Vec<PlanSummary>, String> }`,
  `OpenSavedPlanActions { id, title }`, `LoadSavedPlan { id, action }`,
  `SavedPlanLoaded { request_id, action, result: Result<PlanReadResponse, String> }`.
  `event_dispatch.rs` junto a `OpenAgentPicker` (l.2552): 5 arms → `open_plans_picker`,
  `apply_plans_picker_result`, `chat_widget.show_saved_plan_actions`, `load_saved_plan`,
  `apply_saved_plan_loaded`.
- `app/plans_picker.rs`: padrão `app/agent_picker.rs:26-85` (`app_server.request_handle()` +
  `tokio::spawn` + `request_typed::<PlanListResponse>(ClientRequest::PlanList { request_id:
  RequestId::String(format!("plan-list-{}", Uuid::new_v4())), params: PlanListParams { cursor, limit: Some(100) } })`,
  paginando até 500; `plan/read` idem). Só mostra o picker quando o resultado chega (vazio ⇒
  `add_info_message("No saved plans yet.", Some("Plans you approve in Plan mode are saved to ~/.codex/plans."))`;
  erro ⇒ `add_error_message`); `picker_request_id: Option<Uuid>` descarta respostas obsoletas.
- `chatwidget/saved_plans.rs`:
  - `PendingPlanContext { id, title, path, markdown }`; `SavedPlansState { pending_context, picker_request_id, load_request_id }`
    (campo `saved_plans` em `chatwidget.rs` após `ide_context` l.636; `SavedPlansState::default()`
    em `constructor.rs:174`; `pub(crate) mod saved_plans;` após `mod plan_implementation;` l.385).
  - `picker_params(&[PlanSummary], current_cwd)`: `ListSelectionView` `is_searchable: true`, rows
    newest-first com `name = title`, `description = "{updated_at local} · {basename(cwd)} · rev N"`
    (prefixo «this project ·» quando `cwd == config.cwd`), `search_value = "{title} {cwd} {id}"`
    em todas as rows, ação → `OpenSavedPlanActions`, `dismiss_on_select: false` +
    `dismiss_parent_on_child_accept: true` (`bottom_pane/mod.rs:570-591`).
  - `load_plan_params(id, title, plan_mode_available)`: popup «Load plan «{title}»?» com 3 rows,
    `dismiss_on_select: true`, Esc volta ao picker: «Implement this plan» (Default mode),
    «Attach to my next message», «Revise in Plan mode» (`disabled_reason` se Plan indisponível).
  - `render_plan_context(ctx) = format!("# Saved plan: {title} ({path})\n\n{markdown.trim_end()}\n")`.
  - `apply_loaded_plan(plan, action)`: guard `blocks_direct_input`; guarda `pending_context`;
    `add_info_message("Loaded plan «{title}»", Some(path))`.
    `Implement` ⇒ `default_mode_mask` (None ⇒ erro `PLAN_IMPLEMENTATION_DEFAULT_UNAVAILABLE`,
    contexto fica anexado); turno a correr/pending ⇒ degrada para Attach; senão
    `submit_user_message_with_mode(PLAN_IMPLEMENTATION_CODING_MESSAGE, default_mask)`.
    `Revise` ⇒ igual com `collaboration_modes::plan_mask` e texto fixo
    `SAVED_PLAN_REVISE_MESSAGE = "Revise this plan: point out weaknesses, gaps and risks, then re-emit the full updated plan as a complete <proposed_plan> block."`
    — a máscara Plan é aplicada por `submit_user_message_with_mode` (`input_flow.rs:254-289`);
    quando o plano revisto chega, `maybe_prompt_plan_implementation` (`turn_runtime.rs:226`)
    abre «Implement this plan?» como hoje (o picker já fechou). Nota: numa sessão nova o plano
    revisto vira ficheiro **novo** (chave = thread); no mesmo thread reescreve (`revision+1`).
    `AttachToNextMessage` ⇒ info «Plan «{title}» attached — it will be included with your next message.»
  - `maybe_apply_pending_plan_context(&mut items)`: chamado em `input_submission.rs:328`
    **antes** de `maybe_apply_ide_context`; reutiliza `prefixed_text_input` e
    `PROMPT_REQUEST_BEGIN` de `ide_context/prompt.rs` (tornar `pub(crate)`, l.16/l.71); texto =
    `render_plan_context + "\n" + PROMPT_REQUEST_BEGIN + "\n"`; limpa `pending_context`. O
    display otimista e o item committed já removem tudo até ao último delimitador
    (`user_messages.rs:662-693`, `rsplit_once`) ⇒ TUI/Desktop/export mostram só o pedido.
  - Dica: `replay.rs:128` `ThreadItem::Plan { text, .. } => { self.on_plan_item_completed(text); if !from_replay { self.on_plan_item_saved(); } }`
    — linha dim `• Plan saved to ~/.codex/plans — use /plans to load it in another session.`
    (assinatura de `on_plan_item_completed` inalterada; sem linha em replay/resume).
- Testes (novo `tui/src/chatwidget/tests/saved_plans.rs`, `mod saved_plans;` em
  `chatwidget/tests.rs` entre `review_mode` l.255 e `side` l.256; helpers
  `make_chatwidget_manual`, `next_submit_op`, `drain_insert_history`, `render_bottom_popup`,
  `normalize_snapshot_paths`): `dispatch_command(Plans)` ⇒ `OpenPlansPicker` (bloqueado com task
  a correr e com `blocks_direct_input`); `picker_params` (search_value em todas as rows,
  descrição determinística); snapshots `plans_picker_loaded`, `saved_plan_actions_popup`,
  `saved_plan_user_cell`, `plan_saved_hint`; navegação Enter ⇒ `OpenSavedPlanActions`,
  Enter/Down+Enter/Down+Down+Enter ⇒ `LoadSavedPlan{Implement|Attach|Revise}`;
  `apply_loaded_plan(Implement)` ⇒ `Op::UserTurn` com texto `# Saved plan: … ## My request for
  Codex:\nImplement the plan.` (1 delimitador), máscara Default, `pending_context` limpo;
  `Revise` ⇒ idem com `SAVED_PLAN_REVISE_MESSAGE`, máscara Plan, e após
  `ItemCompleted(Plan)`+`TurnComplete` o popup «Implement this plan?» abre; `Attach` + «do it»
  ⇒ texto prefixado e a 2.ª mensagem sem prefixo; item committed mostra só o pedido;
  `handle_server_notification(ItemCompleted{Plan})` ⇒ linha «Plan saved»; via replay ⇒ ausente;
  `slash_command.rs`: `from_str("plan") == Plan`, `from_str("plans") == Plans`,
  `!Plans.available_during_task()`.

## Ordem de execução

1. **P2** (strings + testes) → `cargo test -p codex-core request_user_input`.
2. **P5** (preset `None` + guard + copy + README + testes) → `cargo test -p codex-models-manager`,
   `cargo test -p codex-tui plan_mode`.
3. **P1** template + `plan_mode_instructions()` + 2 consumidores → `cargo test -p codex-models-manager`,
   `cargo test -p codex-app-server collaboration`, `cargo test -p codex-tui plan_mode`.
4. **P3** fragmento + helper + hook em `turn.rs:371` + filtro `spawn.rs:118` + testes →
   `cargo test -p codex-core plan_mode_reminder`, `turn_tests`.
5. **P4** `SetComposerText` + 2 itens + testes/snapshots → `cargo test -p codex-tui plan_implementation`.
6. **P6.1** crate → `cargo build -p codex-plans` (atualiza `Cargo.lock`) → `cargo test -p codex-plans`.
7. **P6.2** hook no core → `cargo build -p codex-core`.
8. **P6.3** protocolo + handler + wiring → `cargo build -p codex-app-server` → schemas
   (`write_schema_fixtures.py` ×2) → `just test -p codex-app-server-protocol`,
   `cargo test -p codex-app-server plan_`.
9. **P6.4** TUI → `cargo build -p codex-tui` → `cargo test -p codex-tui saved_plans slash plan_mode ide_context`
   → rever `.snap.new` (`cargo insta pending-snapshots -p codex-tui`) → `cargo insta accept -p codex-tui`.
10. `just fmt`, `just fix -p codex-plans -p codex-core -p codex-models-manager -p codex-app-server-protocol -p codex-app-server -p codex-tui`
    (justfile na **raiz** do repo), `just bazel-lock-update` se o Bazel existir localmente
    (senão anotar), `cargo check --workspace --tests`.
11. Docs: `docs/config.md` (override `plan_mode.md`, `/plans`), atualizar
    `docs/plans/2026-08-27-plan-mode/ANALISE.md` §5 com o estado.
12. **Build + hot-swap** (sem commit): `cargo build --release -p codex-cli` (+ `codex-code-mode-host`
    se o crate for tocado — não é), depois no vendor dir
    `…\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\`:
    `Rename-Item codex.exe codex.exe.previous-<yyyyMMdd-HHmmss>.exe` e copiar
    `codex-rs\target\release\codex.exe`.

Nota Windows: `cargo test` direto precisa de `$env:RUST_MIN_STACK = "8388608"` (o `just test`
já o define; o `~/.cargo/config.toml` do dono também). Comparar falhas novas com o catálogo de
test debt conhecido antes de diagnosticar.

## Verificação end-to-end (após hot-swap)

1. `codex --help` arranca. Sessão real com `CODEX_HOME` temporário (`$env:CODEX_HOME =
   "C:\tmp\codex-home-plans"` com `auth.json`/`config.toml` copiados) para não sujar `~/.codex`.
2. `/plan` + pedido pequeno com ambiguidade («plan a /whoami slash command that prints either
   the account email or the plan name»): esperado — o modelo explora, faz **pelo menos uma**
   `request_user_input` com a preferência (checkpoint), o `<proposed_plan>` tem secções
   Context/Decisions/Implementation com paths/Verification, e aparece
   `• Plan saved to ~/.codex/plans …`; o popup mostra 5 opções; escolher «Ask me the open
   decisions first» ⇒ o modelo audita e pergunta em Plan mode; «Revise the plan…» ⇒ composer
   com «Revise the plan: ».
3. Footer do TUI em Plan mode mostra o esforço da sessão (sem `plan_mode_reasoning_effort` no
   config temporário) — antes mostrava `medium`.
4. Ficheiro em `C:\tmp\codex-home-plans\plans\<ts>-<slug>.md` com front matter; pedir uma
   revisão ⇒ mesmo ficheiro, `revision: 2`; pergunta de esclarecimento ⇒ ficheiro intocado.
5. Nova sessão: `/plans` → lista → «Attach to my next message» + «summarize the plan in one
   line» ⇒ célula do user mostra só o pedido, resposta prova que o plano chegou; «Revise in Plan
   mode» ⇒ turno em Plan mode e popup «Implement this plan?» no fim; «Implement this plan» ⇒
   Default mode + «Implement the plan.». Ctrl+T e `/export` mostram só o pedido.
6. Lembrete por turno: com `RUST_LOG`/rollout, confirmar 1 `<plan_mode_reminder>` por turno em
   Plan mode e nenhum em Default.
7. `$env:CODEX_HOME\plan_mode.md` com um texto marcador ⇒ o rollout do turno seguinte em Plan
   mode contém esse texto no `<collaboration_mode>`; remover o ficheiro ⇒ volta ao builtin.
8. app-server por stdio (`codex app-server`): `initialize` (sem `experimentalApi`), `plan/list {}`,
   `plan/read {"id"}`, `plan/read {"id": "../x"}` ⇒ `-32602`; `turn/start` com o template de
   handoff do README.
9. Reutilizar `docs/plans/2026-08-27-plan-mode/simulacao/codex_driver.py` com a mesma tarefa
   para medir o antes/depois (perguntas por plano, paths no plano) — opcional.

## Assunções e riscos

- **Sem `saved_path` na v1** (alternativa: campo em `PlanItem`/`ThreadItem::Plan` com
  `#[serde(default)]` — fica como follow-up; a dica mostra o diretório, não o ficheiro).
- **Sem `multi_select`** em `request_user_input` (follow-up com mudança de protocolo).
- **Sem `plan/delete`/retenção/opt-out**: sempre ligado, guardar indefinidamente; remoção manual.
- **Override do catálogo remoto** (`world_state/collaboration_mode.rs:26-38`) continua a ter
  precedência sobre `plan_mode.md`; hoje o catálogo não envia nada — só `warn!`.
- **Lembrete por turno** custa ≈45 tokens/turno em Plan mode; na 1.ª volta coexiste com as
  instruções completas (aceitável).
- **Higiene do fork**: hooks de 1 linha com `// FORK:` em `turn.rs` (×2), `input_submission.rs`,
  `replay.rs`, `spawn.rs`, `turn_processor.rs`; tudo o resto em ficheiros novos. Após o próximo
  sync upstream verificar `plan.md`, `turn.rs`, `common.rs`, `slash_command.rs`.
- **Config do dono**: com o preset a herdar, `plan_mode_reasoning_effort = "high"` em
  `~/.codex/config.toml` passa a limitar o Plan mode a `high` nas sessões que não estão em
  `ultra` — recomendo removê-lo depois do deploy (não faz parte desta entrega).
- **Comportamento**: o checkpoint obrigatório acrescenta ≈1 ronda de perguntas à maioria dos
  planos (é o objetivo); se ficar pesado em tarefas triviais, a válvula é o próprio
  `plan_mode.md` (editar sem rebuild).

## P2 — Alargar `request_user_input` (só strings + testes)

Ficheiro: `codex-rs/core/src/tools/handlers/request_user_input_spec.rs`
- `options` description: «Provide 2-3 mutually exclusive choices…» → «Provide 2-4 mutually
  exclusive choices…» (resto igual: recomendada primeiro com "(Recommended)", sem "Other").
- `questions` description: «Questions to show the user. Prefer 1 and do not exceed 3» →
  «Questions to show the user. Batch related decisions of the same design choice in one call;
  1 to 4 questions per call.»
- `request_user_input_tool_description`: «Request user input for one to three short questions…»
  → «Request user input for one to four short questions and wait for the response.…».

Testes a atualizar (fixam as strings literalmente):
`core/src/tools/handlers/request_user_input_spec_tests.rs` linhas ~78 (options), ~99
(questions), ~180-188 (descrição nos três modos). Sem mudança de protocolo, sem `multi_select`
(fica como follow-up). O TUI já renderiza N perguntas com cabeçalho de progresso
(`tui/src/bottom_pane/request_user_input/render.rs:266`) e acrescenta «None of the above».

Verificação: `cargo test -p codex-core request_user_input`.

_(P3–P6 a seguir)_
