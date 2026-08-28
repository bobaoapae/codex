# Persistência de planos do Plan mode (`~/.codex/plans/`) + `/plans` no TUI + `plan/list`/`plan/read` no app-server

## Context

Hoje o Plan mode do Codex produz o plano só como bloco `<proposed_plan>` dentro da mensagem do
assistente: o core emite `TurnItem::Plan { id, text }` (persistido no rollout), o TUI renderiza e
abre «Implement this plan?», e o texto morre com a sessão. Não há ficheiro, não há reentrada noutra
sessão, não há forma de o Desktop listar planos anteriores. A análise de hoje
(`docs/plans/2026-08-27-plan-mode/ANALISE.md`, §5 P6) já identificou o ponto de ancoragem
(`core/src/session/turn.rs::maybe_complete_plan_item_from_message`) e o objetivo: dar ao Codex o que o
Claude Code tem com o plan file (reentrada, revisão entre sessões, planos legíveis fora do rollout).

Resultado pretendido:

1. Cada plano final (o bloco `<proposed_plan>` completo de um turno em Plan mode) fica guardado como
   markdown em `~/.codex/plans/` — em qualquer cliente (TUI, Desktop, exec), porque a gravação é no core.
2. `/plans` no TUI lista os planos guardados e permite carregar um deles para a sessão atual, para
   implementar («Implement this plan») ou como contexto da próxima mensagem («Attach to next message»).
3. O Desktop (ou qualquer cliente app-server) lista e lê planos via `plan/list` / `plan/read` e injeta o
   plano num turno com `turn/start` (template de prompt documentado no README do app-server).

Restrições do fork (política já em vigor): mudar o mínimo de ficheiros quentes do upstream; código novo em
módulos/crates novos; sem flag de feature nova (evita `features/src/lib.rs`, ficheiro de conflitos
recorrentes); Windows é a plataforma de dev (`RUST_MIN_STACK=8388608` nos testes).

## Decisões de design (assumidas — cada uma tem a alternativa em 1 linha)

| # | Decisão | Alternativa rejeitada |
|---|---|---|
| D1 | **Gravação no core**, em `maybe_complete_plan_item_from_message` (`core/src/session/turn.rs:2059`) — único sítio onde existe o plano final autoritativo; cobre TUI, Desktop e `exec`. | Extension API (`TurnItemContributor`): nunca vê `TurnItem::Plan` e registar um contributor muda o streaming de *todas* as sessões (`turn.rs:2337 defer_streamed_turn_items_for_contributors`). |
| D2 | **Um ficheiro por thread**: `~/.codex/plans/<YYYY-MM-DDTHH-MM-SS>-<slug>.md`, nome fixado no 1.º plano; planos seguintes na mesma thread reescrevem o ficheiro (`revision++`, `updated_at`), corpo idêntico ⇒ não escreve (o template manda repetir o plano em follow-ups de esclarecimento). É o modelo do plan file do Claude. | Um ficheiro por bloco: a lista enche-se de revisões do mesmo plano; o histórico de revisões continua no rollout de qualquer forma. |
| D3 | Metadados em **YAML front matter** no próprio `.md` (`title, thread_id, turn_id, cwd, model, created_at, updated_at, revision`). Ficheiro legível/editável à mão, sem índice. | Sidecar `.json` / índice SQLite: mais peças para manter sincronizadas. |
| D4 | O `PlanItem` passa a levar `saved_path` (core + v2 `ThreadItem::Plan.savedPath`) → o TUI mostra «Plan saved to …» e o Desktop sabe o caminho. | Novo `EventMsg::PlanSaved`: toca ~10 `match` exaustivos em 8 crates (mapa em `rg EventMsg::PlanDelta`). |
| D5 | App-server: **`plan/list` + `plan/read`**, não experimentais (o Desktop pode não ligar `experimentalApi`), marcados «Fork extension». Carregar = o cliente compõe `turn/start` com o template de prompt documentado no README. | `plan/load` server-side: duplicaria `turn/start` (modo, modelo, settings); fica como follow-up se o UI injetado no Desktop precisar. |
| D6 | TUI obtém os planos **via RPC** (`plan/list`/`plan/read`), como `skills/list`/`thread/list` — o TUI já não depende de `codex-core` (`tui/Cargo.toml`), é cliente puro do app-server. | Ler `~/.codex/plans` diretamente no TUI (padrão do theme picker): duplicaria a lógica de parsing e diverge do rumo upstream. |
| D7 | Injeção de contexto no TUI **igual ao IDE context**: prefixo + delimitador `## My request for Codex:` (`tui/src/ide_context/prompt.rs:16`); todas as superfícies (TUI, Desktop, export) mostram só o que vem depois do delimitador. | Mensagem visível com o plano inteiro (o que faz hoje «Yes, clear context and implement»): transcript poluído com 100+ linhas. |
| D8 | Sem feature flag e sem opção de config: gravação sempre ligada (como os rollouts). | `Feature::PlanPersistence`: toca `features/src/lib.rs` (conflitos de sync) sem ganho real. |

## Arquitetura (fluxo)

```
Plan mode turn ──► core turn.rs: extract_proposed_plan_text ──► codex_plans::save_plan ──► ~/.codex/plans/*.md
                                          │                                  │
                                          └── ItemCompleted(TurnItem::Plan{text, saved_path}) ──► rollout / app-server
                                                                                                        │
TUI /plans ──AppEvent──► app layer ──RPC plan/list, plan/read──► app-server PlanRequestProcessor ──► codex_plans::{list_plans, read_plan}
   └── picker ──► «Load plan?» popup ──► PendingPlanContext ──► prefixo no UserInput::Text (delimitador) ──► turn/start
Desktop ──RPC plan/list, plan/read──► idem ──► compõe turn/start (template no README)
```

Novo crate `codex-rs/plans` (`codex-plans`) sem dependência de `codex-core` (core depende dele; app-server também).
Edições a ficheiros upstream quentes ficam pequenas: `turn.rs` (~25 linhas), `protocol/src/items.rs` (+4),
`v2/item.rs` (+6), `common.rs` (+12), `message_processor.rs` (+7), `request_processors.rs` (+2), `v2/mod.rs` (+2),
`slash_command.rs` (+4 arms), `app_event.rs` (+3 variantes), `event_dispatch.rs` (+3 arms).

---

## Parte 1 — crate `codex-plans` (novo: `codex-rs/plans/`)

**Scaffolding** (copiar convenções de `codex-rs/memories/write/Cargo.toml` e `BUILD.bazel`):
- `plans/Cargo.toml`: `name = "codex-plans"`, `[lib] name = "codex_plans"`, `doctest = false`, `[lints] workspace = true`;
  deps: `chrono`, `codex-protocol` (ThreadId), `codex-utils-absolute-path`, `codex-utils-path` (crate real de
  `utils/path-utils`, tem `write_atomically(&Path, &str)` em `utils/path-utils/src/lib.rs:122`), `serde` (derive),
  `serde_yaml` (workspace "0.9", já usado por `skills`), `thiserror`, `tokio` (fs, rt), `tracing`;
  dev: `pretty_assertions`, `tempfile`, `tokio` (macros, rt-multi-thread).
- `plans/BUILD.bazel`: `codex_rust_crate(name = "plans", crate_name = "codex_plans")` (deps vêm do Cargo metadata).
- `codex-rs/Cargo.toml`: `"plans"` em `[workspace] members` (após `"otel"`, ~l.96) e
  `codex-plans = { path = "plans" }` em `[workspace.dependencies]` (entre `codex-otel` e `codex-plugin`, ~l.233).
- `core/Cargo.toml` e `app-server/Cargo.toml`: `codex-plans = { workspace = true }`. `app-server-protocol` **não** depende (tipos de wire independentes).

**Módulos** (testes em ficheiros irmãos `#[path = "x_tests.rs"]`, convenção do repo):

```
plans/src/lib.rs           API pública + tipos + plans_dir(codex_home) = codex_home.join("plans")
plans/src/front_matter.rs  PlanFrontMatter {title, thread_id?, turn_id?, cwd?, model?, created_at, updated_at, revision(default 1)}
                           render_document(fm, body) -> String   ("---\n" + serde_yaml::to_string + "---\n\n" + body com '\n' final)
                           parse_document(&str) -> Option<(PlanFrontMatter, String)>  (mesma regra de split de skills/src/parser.rs:200-221)
plans/src/naming.rs        extract_title(md, now) (1.º `#` heading → 1.ª linha não vazia → "Plan YYYY-MM-DD"; ≤80 chars via chars().take)
                           slugify(title) ([a-z0-9]+ unidos por '-', ≤48, sem '-' final, vazio ⇒ "plan")
                           file_stem_for(now_local, slug) = "%Y-%m-%dT%H-%M-%S-{slug}"  (hora local, como os rollouts; sem ':' → Windows-safe)
plans/src/store.rs         save_plan_at(req, now) / list_plans / read_plan / is_valid_plan_id
```

**API pública** (`lib.rs`):
```rust
pub struct SavePlanRequest { codex_home: AbsolutePathBuf, thread_id: ThreadId, turn_id: String,
                             cwd: Option<AbsolutePathBuf>, model: Option<String>, markdown: String }
pub struct SavedPlanPath { id: String /*file stem*/, path: AbsolutePathBuf, revision: u32, written: bool }
pub struct PlanListFilter { thread_id: Option<ThreadId>, cwd: Option<PathBuf> }   // Default
pub struct SavedPlanSummary { id, path: AbsolutePathBuf, title, thread_id: Option<String>, turn_id: Option<String>,
                              cwd: Option<String>, model: Option<String>, created_at: DateTime<Utc>, updated_at: DateTime<Utc>, revision: u32 }
pub struct SavedPlan { summary: SavedPlanSummary, markdown: String /*sem front matter*/ }
pub enum PlanReadError { InvalidId(String), Io(io::Error) }   // thiserror
pub async fn save_plan(SavePlanRequest) -> io::Result<SavedPlanPath>;
pub async fn list_plans(&AbsolutePathBuf, &PlanListFilter) -> io::Result<Vec<SavedPlanSummary>>;  // updated_at desc, id desc
pub async fn read_plan(&AbsolutePathBuf, id: &str) -> Result<Option<SavedPlan>, PlanReadError>;
pub fn is_valid_plan_id(&str) -> bool;
```

**Semântica de `save_plan`** (D2): lista o diretório (parse do front matter de cada `*.md`; inválidos ⇒ `warn!` e ignora);
se existe entrada com o mesmo `thread_id` (a mais recente por `updated_at`): corpo igual ⇒ devolve `written: false` sem
tocar no ficheiro; diferente ⇒ reescreve o **mesmo path** com `revision+1`, `updated_at = now`, `title/turn_id/cwd/model`
novos, `created_at` preservado. Senão, ficheiro novo `file_stem_for(now, slug)`; colisão ⇒ sufixo `-2`, `-3`… Escrita com
`tokio::task::spawn_blocking(|| write_atomically(..))`. Timestamps: ficheiro em hora local, front matter RFC3339 UTC
(`to_rfc3339_opts(Secs, true)`), API em unix seconds. `cwd` guardado como string nativa.

**`read_plan`**: `is_valid_plan_id` = não vazio, só `[A-Za-z0-9._-]`, ≠ `.`/`..`; path = `plans_dir/{id}.md` e
`path.parent() == plans_dir`; `NotFound` ⇒ `Ok(None)`.

Exemplo de ficheiro:
```markdown
---
title: Persist plan-mode plans
thread_id: 0199a3b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b
turn_id: 0199a3b5-0000-7000-8000-000000000042
cwd: C:\Users\Joao\RustProjects\codex
model: gpt-5.6-sol
created_at: 2026-08-27T14:03:05Z
updated_at: 2026-08-27T15:10:41Z
revision: 2
---

# Persist plan-mode plans
...
```

**Testes unitários** (`front_matter_tests.rs`, `naming_tests.rs`, `store_tests.rs` com `TempDir` e `save_plan_at` com
`now` fixo): round-trip do front matter (título com `:` e aspas; começa por exatamente um `---`); título/slug (heading,
fallback, truncagem multibyte); criar → `revision 1`; mesma thread reescreve e faz bump preservando `created_at`; corpo
idêntico ⇒ `written == false`; threads distintas ⇒ ficheiros distintos; colisão ⇒ `-2`; ordenação por `updated_at`;
filtros `thread_id`/`cwd`; ignora ficheiros não-`.md`/inválidos; `read_plan` rejeita `""`, `..`, `a/b`, `a\b`, `a b`; missing ⇒ `None`.

---

## Parte 2 — core: gravar no hook + `saved_path` no `PlanItem`

- `codex-rs/protocol/src/items.rs:174` `PlanItem` ganha (mesmo shape de `ImageGenerationItem.saved_path`, l.382-384):
  ```rust
  /// FORK: absolute path of the persisted plan markdown file, when saved.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  #[ts(optional)]
  pub saved_path: Option<AbsolutePathBuf>,
  ```
  (`#[serde(default)]` obrigatório: `ItemCompleted(Plan)` é persistido no rollout em ambos os history modes — `rollout/src/policy.rs:92-104`.)
- `core/src/session/turn.rs`:
  - `ProposedPlanItemState::start` (l.1710): `saved_path: None`.
  - `complete_with_text(.., text, saved_path: Option<AbsolutePathBuf>)` (l.1734): passa para o `PlanItem`.
  - `maybe_complete_plan_item_from_message` (l.2074-2082): após `strip_citations`, guard `if state.plan_item_state.completed { return; }`
    (evita que um 2.º bloco no mesmo turno reescreva o ficheiro), depois:
    ```rust
    // FORK: persist the final plan under `$CODEX_HOME/plans` (one file per thread).
    let saved_path = match codex_plans::save_plan(codex_plans::SavePlanRequest {
        codex_home: turn_context.config.codex_home.clone(),
        thread_id: sess.thread_id,
        turn_id: turn_context.sub_id.clone(),
        cwd: Some(turn_context.config.cwd.clone()),      // cwd do environment primário já resolvido em new_turn_context (turn_context.rs:914-919, :641)
        model: Some(turn_context.model_info().slug.clone()),
        markdown: plan_text.clone(),
    }).await {
        Ok(saved) => Some(saved.path),
        Err(err) => { warn!("failed to persist proposed plan: {err}"); None }
    };
    state.plan_item_state.complete_with_text(sess, turn_context, plan_text, saved_path).await;
    ```
    Nunca falha o turno; `CODEX_HOME` é honrado porque `Config.codex_home` vem de `find_codex_home()`.
- Teste: `core/tests/suite/items.rs:443` `plan_mode_emits_plan_item_from_proposed_plan_block` — ligar `home` do
  `TestCodex` e assertar `plan_completed.saved_path` dentro de `home/plans/`, ficheiro começa por `---\n` e acaba com o
  corpo. **Nota:** o ficheiro inteiro é `#![cfg(not(target_os = "windows"))]` — não corre localmente; a cobertura
  Windows é o teste de app-server (Parte 3).

---

## Parte 3 — app-server: `plan/list` + `plan/read` + `savedPath`

**Protocolo** (`codex-rs/app-server-protocol/src/protocol/`):
- Novo `v2/plan.rs` (+ `mod plan;` / `pub use plan::*;` em `v2/mod.rs`, ordem alfabética após `permissions`):
  ```rust
  PlanSummary { id, title, path: String, thread_id: Option<String>, cwd: Option<String>, model: Option<String>,
                #[ts(type = "number")] created_at: i64, #[ts(type = "number")] updated_at: i64, revision: u32 }
  PlanListParams { #[ts(optional = nullable)] cursor: Option<String>, limit: Option<u32>, thread_id: Option<String>, cwd: Option<PathBuf> }  // Default
  PlanListResponse { data: Vec<PlanSummary>, next_cursor: Option<String> }
  PlanReadParams { id: String }
  PlanReadResponse { #[serde(flatten)] plan: PlanSummary, markdown: String }   // flatten tem precedente em v2/plugin.rs:544; fallback: aninhar `plan`
  ```
  Todos com `#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)] #[serde(rename_all = "camelCase")] #[ts(export_to = "v2/")]`;
  sem `skip_serializing_if` em respostas; `#[ts(optional = nullable)]` só nos `*Params` (regras de `AGENTS.md:260-307`).
- `common.rs` `client_request_definitions!` — inserir antes de `SkillsList` (~l.809), sem `#[experimental]`:
  ```rust
  /// Fork extension (codex fork): list plan-mode plans persisted under `$CODEX_HOME/plans`.
  PlanList => "plan/list" { params: v2::PlanListParams, serialization: global_shared_read("plans"), response: v2::PlanListResponse },
  /// Fork extension (codex fork): read one persisted plan, including its markdown body.
  PlanRead => "plan/read" { params: v2::PlanReadParams, serialization: global_shared_read("plans"), response: v2::PlanReadResponse },
  ```
- `v2/item.rs:267-274` `ThreadItem::Plan { id, text, #[serde(default)] saved_path: Option<String> }` (o JSON v2 é
  persistido em SQLite pelo thread-store — `thread-store/src/local/thread_history.rs:162,440` — daí o `default`);
  mapeamento em `item.rs:892`: `saved_path: plan.saved_path.map(|p| p.to_string_lossy().into_owned())`.
- **Sites que deixam de compilar** com o campo novo (corrigir mecanicamente): destructuring exaustivo em
  `tui/src/dynamic_tools.rs:1380` (`ThreadItem::Plan { id, text }` → `..`); literais `ThreadItem::Plan { id, text }` em
  `tui/src/resume_picker.rs:6338`, `tui/src/chatwidget/tests/slash_commands.rs:1828`,
  `exec/src/event_processor_with_human_output_tests.rs:255,279,283`, `exec/src/lib_tests.rs:377`,
  `exec/tests/event_processor_with_json_output.rs:283,1590`, `app-server/tests/suite/v2/plan_item.rs:66` (+ `saved_path: None`).

**Handler** (`codex-rs/app-server/src/request_processors/plan_processor.rs`, novo; imports explícitos como `projects.rs`):
```rust
pub(crate) struct PlanRequestProcessor { config: Arc<Config> }
pub(crate) async fn plan_list(&self, PlanListParams) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError>
pub(crate) async fn plan_read(&self, PlanReadParams) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError>
```
- `plan_list`: `thread_id` via `ThreadId::from_string` (erro ⇒ `invalid_params`); `cwd` relativo resolvido com
  `AbsolutePathBuf::relative_to_current_dir` (precedente `catalog_processor.rs:222`); `codex_plans::list_plans(&self.config.codex_home, ..)`
  (erro ⇒ `internal_error`); `limit` default 50, max 200 (`clamp`, como `projects.rs:71-77`); cursor opaco
  `"{updated_at}|{id}"` — `skip_while((updated_at, id) >= cursor)` + `take(limit)`; `next_cursor` só se sobrar.
- `plan_read`: `InvalidId` ⇒ `invalid_params`; `Io` ⇒ `internal_error`; `None` ⇒ `invalid_params("plan not found: {id}")` (precedente `projects.rs:103`).
- Wiring: `request_processors.rs` (`mod plan_processor;` + `pub(crate) use plan_processor::PlanRequestProcessor;`),
  `message_processor.rs` (import ~l.27; campo após `catalog_processor` l.142; construção após l.413-419
  `PlanRequestProcessor::new(Arc::clone(&config))`; literal `Self { .. }` l.564; arms após `ClientRequest::SkillsList` l.1353:
  `ClientRequest::PlanList { params, .. } => self.plan_processor.plan_list(params).await,` e idem `PlanRead`).
  O `match` é exaustivo — não compila até os arms existirem.

**Schema/TS** (obrigatório após mudar o protocolo; a receita `just write-app-server-schema` está partida — não há bin
`write_schema_fixtures`): a partir de `codex-rs/`:
`python app-server-protocol/scripts/write_schema_fixtures.py` e `python app-server-protocol/scripts/write_schema_fixtures.py --experimental`
(regeneram `schema/json/v2/Plan*.json`, `schema/typescript/v2/Plan*.ts`, `ThreadItem.ts`, `ClientRequest.*` e os dois
`schema/precomputed/*.zst`). Verificar com `just test -p codex-app-server-protocol` (`schema_fixtures_tests::*`).

**README** (`app-server/README.md`): bullets `plan/list` / `plan/read` após `skills/list` (l.255); `savedPath` na lista de
items (~l.1728); nova secção «Example: List and load a saved plan (fork extension)» antes de «Example: One-off command
execution» (~l.1411) com request/response JSON de `plan/list`, `plan/read` e o `turn/start` de handoff:
`"# Saved plan: {title} ({path})\n\n{markdown}\n\n## My request for Codex:\nImplement this plan."`.

**Testes** (`app-server/tests/suite/v2/`, harness `TestAppServer`, `mcp.request::<T>(|request_id| ClientRequest::X{..})` — sem novos `send_*`):
- `plan_item.rs` (existente): assertar `savedPath` no item completado (ficheiro existe, pai chama-se `plans`) e que
  `plan/read` desse id devolve `markdown == "# Final plan\n- first\n- second\n"`, `title == "Final plan"`, `threadId == thread.id`.
  Comparar existência/nome do pai, não prefixo exato (o `codex_home` do app-server é normalizado com dunce no Windows).
- Novo `plan_list.rs` (+ `mod plan_list;` em `v2/mod.rs`), modelo `collaboration_mode_list.rs`, fixtures escritos à mão no
  `TempDir`: newest-first + paginação (`limit: 1` → `nextCursor` → 2.ª página vazia de cursor); filtro `threadId`;
  `plan/read` sem front matter; id inválido ⇒ erro `-32602`; id desconhecido ⇒ `-32602`.

---

## Parte 4 — TUI: `/plans`, carregar plano, dica «Plan saved»

Módulos novos (fork-local): `tui/src/chatwidget/saved_plans.rs` (estado, popups, injeção) e `tui/src/app/plans_picker.rs`
(RPCs em background). Edições mínimas aos ficheiros existentes listadas em T1–T10.

**T1 — compile fixes pelo campo novo `saved_path`** (v2 `ThreadItem::Plan`): `tui/src/dynamic_tools.rs:1380`
(`ThreadItem::Plan { id, text, .. }`), literais em `tui/src/chatwidget/tests/slash_commands.rs:1828` e
`tui/src/resume_picker.rs:6338` (`saved_path: None`). (Os de `exec/` e `app-server/tests` estão na Parte 3.)

**T2 — `tui/src/slash_command.rs`**: `Plans,` na l.43 (logo após `Plan,` — ordem do enum = ordem no popup); arm em
`description` (`"browse saved plans and load one into this session"`); `| SlashCommand::Plans` no arm `=> false` de
`available_during_task` (l.209-230, junto a `Plan`). Sem mudanças em `supports_inline_args`/`available_in_side_conversation`/
`is_visible`; sem `BuiltinCommandFlags` (não há gate). `/plan` continua a resolver exatamente para `Plan`
(strum `from_str`; o popup ordena exact match antes de prefix match — `command_popup.rs:146-194`).

**T3 — `tui/src/app_event.rs`** (junto a `OpenResumePicker`, l.415):
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub(crate) enum SavedPlanAction { Implement, AttachToNextMessage }
OpenPlansPicker,
PlansPickerLoaded { request_id: Uuid, result: Result<Vec<PlanSummary>, String> },
OpenSavedPlanActions { id: String, title: String },        // SelectionAction só recebe &AppEventSender → 2.º popup via evento (como model → reasoning popup)
LoadSavedPlan { id: String, action: SavedPlanAction },
SavedPlanLoaded { request_id: Uuid, action: SavedPlanAction, result: Result<PlanReadResponse, String> },
```
(`PlanSummary`/`PlanReadResponse` têm de derivar `Debug` — já previsto na Parte 3.)

**T4 — `tui/src/chatwidget/saved_plans.rs`** (novo):
- `PendingPlanContext { id, title, path, markdown }`; `SavedPlansState { pending_context, picker_request_id: Option<Uuid>, load_request_id: Option<Uuid> }`
  (ids para descartar respostas obsoletas — sem campos novos em `App`).
- Builders puros (testáveis sem widget): `picker_loading_params()` (item «Loading…» desativado, `view_id: "plans-picker"`),
  `picker_error_params(err)`, `picker_params(&[PlanSummary], current_cwd, now_secs)` e `load_plan_params(id, title)`.
- Picker: `tabs` = «This project» (`Path::new(plan.cwd) == config.cwd`) e «All» (`SelectionTab { id, label, header, items }`,
  `bottom_pane/selection_tabs.rs:16`; com `tabs` o título vai no `header` de cada tab — `ColumnRenderable` como `settings_popups.rs`);
  `initial_tab_id` = «project» se houver planos do projeto, senão «all»; `is_searchable: true` — **todas** as rows com
  `search_value: Some("{title} {cwd} {id}")` (rows sem `search_value` desaparecem ao pesquisar — `list_selection_view.rs:506-519`),
  incluindo a row placeholder desativada da tab vazia. Row: `name = title`, `description = "{updated_at relativo} · {basename(cwd)} · rev N"`,
  ação → `AppEvent::OpenSavedPlanActions`, `dismiss_on_select: false` + `dismiss_parent_on_child_accept: true` (o picker fica
  por baixo do 2.º popup; Esc no 2.º popup volta ao picker; aceitar fecha os dois — `bottom_pane/mod.rs:570-591`).
  `format_relative_time` duplicado (10 linhas; o de `resume_picker.rs` é privado).
- Popup «Load plan «{title}»?»: 2 rows — «Implement this plan» (Switch to Default mode and start coding) e «Attach to my next
  message» (Include the plan as hidden context with what you type next) → `AppEvent::LoadSavedPlan { id, action }`,
  `dismiss_on_select: true`. Sem row «Cancel» (seria `Accepted` e fechava o picker também; Esc é o cancelar).
- `render_plan_context(ctx) = format!("# Saved plan: {title} ({path})\n\n{markdown.trim_end()}\n")`.
- `impl ChatWidget`: `begin/finish_plans_picker_request`, `apply_plans_picker_result` (vazio ⇒ `dismiss_view_by_id("plans-picker")`
  + `add_info_message("No saved plans yet.", Some("Plans you approve in Plan mode are saved to ~/.codex/plans."))`;
  ok ⇒ `replace_selection_view_if_present` — se o user já fechou o picker o resultado cai silenciosamente; erro ⇒ substitui por
  `picker_error_params` + `add_error_message`), `show_saved_plan_actions`, `begin/finish_saved_plan_load`,
  `apply_loaded_plan(plan, action)`, `maybe_apply_pending_plan_context(&mut items)`, `on_plan_item_saved(path)`, `pending_plan_context()` (`#[cfg(test)]`).
- `apply_loaded_plan`: guard `blocks_direct_input` ⇒ `PARENT_OWNED_INPUT_MESSAGE`; guarda `pending_context`;
  `add_info_message("Loaded plan «{title}»", Some(path))`; `Implement` ⇒ `default_mode_mask(model_catalog)` (None ⇒ erro
  `PLAN_IMPLEMENTATION_DEFAULT_UNAVAILABLE`, contexto fica anexado) e, se `is_user_turn_pending_or_running()` (uma mensagem em
  fila pode ter arrancado um turno enquanto o `plan/read` estava em voo — `submit_user_message_with_mode` recusa trocar de modo com
  turno a correr, `input_flow.rs:268-275`) ⇒ degrada para «attached»; senão
  `submit_user_message_with_mode(PLAN_IMPLEMENTATION_CODING_MESSAGE /*"Implement the plan."*/, default_mask)`.
  `AttachToNextMessage` ⇒ info «Plan «{title}» attached — it will be included with your next message.»
- `on_plan_item_saved(path)`: uma linha dim via `add_plain_history_lines` — `• Plan saved to <path> — use /plans to load it in another session.`

**T5 — wiring do módulo**: `chatwidget.rs` (após `mod plan_implementation;` l.385: `pub(crate) mod saved_plans;`; campo
`saved_plans: SavedPlansState` após `ide_context` l.636) e `chatwidget/constructor.rs:174` (`SavedPlansState::default()`).

**T6 — `chatwidget/slash_dispatch.rs`** (após o arm `Plan`, l.305-307):
`SlashCommand::Plans => { if self.blocks_direct_input { add_error_message(PARENT_OWNED_INPUT_MESSAGE); return; } self.app_event_tx.send(AppEvent::OpenPlansPicker); }`
(bloqueio com turno a correr vem de graça via `available_during_task == false`; **não** chamar `defer_input_until_settings_applied`).

**T7 — `tui/src/ide_context/prompt.rs`**: novo `pub(crate) fn prepend_prompt_context(items: &mut Vec<UserInput>, context_text: &str)`
(re-export em `ide_context.rs:9-11`): se o 1.º `UserInput::Text` já contém `PROMPT_REQUEST_BEGIN` (IDE context aplicado antes),
insere `"{context}\n"` imediatamente antes do último delimitador e desloca os `text_elements` com `start >= delimiter_start`
(`TextElement::map_range`, como `user_messages.rs:683`); senão reutiliza `prefixed_text_input(format!("{context}\n{PROMPT_REQUEST_BEGIN}\n"), ..)`;
sem item de texto ⇒ insere um em `items[0]`. Resultado: **exatamente um** delimitador; texto final
`# Saved plan: T (P)\n\n{markdown}\n\n## My request for Codex:\nImplement the plan.`. `apply_ide_context_to_user_input` fica intocado.

**T8 — `chatwidget/input_submission.rs:328`**: logo após `self.maybe_apply_ide_context(&mut items);` →
`self.maybe_apply_pending_plan_context(&mut items);` (IDE primeiro, plano depois). O display otimista e o item committed
já removem tudo até ao último delimitador (`user_messages.rs:662-696`, dedupe em `chatwidget.rs:1344-1347`), o `message_history`
guarda só o texto do user; o Desktop e o export usam a mesma regra.

**T9 — `tui/src/app/plans_picker.rs`** (novo; `mod plans_picker;` em `app.rs` ~l.228) + 5 arms em `app/event_dispatch.rs` junto a
`OpenAgentPicker` (l.2552): `OpenPlansPicker => self.open_plans_picker(app_server)`, `PlansPickerLoaded => self.apply_plans_picker_result(..)`,
`OpenSavedPlanActions => self.chat_widget.show_saved_plan_actions(id, title)`, `LoadSavedPlan => self.load_saved_plan(app_server, id, action)`,
`SavedPlanLoaded => self.apply_saved_plan_loaded(..)`. Padrão de `app/agent_picker.rs:26-85`: `app_server.request_handle()`
(`AppServerRequestHandle`, cloneable) + `tokio::spawn` + `request_typed::<PlanListResponse>(ClientRequest::PlanList { request_id: RequestId::String(format!("plan-list-{}", Uuid::new_v4())), params: PlanListParams { cursor, limit: Some(100), thread_id: None, cwd: None } })`
paginando até 500 planos (`seen_cursors` como no agent picker), erro ⇒ `err.to_string()`; `plan/read` idem com `PlanReadParams { id }`.
O filtro «This project» é client-side (uma só fetch alimenta as duas tabs).

**T10 — dica «Plan saved»** (`chatwidget/replay.rs:128`):
`ThreadItem::Plan { text, saved_path, .. } => { self.on_plan_item_completed(text); if !from_replay && let Some(path) = saved_path { self.on_plan_item_saved(path); } }`
— **não** muda a assinatura de `on_plan_item_completed` (13 call sites em testes). Sem dica em replay/resume (cada revisão reescreve o
mesmo ficheiro; um thread com N revisões repetiria a linha N vezes; `/plans` é o caminho de descoberta). A linha é emitida depois de
`on_plan_item_completed` devolver, portanto fica depois do `ConsolidateProposedPlan` (ordem de eventos preservada).

**T11 (opcional, polish)** — indicador no footer do composer `plan: <title>` enquanto há contexto pendente (espelhar
`set_ide_context_active`: `footer_state.rs:28`, `chat_composer.rs:960/1403-1424`, `bottom_pane/mod.rs:477`). Só é visível com a
status line ativa (`chat_composer.rs:4720-4731`), tal como o «IDE context» — a linha de info no histórico é o feedback principal.

**Testes** (novo `tui/src/chatwidget/tests/saved_plans.rs` + `mod saved_plans;` em `chatwidget/tests.rs` entre `review_mode` e `side`;
helpers: `make_chatwidget_manual`, `next_submit_op`, `drain_insert_history`, `render_bottom_popup`, `complete_user_message_for_inputs`,
`normalize_snapshot_paths`; snapshots em `tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__<nome>.snap`):
1. `slash_commands.rs`: `dispatch_command(Plans)` ⇒ `AppEvent::OpenPlansPicker`; bloqueado com task a correr; bloqueado com `blocks_direct_input`.
2. `picker_params`: tabs/`initial_tab_id`, `search_value` em todas as rows, `description` com tempo relativo (`now_secs` injetado ⇒ determinístico).
3. Snapshots `plans_picker_loading`, `plans_picker_loaded`, `saved_plan_actions_popup`, `saved_plan_user_cell`, `plan_saved_hint`.
4. Pesquisa filtra rows; resultado vazio fecha o picker e informa; erro substitui a view e reporta; resposta obsoleta ignorada (`finish_*` false).
5. Picker → Enter ⇒ `OpenSavedPlanActions`; popup → Enter ⇒ `LoadSavedPlan{Implement}`, Down+Enter ⇒ `AttachToNextMessage`.
6. `apply_loaded_plan(.., Implement)` com `chat.thread_id = Some(..)`, `Feature::CollaborationModes` on e máscara Plan ativa ⇒
   `Op::UserTurn` com 1 item de texto que começa por `# Saved plan: ` e acaba em `## My request for Codex:\nImplement the plan.`
   (delimitador ocorre 1×), `collaboration_mode` Default, `pending_plan_context()` limpo, `active_collaboration_mode_kind() == Default`.
7. `AttachToNextMessage` + «do it» ⇒ texto = `render_plan_context + "\n## My request for Codex:\ndo it"`; 2.ª mensagem «again» sem prefixo.
8. Item committed com prefixo ⇒ a célula do user mostra só «Implement the plan.».
9. `ide_context/prompt.rs` tests: `prepend_prompt_context` com IDE context já aplicado ⇒ 1 delimitador, ordem IDE → plano → delimitador → pedido,
   `text_elements` deslocados; caso sem delimitador; caso sem item de texto.
10. `handle_server_notification(ItemCompleted{ Plan{ saved_path: Some(..) } })` ⇒ linha «Plan saved to …»; via replay ⇒ ausente.
11. `slash_command.rs`: `from_str("plan") == Plan`, `from_str("plans") == Plans`, `!Plans.available_during_task()`.

---

## Ordem de execução

1. **Crate `codex-plans`** (Parte 1) + `Cargo.toml` workspace → `cargo build -p codex-plans` → `just test -p codex-plans`.
2. **Protocolo core + hook** (Parte 2): `protocol/src/items.rs`, `core/Cargo.toml`, `turn.rs` → `cargo build -p codex-core`.
3. **app-server-protocol** (Parte 3): `v2/plan.rs`, `v2/mod.rs`, `common.rs`, `v2/item.rs` → `cargo build -p codex-app-server-protocol`.
4. **Compile fixes** do campo novo (Parte 3 lista + T1) → `cargo check --workspace --tests`.
5. **Handler app-server** (Parte 3) → `cargo build -p codex-app-server`.
6. **Schema/TS**: os dois `write_schema_fixtures.py` → `just test -p codex-app-server-protocol`.
7. **TUI** T2–T10 (+T11 se quiser) → `cargo build -p codex-tui`.
8. **Testes**: Parte 1 (unit), Parte 3 (`just test -p codex-app-server plan_`), TUI (`just test -p codex-tui saved_plans`,
   `slash`, `plan_mode`, `ide_context`) → rever `*.snap.new` (`cargo insta pending-snapshots -p codex-tui`) → `cargo insta accept -p codex-tui`.
9. **README** do app-server (Parte 3) + `just fmt` + `just fix -p codex-plans -p codex-core -p codex-app-server-protocol -p codex-app-server -p codex-tui`.
10. `Cargo.lock` commitado; `just bazel-lock-update` (`MODULE.bazel.lock`) só se o Bazel estiver instalado — o gate real do fork é
    cargo check/test (CI upstream já está vermelha por outros motivos). Deploy/hot-swap do binário e commit **só se pedidos**.

## Verificação end-to-end

1. `cd codex-rs && cargo build -p codex-cli` (ou o alvo de release habitual), depois `./target/debug/codex --help` para confirmar que arranca.
2. TUI real com `CODEX_HOME` temporário: `$env:CODEX_HOME = "C:\tmp\codex-home-plans"` (copiar `auth.json`/`config.toml` do
   `~/.codex`), `codex`, `/plan`, pedir um plano pequeno («plan a hello-world script»), esperar pelo `<proposed_plan>` →
   ver `• Plan saved to C:\tmp\codex-home-plans\plans\<ts>-<slug>.md` + popup «Implement this plan?» → «No, stay in Plan mode»;
   abrir o ficheiro e confirmar front matter + corpo; pedir uma revisão → mesmo ficheiro, `revision: 2`; pergunta de esclarecimento
   («why step 2?») ⇒ ficheiro intocado.
3. Nova sessão (`codex`), `/plans` → tabs «This project»/«All», pesquisa, Enter → «Attach to my next message» → escrever «summarize
   the plan in one line» → a célula do user mostra só o pedido; a resposta prova que o modelo recebeu o plano. Repetir com
   «Implement this plan» → modo muda para Default e o turno arranca com «Implement the plan.». Ctrl+T (transcript) e `/export`
   também mostram só o pedido.
4. app-server manual (Desktop-equivalent): `codex app-server` por stdio (ou `codex-rs/app-server-test-client` se preferir) —
   `initialize`, `plan/list {}`, `plan/list {"cwd": "<repo>"}`, `plan/read {"id": "<id>"}`, `plan/read {"id": "../x"}` ⇒ `-32602`;
   depois `thread/start` + `turn/start` com o texto de handoff do README e confirmar `item/completed` do user message no fluxo.
5. `just test -p codex-plans`, `just test -p codex-app-server-protocol`, `just test -p codex-app-server plan_`, `just test -p codex-tui`
   (Windows: `RUST_MIN_STACK` já vem do `just test`). Comparar falhas novas com o catálogo de test debt conhecido antes de diagnosticar.

## Riscos e notas

- **Compat de dados persistidos**: `saved_path` com `#[serde(default)]` no core (rollouts) e no v2 (SQLite do thread-store);
  rollouts antigos ⇒ `None`/`null`. `codex exec --json` passa a emitir `savedPath` nos items `plan` (só aparece quando `Some` no
  core por `skip_serializing_if`; no v2 aparece sempre) — sem testes que fixem o JSON exato.
- **`#[serde(flatten)]` em `PlanReadResponse`**: precedente em `v2/plugin.rs:544`; se o TS/JSON gerado ficar estranho, aninhar em `plan:` e
  ajustar o TUI (`plan.plan.title`).
- **Scan de diretório a cada plano completado** (para achar o ficheiro da thread): centenas de ficheiros de poucos KB — desprezível;
  documentar no crate; índice fica para depois se alguma vez for preciso.
- **Concorrência**: threads distintas → ficheiros distintos; colisão só com mesmo segundo+slug (sufixo `-2`); escrita atómica.
- **Windows**: nomes sem `:`; `codex_home` do app-server normalizado com dunce ⇒ nos testes comparar existência/nome do pai, não prefixo.
- **Higiene do fork**: comentários `// FORK:` no core (como `session.rs:73-89`) e «Fork extension (codex fork)» nos doc comments do
  protocolo/README; tudo o resto em ficheiros novos. Após o próximo sync upstream, verificar `turn.rs` (hook), `common.rs` (entries),
  `replay.rs:128` e `input_submission.rs:328` (hooks de 1 linha).
- **Contexto do modelo**: o plano entra como texto do user (client-side, como o IDE context, que já admite 40k chars de seleção);
  planos reais têm 0,8–4k palavras. Se algum dia for preciso, capar em `render_plan_context` com aviso de truncagem.
- **Follow-ups possíveis (fora de scope)**: `plan/load` server-side para o Desktop; apagar/renomear planos (`plan/delete`); mostrar
  preview lateral no picker (`side_content`); ligar o «Implement this plan?» pós-plano a `/plans` (já está coberto pelo `saved_path`).
