# Sync upstream 02/09 — análise e plano

> Somente leitura até à Fase 1 do plano: **o merge não foi iniciado**. Toda a análise de conflito
> saiu de `git merge-tree --write-tree` (árvores `cca47f96cb` e, com `-X ignore-space-change`,
> `e83101bcd1`), que produzem o resultado do merge sem tocar na árvore de trabalho.
>
> Revisão 2 (02/09, fim de tarde). A revisão 1 cobria 128 commits e deixava duas decisões em
> aberto. Esta revisão: (a) actualiza para os 138 commits actuais, (b) fecha as duas decisões com
> os dados que faltavam (auditoria de leitores + inventário dos módulos do fork), (c) acrescenta o
> incidente do cache de plugins do Desktop, que o upstream corrigiu hoje, e (d) inclui o plano de
> execução completo com os 29 conflitos resolvidos um a um.

## 0. Situação

| | |
|---|---|
| base upstream do fork | `88f776588f` (30/08) |
| upstream agora | `f59905647a` (02/09, #42330) |
| commits novos | 138 (os 10 desde a revisão 1 não acrescentam conflitos; hunks idênticos) |
| ficheiros em conflito | 29 (40 hunks) — 11 mecânicos, 16 costura, 2 com decisão (fechadas abaixo) |
| CI upstream | **cronicamente vermelho**, 36–64 checks falhados em cada um dos 5 últimos commits (`Build nextest archive`, Bazel, sdk, codespell). Mesma classe de sempre: gate real é local. |
| árvore do merge-tree | `cca47f96cb3ca02f71dd1b82be3987d0f49aae56` (`git show <árvore>:<caminho>` mostra o ficheiro com marcadores) |
| árvore c/ `ignore-space-change` | `e83101bcd1` — mesma lista de ficheiros, mas `rollout_reconstruction.rs` cai de 6 para 2 hunks |

**Passa intacto:** `core/src/ownership/`, `state/src/workflow/lease*`, `agent-roles/`, o carve-out
`model_provider` em `core/src/agent/role.rs`, `login/src/auth/manager.rs` (bloco FORK), o pin
`0.146.1`, `model-provider-info` (nenhum `match` novo sobre `WireApi`). Nenhuma das classes
recorrentes de quebra pós-merge (`ToolHandler::handle`, `ResponseEvent::Completed`,
`request_command_approval`, `ReasoningEffort`) reaparece nesta janela.

### Decisões (resumo)

| tema | decisão recomendada | secção |
|---|---|---|
| Compressão de rollouts | **C′**: manter a estrutura do fork (worker/journal/validação/writer/cleanup), remover o modo + gate + índice de referências, como o upstream fez | §1.9 |
| `/agents` overview | **A**: combinar — desvio para a frota quando há thread primária; caminho upstream (sessões recentes) como fallback | §2.6 |
| `[tools.update_plan]` | já decidido: ambos os lados; config explícita blindada (`config.toml.bak-20260902`) | rev. 1 |
| `tool_log_payload` | já decidido: manter `tool_name` + tags de sandbox do upstream | §4 #11 |
| Cache de plugins (Desktop) | **nenhum patch do fork**: o upstream `50fffd5ed3` (#42284) resolve; entra com o sync | §3 |

---

# Parte 1 — Compressão de rollouts

## 1.1 O problema que a compressão cria

O worker varre `~/.codex/sessions/` e `~/.codex/archived_sessions/` e converte rollouts frios
(>7 dias) de `.jsonl` para `.jsonl.zst`. O caso delicado são as **linhagens partilhadas**: quando
B é fork de A, o `SessionMeta` de B guarda um `history_base` com `end_byte_offset` — um
deslocamento em bytes dentro do ficheiro de A. Comprimir A invalida esse offset para qualquer
leitor que abra o ficheiro directamente; só funciona se o leitor passar pelos helpers zstd-aware.

Não é hipotético: no mesmo commit em que passou a comprimir linhagens partilhadas o upstream teve
de corrigir um leitor directo em `exec/src/lib.rs` (`codex exec resume` a determinar o cwd do
último turno).

## 1.2 Como o fork resolve hoje

- `RolloutCompressionMode { Standalone, IncludeShared }` (`rollout/src/compression.rs:41`).
- `RolloutCompressionCapabilities { cargo, bazel, tui, app_server, desktop: Option<bool> }`
  (`compression_capabilities.rs`); `all_readers_support_shared()` exige tudo `true` e
  `desktop == Some(true)`; `Default` recusa.
- Gate no worker (`compression_worker.rs:128`): `IncludeShared` sem capabilities **aborta a
  rodada inteira**. Em `Standalone`, dois filtros (`:298-307`) saltam rollouts referenciados
  (`RolloutReferenceIndex`) e rollouts com `history_base`.
- Invariante `fork_invariant_include_shared_fails_closed_without_reader_capabilities`.

**Facto novo (rev. 2):** o único call site (`core/src/thread_manager.rs:393-430`) chama
`spawn_rollout_compression_worker(codex_home, mode)` — a variante `_with_capabilities` **nunca é
chamada em produção**. Logo `IncludeShared` é inatingível por construção; o modo só tinha uma
configuração possível. Isto responde às perguntas 3 e 4 da rev. 1.

## 1.3 O que o upstream fez

`9d0eae74cd` — *Include shared histories in rollout compression (#42039)*: apagou o enum, o
parâmetro de modo, a leitura do índice de referências no worker (`rollout_reference_index.rs`
perdeu 133 linhas) e os dois `skipped_*`. Resta "comprime todo o ficheiro frio". A segurança passa
a depender de "todo o leitor usa o reader zstd-aware". O flag `local_thread_store_shared_compression`
continua aceite em config estrita, sem efeito.

## 1.4 Estado real na máquina

Compressão **desligada**: ambos os flags `Stage::UnderDevelopment, default_enabled: false`
(`features/src/lib.rs:1108-1121`), nenhum ligado em `~/.codex/config.toml`. Nenhum rollout foi
comprimido até hoje. Tudo aqui é código dormente.

## 1.5 A premissa do gate (Desktop), verificada — rev. 1

No bundle `OpenAI.Codex_26.831.2377.0` (`app.asar`, 284,7 MB): as 20 ocorrências de `.zst` são
da lib `tar`; zero `readFile` perto de `rollout`; os únicos caminhos de rollout que o Desktop toca
são o staging de import de transcript e o handoff, nenhum sob `sessions/`. A leitura normal de
thread passa pelo app-server (o nosso binário). Limite honesto: provei ausência de leitor zstd num
bundle minificado, não a ausência de qualquer leitura directa; vale para 26.831.

## 1.6 Auditoria dos leitores do fork — rev. 2 (feita)

Varrimento completo de `codex-rs` (e `scripts/`) por leituras de rollout, classificadas por
"passa pelo reader zstd-aware" vs "abre o ficheiro directamente".

**Entradas zstd-aware em `rollout/src/`:** `open_rollout_line_reader` (`compression.rs:82`, só
linhas, sem seek), `open_rollout_seekable_reader` (`seekable_reader.rs:46`, descomprime para
tempfile anónimo; **seeks por offset original funcionam**), `rollout_contains_prefix(path,
end_byte_offset)` (`seekable_reader.rs:63`), `existing_rollout_path`, `plain_rollout_path`,
`materialize_rollout_for_append/_reference`, `RolloutFile::from_path`. Todos os derivados
(`read_session_meta_line`, `load_rollout_items`, `get_rollout_history`, `search_rollout_matches`,
`extract_metadata_from_rollout`, `RolloutReferenceIndex::scan`) assentam nestes.

**Resultado nos módulos exclusivos do fork — nenhum leitor directo:**

| módulo | o que lê | como |
|---|---|---|
| `thread-store/src/local/model_context.rs:135` | linhagem com `end_byte_offset` | `open_rollout_seekable_reader` + `ReverseJsonlScanner::new_at` — **o principal consumidor do offset, e está correcto** |
| `thread-store/src/local/{thread_history_materialization,recovery_scan,rollout_lineage,revert_thread,search_index_projection,receipt_append,read_thread,search_threads}.rs` | vários | seekable reader / line reader / `rollout_contains_prefix` / materialize |
| `thread-store/src/local/rollout_migration*` | migração | 7 `File::open` directos, **todos sobre a cópia já descomprimida** (`rollout_path_is_compressed` em `:821/:849-867`; `preview.rs:515-519` ramifica explicitamente) |
| `app-server` evidence/artifact/recovery/lease/fleet/search-index | tudo via `thread_store.read_thread` | sem IO próprio |
| `core/src/chatgpt_web/`, `core/src/claude_code/` (incl. `sessions.rs`), `state/src/workflow/`, `agent/control/fleet.rs`, `core/src/ownership/`, `memories/`, `session/rollout_reconstruction.rs` | — | **não abrem rollouts** (estado de daemon, JSON de contas, SQLite, `&[RolloutItem]` em memória) |
| `cli/src/doctor/thread_inventory.rs:493-528` | inventário | normaliza com `plain_rollout_path`, dedupe `.zst`/`.jsonl` |
| `scripts/`, `codex-rs/scripts/` | — | nenhum parser de rollout |

**Leitores directos que sobram — ambos em ficheiros partilhados com o upstream, ambos fail-soft:**

| ficheiro | estado | consequência |
|---|---|---|
| `exec/src/lib.rs:1625` `parse_latest_turn_context_cwd` — `tokio::fs::read_to_string(path)` | **corrigido pelo upstream nesta janela** (`9d0eae74cd`: `open_rollout_seekable_reader` + `ReverseJsonlScanner`) — o sync traz a correcção | — |
| `tui/src/resume_picker_transcript_preview.rs:152` `scan_legacy_transcript_preview` — `File::open(path)?` | **igual no upstream** (`:153`), não tocado | em rollout comprimido dá `NotFound` → cai em `hydrate_initial_thread_history`; perde-se só o tail scan rápido |
| `cli/src/doctor.rs:2339` `is_rollout_file` — `extension() == "jsonl"` | igual no upstream (`:2189`) | rollouts `.zst` saem das estatísticas de tamanho/contagem do `doctor` (metadata, sem leitura de conteúdo) |

**Conclusão:** a pergunta 1 da rev. 1 ("algum leitor exclusivo do fork abre rollout sem passar
pelo reader?") tem resposta **não**. A premissa do gate está vazia dos dois lados: nem o Desktop
nem o fork leem rollouts directamente. O residual é upstream-partilhado e falha suave.

## 1.7 O que o fork tem que o upstream não tem (inventário — rev. 2)

`5167390020` (subsistema de workflow) reescreveu o worker em módulos, e **não é só extracção**:

| módulo | linhas | o que faz |
|---|---|---|
| `compression_worker.rs` | 381 | o worker (equivalente ao `mod worker` inline do upstream) + gate |
| `compression_writer.rs` | 206 | `compress_rollout_if_cold` — escrita atómica com medição |
| `compression_validation.rs` | 272 | `validate_rollout_replacement` / `inspect_rollout` — verifica que o `.zst` reproduz o JSONL antes de substituir |
| `compression_journal.rs` | 219 | journal por ficheiro + `recover()` — retoma/reverte uma substituição interrompida |
| `compression_cleanup.rs` | 110 | limpeza de temporários/journals órfãos |
| `compression_capabilities.rs` | 73 | o gate |

Fora do crate, só `core/src/thread_manager.rs` referencia estes tipos (o modo). Os 28 testes de
compressão do fork (P2.3) cobrem journal/validação/recuperação.

## 1.8 Opções

| | fica | perde-se | custo |
|---|---|---|---|
| **A. Manter gate, portar worker deles** | tudo do fork | simplificação | conflito recorrente a cada sync num gate que nunca é atingível |
| **B. Pegar o deles inteiro** | convergência total | modo, capabilities, invariante **e os ~1000 linhas de journal/validação/writer/cleanup** | baixo agora; perde robustez real da P2.3 |
| **C′. Estrutura do fork, semântica do upstream** | journal/validação/writer/cleanup, worker extraído | modo, capabilities, gate, `skipped_referenced`/`skipped_fork_pointer`, invariante | portar as **remoções** do upstream para `compression_worker.rs` (~40 linhas); adaptar testes |

## 1.9 Decisão: C′

Fundamentos: (1) a auditoria mostra que o gate protege contra um leitor que não existe; (2) o
modo `IncludeShared` nunca foi atingível, logo o gate nunca fez diferença; (3) os módulos de
journal/validação são robustez que o upstream não tem e que a P2.3 testou; (4) o
`rollout_reference_index.rs` encolhido do upstream merge limpo e o único consumidor de
`scan_until`/`reference_count` era o worker do fork (risco pós-merge #10 do §4 desaparece).

O que muda concretamente:

- `compression.rs`: tomar o lado upstream do hunk 1 (`spawn_rollout_compression_worker(codex_home)`),
  mas no hunk 2 manter `#[path = "compression_worker.rs"] mod worker;` (rejeitar o `mod worker { … }`
  inline do upstream). Apagar `RolloutCompressionMode`.
- `compression_worker.rs` (sem conflito, edição manual): `spawn(codex_home)` / `run(codex_home)`;
  remover `capability_blocked` (`:128-161`), o `RolloutReferenceIndex::scan_until` (`:165`), os
  parâmetros `reference_index`/`mode` de `compress_rollouts_in_root`, os dois skips (`:298-307`);
  manter `skipped_unreadable_meta` como o upstream (`read_session_meta_line(...).is_err()`).
- Apagar `compression_capabilities.rs`; `lib.rs`: tomar o lado upstream (sem os dois `pub use`).
- `compression_tests.rs`: substituir `worker_compresses_archived_fork_chain_only_with_shared_mode`
  pelo `worker_compresses_archived_fork_chain` do upstream (comprime ambos, verifica round-trip
  com `open_rollout_seekable_reader`); apagar o invariante `fork_invariant_include_shared_fails_closed…`
  e `worker_skips_source_referenced_by_archived_compressed_rollout`; manter todos os testes de
  journal/validação/recuperação. Actualizar `fork-invariants.toml` se listar o invariante removido.
- `core/src/thread_manager.rs`: o merge-tree já resolveu o call site para a forma upstream
  (`:414/:418`); apagar `use codex_rollout::RolloutCompressionMode;` (`:72`) e o cálculo de
  `compression_mode` (`:393-400`).
- `features/src/lib.rs`: tomar o upstream — `LocalThreadStoreSharedCompression` fica como flag
  removido/aceite ("Removed compatibility flag").
- README do app-server (secção "compression gating" acrescentada na P2): actualizar para dizer que
  o gate deixou de existir.

Follow-ups opcionais (não bloqueiam; candidatos a PR upstream): trocar `File::open` em
`resume_picker_transcript_preview.rs:152` por `open_rollout_seekable_reader`; fazer
`doctor::is_rollout_file` normalizar com `plain_rollout_path` como `thread_inventory.rs` já faz.

---

# Parte 2 — `tui/src/app/agents_overview.rs`

## 2.1 Escala real

O fork mudou **28 linhas** em `agents_overview.rs`; o dashboard de frota vive em
`agents_fleet.rs` (521 linhas, sem conflito). As três inserções do fork: campo
`fleet: AgentsFleetState` no estado; desvio no topo de `open_agents_overview` (com thread
primária → `open_agents_fleet_overview`; sem ela → selection view *"No fleet root selected"*);
early-return em `refresh_agents_overview_threads` quando `fleet.root_thread_id.is_some()`.

O fork **já tem** o guard `AppServerTarget::Embedded` do upstream antes do desvio
(`agents_overview.rs:46`), portanto o desvio só corre com app-server remoto.

## 2.2 O que o upstream fez (4 commits: `f40e08478c` #42104, `746798b2f7` #41918, `9d57be71ba` #42202, `a7913390f7` #41911)

- Estado novo: `threads: HashMap<ThreadId, Option<Thread>>`, `initialized`, `refresh_thread_ids`,
  `refresh_task: Option<AbortHandle>`, `refresh_notifications`, `input_states`,
  `dispatched_requests`; `impl Drop` aborta a `refresh_task`.
- **Facto novo (rev. 2):** `refresh_agents_overview_threads` **saiu deste ficheiro** para o novo
  `tui/src/app/agents_overview_threads.rs` (336 linhas, sem conflito), decomposta em
  `refresh_agents_overview_threads` (`:104`), `refresh_changed_agents_overview_threads` (`:111`),
  `start_agents_overview_refresh` (`:126`) e `track_agents_overview_notification` (`:37`).
- `open_agents_overview` upstream: guard Embedded → constrói a view a partir de
  `agents_overview.threads` → `show_bottom_pane_view` → `refresh_agents_overview_threads`.

## 2.3 Os dois hunks de conflito

1. Cabeçalho `//!` — combinar as duas frases.
2. Corpo: lado fork = desvio `primary_thread_id` + `request_id = None; refresh_pending = false;` +
   `open_agents_fleet_overview` + **todo o corpo antigo** de `refresh_agents_overview_threads` (que
   já não deve existir aqui); lado upstream = os 4 statements novos de `open_agents_overview`.

## 2.4 Opções

| | resultado | custo |
|---|---|---|
| **A. Combinar** | `/agents` vai para a frota com thread primária; sem ela cai na visão de sessões recentes do upstream (em vez de "No fleet root selected") | reposicionar o desvio; 1 early-return no ficheiro novo |
| B. Só o nosso | comportamento actual | perde 4 commits; o conflito volta maior |
| C. Só o deles | convergência | `agents_fleet.rs` fica órfão |

## 2.5 Perguntas da rev. 1, respondidas

1. *Sem `primary_thread_id`, mostrar a mensagem ou a visão upstream?* → a visão upstream: é útil
   e custa apagar um bloco. (Decisão de produto menor; reversível.)
2. *O `Drop` que aborta a `refresh_task` precisa de correr na rota da frota?* → não: o desvio
   retorna antes de `start_agents_overview_refresh`, a task nunca é criada. Mas
   `track_agents_overview_notification` continua a acumular `refresh_thread_ids` a partir de
   notificações; com o early-return em `start_agents_overview_refresh` isso é inofensivo.
3. *`agents_fleet.rs` duplica o #42104?* → não: a frota lista membros duráveis de um root com
   suspend/resume/close por geração (CAS no app-server); o #42104 lista sessões recentes do daemon.
   Fontes diferentes; unificar não se justifica agora.

## 2.6 Decisão: A

- `agents_overview.rs`: tomar o ficheiro upstream; em `open_agents_overview`, depois do guard
  Embedded, inserir `if let Some(root_thread_id) = self.primary_thread_id { self.open_agents_fleet_overview(app_server, root_thread_id); return; }`
  (sem o bloco "No fleet root selected", sem tocar em `request_id`/`refresh_pending`); manter o
  campo `fleet` em `AgentsOverviewState`.
- `agents_overview_threads.rs` (ficheiro novo, sem conflito): no topo de
  `start_agents_overview_refresh`, `if self.agents_overview.fleet.root_thread_id.is_some() { return; }`.
- Testes: `agents_fleet_tests.rs` (3 testes fixam `fleet.root_thread_id`) devem continuar a passar;
  o gate TUI da P2.3 (11 testes) + `agents_overview_tests.rs` do upstream (516 linhas novas).

---

# Parte 3 — Cache de plugins preso no app-server do Desktop (incidente de 02/09)

## 3.1 Diagnóstico (confirmado)

- O app-server do Desktop (PID de 11:16) lançava o plugin `spine-workbench@personal` a partir de
  `~/.codex/plugins/cache/personal/spine-workbench/<versão-antiga>`, que uma reinstalação feita por
  outro processo (15:37) tinha **esvaziado mas não apagado** (os `node` MCP vivos tinham cwd lá).
- `PluginsManager::loaded_plugins_cache` é indexado por `PluginLoadCacheKey` =
  `{configured_plugins, skill_config_rules, remote_global_catalog_active, auth_identity}`
  (`core-plugins/src/manager.rs:563-585`). **Não inclui a versão activa no disco.** Só é limpo por
  `clear_cache()` em mutações feitas *via* app-server; não há watcher.
- `LoadedPlugin.root` vem de `store.active_plugin_installation(id).root` no momento do load
  (`core-plugins/src/loader.rs:822-834`) e os MCP servers do plugin são carregados a partir desse
  `plugin_root` (`loader.rs:1398-1420`, `:1546+`) — logo o cwd dos `node` é o root cacheado.
- Na thread `01a05582…` (retomada 15:55, ~20 tentativas) o `node ./server/index.mjs` morre com
  *Cannot find module* e o rmcp devolve `TransportClosed` → *connection closed: initialize response*;
  como o plugin é `required: true`, a sessão aborta.

## 3.2 O upstream corrigiu exactamente isto hoje

`50fffd5ed3` — *Refresh plugin skills after out-of-process version changes (#42284)*, 02/09 14:03 UTC:

```rust
// LoadedPluginsCache::get(&mut self, key, store: &PluginStore)
if entry.plugins.iter().any(|plugin| {
    let Ok(plugin_id) = PluginId::parse(&plugin.config_name) else { return false };
    let installed_root = store.active_plugin_root(&plugin_id)
        .unwrap_or_else(|| store.plugin_base_root(&plugin_id));
    installed_root != plugin.root
}) { return None; }
```

Cobre o nosso caso: `active_plugin_root` (`store.rs:169-195`) reenumera as versões no disco a
cada chamada e escolhe a mais alta (ou `DEFAULT_PLUGIN_VERSION`); a nova `…183714` > antiga
`…164447`, logo `installed_root != plugin.root` e a entrada é rejeitada → reload → MCP com cwd
certo. Aplica-se aos dois pontos de consumo (`plugins_for_config` e
`plugin_skill_snapshots_for_config`). Também traz um LRU de 32 snapshots de skills.

- Está **dentro da janela do sync** e `core-plugins/` não conflita.
- `git apply --check --3way` do patch sobre o HEAD actual: **limpo** (4 ficheiros). Se o sync
  atrasar, `git cherry-pick 50fffd5ed3` é uma opção isolada.

## 3.3 Mitigação imediata (sem código)

Reiniciar o Codex Desktop (mata o app-server e os dois `node` órfãos; a pasta vazia pode então
ser removida). Alternativa: qualquer mutação de config pela UI do Desktop chama `clear_cache()`.
De qualquer forma, o deploy do binário novo (Fase 6) exige reiniciar o Desktop.

## 3.4 Endurecimento opcional (fork, baixo valor)

`active_plugin_version` não valida que a versão escolhida tem manifesto. Hoje é inofensivo (a
pasta esvaziada é sempre a *antiga*; se fosse a mais recente, `remove_old_plugin_versions` já
falha alto via `old_plugin_version_would_stay_active`). Se quisermos: ignorar dirs sem
`.codex-plugin/plugin.json`/`plugin.json` na enumeração. Não incluído no plano.

---

# Parte 4 — Inventário dos 29 conflitos e resolução

Classes: **mec** = mecânico (linhas adjacentes, manter ambos); **cost** = costura sem escolha de
produto; **dec** = decisão (fechada acima).

| # | ficheiro | hunks | classe | upstream | resolução |
|---|---|---|---|---|---|
| 1 | `Cargo.toml` | 2 | mec | #42102 | manter `plans` **e** `otel-trace-websocket` em members e deps |
| 2 | `Cargo.lock` | 2 | mec | — | regenerar (`cargo update -w` não; deixar o `cargo check` reescrever) |
| 3 | `app-server-protocol/schema/precomputed/*.json.zst` ×2 | — | mec | — | tomar upstream; regenerar depois com o gerador de schema do repo (`just`/teste de snapshot) |
| 4 | `core/src/agent/control/spawn.rs` | 1 | cost | #41744 | `resolve_usage_hints(&parent_config.multi_agent_v2, None, !parent_config.update_plan_enabled)` + `flat_map` do fork com `without_fork_hint_sections` |
| 5 | `core/src/context/world_state/collaboration_mode.rs` | 1 | cost | #41744 | manter `settings_instructions` + `tracing::warn!` do fork; `let instructions = catalog_instructions.cloned().or(settings_instructions);` e a seguir o bloco upstream que remove instruções de update_plan |
| 6 | `core/src/mcp_tool_call.rs` | 1 | cost | #42054 (+#42133/#42134) | bloco root-only do fork primeiro; depois `let metadata = match mcp_tool_metadata(prepared_call.tool_info(), prepared_call.plugin_id(), invocation.arguments.as_ref()) { … }` do upstream; sem duplicar `fn mcp_tool_metadata` |
| 7 | `core/src/session/input_queue.rs` | 1 | cost | #41912 | fechar a cadeia com `.map(str::to_string);` (upstream) e re-inserir `if !has_trigger_mail && let Some(wake) = pending_wakes.last() { start_options = wake.start_options.clone(); }` |
| 8 | `core/src/session/mod.rs` | 2 | mec | #41924 | `mod plan_reminder;` + `mod realtime_history;`; em `send_event_raw_with_persistence`: `derived_receipt` (fork) → `(before_event, after_event)` (upstream) → envio de `before_event` |
| 9 | `core/src/session/rollout_reconstruction.rs` | 6 (2 c/ `ignore-space-change`) | cost pesada | #42065, #42293, #41912 | **partir da árvore `e83101bcd1`**: struct = união (`retained_context`, `guardian_history` + `last_plan`, `approved_plan`, `history_recovery_reason`; manter `pub(crate)`); `let rollout_suffix = base_compaction.map_or(rollout_items, \|c\| c.suffix);` do upstream; nas 2 chamadas a `finalize_active_segment` passar `&mut base_compaction` **e** `&mut last_plan`; `truncation_policy` (parâmetro) no lugar de `turn_context.model_info().truncation_policy.into()`; `restore_plans(...)` depois do override legacy de `reference_context_item` e antes do replay de world-state; literal final lê `retained_context`/`guardian_history` **antes** de `into_annotated_items()`; `cargo fmt` |
| 10 | `core/src/session/session.rs` | 1 | mec | #41912 | manter `let session_source_is_non_root_agent = …;` (usado em `is_subagent`) |
| 11 | `core/src/state/session.rs` | 1 | mec | #41912 | manter os três `use` |
| 12 | `core/src/tools/handlers/unified_exec/exec_command.rs` | 1 | cost → upstream | #42113 | apagar `command_for_display` do fork (upstream removeu o único uso; ficaria unused) |
| 13 | `core/src/tools/registry.rs` | 1 | dec (fechada) | #41933 | `tool_log_payload(&invocation.tool_name, &invocation.payload, &invocation.source)` + `let mut tool_result_tags = …; sandbox_tags.append_metric_tags(&mut tool_result_tags);` |
| 14 | `core/src/unified_exec/process_manager.rs` | 1 | mec | #42113 | manter os três `use` |
| 15 | `core/src/thread_manager.rs` | 3 | mec | #42132 | 4 campos do fork + `git_root_discovery: Arc<GitRootDiscovery>` (`Arc::default()` nos 2 construtores); depois a limpeza do §1.9 |
| 16 | `thread-store/src/local/read_thread.rs` | 1 | cost | #42151 | `is_tombstoned` → `ThreadNotFound` primeiro; depois `sqlite_metadata` / `persisted_model_settings` / `if let Some(metadata) = sqlite_metadata` |
| 17 | `app-server/src/message_processor.rs` | 1 | cost | #42149 | manter `transient_job_recovery_task` + `job_processor`; `if let Some(startup_config) = plugin_startup_tasks {` (campo é `Option<PluginStartupConfig>`) |
| 18 | `app-server/src/request_processors.rs` | 1 | mec | #42033 | `mod fleet_processor;` + `mod feedback_thread_index;` |
| 19 | `app-server/src/request_processors/thread_lifecycle.rs` | 1 | cost | #41924 | apagar a chamada `apply_realtime_event_effects(...)` (fn e `realtime_effects` já não existem); manter `apply_bespoke_event_handling_with_classification(..., &transient_job_classification)` |
| 20 | `rollout/src/compression.rs` | 2 | dec (C′) | #42039 | §1.9 |
| 21 | `rollout/src/lib.rs` | 1 | dec (C′) | #42039 | tomar upstream (sem os `pub use`) |
| 22 | `rollout/src/compression_tests.rs` | 1 | dec (C′) | #42039 | §1.9 |
| 23 | `tui/src/app.rs` | 1 | mec | #41911 | `mod recovery;` + `mod reconnect;` |
| 24 | `tui/src/app/agents_overview.rs` | 2 | dec (A) | #42104 … | §2.6 |
| 25 | `tui/src/app_event.rs` | 1 | mec | #42104 | manter os 4 variants do fork + `AgentsOverviewThreadRefresh` |
| 26 | `tui/src/chatwidget/interaction.rs` | 1 | cost | #41893 | manter só `let recovery_popup_was_active = self.recovery_popup_active();`; apagar `flush_completed_command_activity()` (upstream retirou-a daqui; continua usada noutros sítios) |
| 27 | `tui/src/chatwidget/tests.rs` | 1 | mec | #41742, #42325 | `mod recovery;` + os dois `#[path]` mods |
| 28 | `tui/src/chatwidget/tests/status_and_layout.rs` | 3 | cost | #42202 | em cada hunk `chat.local_settings.tui.status_line = Some(vec![…]);` + `chat.set_plan_mode_reasoning_effort(Some(ReasoningEffortConfig::Medium));`; no fim não pode restar `chat.config.tui_status_line` |

## 4.1 Quebras de compilação previstas fora dos conflitos (código do fork que "mergeou limpo")

Aparecem quase todas só em `cfg(test)` — por isso o gate é `cargo test --no-run --workspace`.

| # | ficheiro | causa | correcção |
|---|---|---|---|
| 1 | `core/src/agent/control_tests.rs:1930` | `resolve_usage_hints` ganhou 3.º arg (#41744) | `+ false` (ou `!config.update_plan_enabled`) |
| 2 | `ext/goal/src/approved_plan.rs:106` | `GoalRuntime::invalidate_turn_lineage` apagada (#41912; `mark_root_turn_ambiguous` → `set_root_turn_id`) | apagar a chamada ou reimplementar só o `thread_extension_data().remove::<TurnStartOptions>()` |
| 3 | `core/src/agent/control/fork_metrics_tests.rs:86` | `AgentMessageEvent` ganhou `questions` (#42178) | `questions: None` |
| 4 | `core/src/session/plan_tests.rs:96,172` | `CompactedHistoryMetadata` ganhou `compaction_response_id`; `CompactedItem` ganhou `compaction_response_id`, `guardian_history`, `latest_token_usage_record`, `retained_context` | preencher com `None` / `..Default::default()` |
| 5 | `thread-store/src/local/search_index_extractor_tests.rs:148` | mesmo literal `CompactedItem` | idem |
| 6 | `hooks/src/events/post_tool_use_evidence_tests.rs:265` | `ConfiguredHandler` ganhou `builtin` (#42110) | `builtin: false` |
| 7 | `app-server/src/request_processors/thread_recovery_processor.rs:524` | `ListenerTaskContext` ganhou `thread_unload_delay` (#42320) | `source_config.thread_unload_delay`, como `thread_processor.rs:1072` |
| 8 | `core/src/context_inspection_items.rs:145,184,240`, `context_inspection_preview.rs:52` | `match` exaustivo sem `ResponseItem::ConfigurationUpdate { .. }` (#42328) | braço novo |
| 9 | `core/src/context_inspection_provenance.rs:176` | `match` exaustivo sem `RolloutItem::RetainedContext(_)` / `TokenUsageRecord(_)` | acrescentar ao braço `=> 0` |
| 10 | `rollout/src/compression_worker.rs:165,298,304` | `RolloutReferenceIndex::scan_until` / `reference_count` / `.history_base` alterados no upstream | desaparece com a decisão C′ (§1.9) |

Verificados e sem problema: `codex-mcp/src/binding.rs`, literais `Session { … }` em
`session/tests.rs` e `session.rs`, destruturação de `RolloutReconstruction` em `session/mod.rs:1583`,
referências residuais a `realtime_event_handling` (nenhuma), `agent-roles/`, e todos os módulos
exclusivos do fork (`claude_code/**`, `chatgpt_web/**`, `ownership/**`, `state/src/workflow/**`,
`agents_fleet*.rs`, `claude_accounts.rs`, `chatgpt_web_cmd.rs`) — não chamam nada cuja assinatura
mudou nem têm `match` exaustivos sem wildcard sobre `ResponseItem`/`RolloutItem`/`Feature`.

## 4.2 Novidades upstream com impacto no fork (fora dos conflitos)

- **#42284** cache de plugins (§3). **#42324** "Avoid executing PATH helpers before workspace trust"
  toca `core-plugins/startup_sync.rs`.
- **#41744** `update_plan` opt-in: já blindado em config; confirmar após o merge que
  `resolve_update_plan_enabled` do fork e a leitura upstream concordam com a secção explícita.
- **#42202** LocalSettings da TUI: refactor interno; nenhuma chave de `config.toml` muda
  (`disable_paste_burst` de #41976 não está no nosso config).
- **#42320** `thread_unload_delay` configurável no app-server — útil para o Desktop; default mantém.
- **#42328** `ResponseItem::ConfigurationUpdate` (reasoning durável) — os provedores locais
  (`claude_code/mod.rs`, `chatgpt_web/mod.rs`) não fazem `match` exaustivo; confirmar no `cargo check`.
- **#42330/#42326/#42309** sandbox Windows (ACLs dos binários, control socket) — coexistem com o
  `split_packaged_candidates` do fork em `shell_detect.rs` (não tocado nesta janela).
- **#42039** compressão (§1); **#42104/#41918** agents overview (§2); Guardian V2 (vários) — só
  OpenAI-backend, não atinge os provedores locais.

---

# Parte 5 — Plano de execução

Pré-condições: árvore de trabalho limpa excepto `agent_docs/` (untracked, deixar); nenhum build
Rust largo a correr (o guard de admissão de build do fork bloqueia concorrentes); `RUST_MIN_STACK`
e o override `RUSTY_V8_*` já no cargo config do utilizador.

### Fase 0 — preparação (5 min)
1. `git tag pre-sync-20260902 HEAD` (ponto de retorno; `git reset --hard pre-sync-20260902` desfaz tudo).
2. Confirmar `git merge-tree --write-tree HEAD origin/main` ainda dá `cca47f96cb` (se o upstream
   avançou, refazer o inventário só para os ficheiros novos: `git log --oneline f59905647a..origin/main`).
3. Backup `~/.codex/config.toml` já feito (`config.toml.bak-20260902`).

### Fase 1 — merge e resolução dos 29 (1–2 h)
1. `git merge --no-commit -X ignore-space-change origin/main` (dá a forma `e83101bcd1` a
   `rollout_reconstruction.rs`; os outros ficheiros têm os mesmos hunks).
2. Resolver na ordem: mecânicos (#1, 2, 3, 8, 10, 11, 14, 15, 18, 23, 25, 27) → costuras
   (#4–7, 12, 16, 17, 19, 26, 28) → #13 → #9 → compressão (#20–22 + edições do §1.9) →
   `/agents` (#24 + `agents_overview_threads.rs`).
3. `.zst` de schema: tomar upstream e regenerar (o teste de snapshot do app-server-protocol acusa
   se divergir).
4. `git diff --check`; **não** correr `just fmt` antes de rever (reescreve `.bazel`/`justfile`
   para LF — se correr, reverter com `git diff --name-only | grep -v '\.rs$' | xargs git checkout --`).

### Fase 2 — compilação (30–40 min)
1. `cargo check -p codex-rollout -p codex-core -p codex-app-server -p codex-tui -p codex-ext-goal -p codex-thread-store -p codex-hooks`.
2. `cargo check --workspace --all-targets` e depois `cargo test --no-run --workspace` (apanha os
   itens 1, 3–9 do §4.1 — são `cfg(test)`).
3. Corrigir o que o §4.1 prevê + o que aparecer de novo; catalogar qualquer classe nova na memória
   de sync.
4. Commit do merge com mensagem que liste as 4 decisões e os fixes pós-merge (um `fix(fork)`
   separado se ficar grande).

### Fase 3 — testes focados (30 min)
Ordem (todos com `RUST_MIN_STACK=8388608`):
- `just fork-invariants` (49 testes; ajustar `fork-invariants.toml` se listar o invariante de
  compressão removido).
- `cargo test -p codex-rollout` (compressão: journal/validação/recuperação + o teste upstream de
  fork chain).
- `cargo test -p codex-core -- session::plan_tests session::rollout_reconstruction_tests context_inspection agent::role` (reconstrução, planos, carve-out `model_provider`).
- `cargo test -p codex-core -- wait_agent mailbox claude_code` (E1–E5 do workflow).
- `cargo test -p codex-tui -- agents_fleet agents_overview status_and_layout recovery`.
- `cargo test -p codex-app-server -- skills_list plugins thread_recovery jobs` (inclui os testes
  novos do #42284 para upgrade/rollback out-of-process).
- `cargo test -p codex-thread-store`.
- Confrontar falhas com o catálogo de dívida conhecida antes de diagnosticar.
- `cargo test --workspace` completo fica **user-gated** (contrato do repo; ~44 falhas conhecidas).

### Fase 4 — config e roles (10 min)
- Confirmar `[tools.update_plan] enabled = true` continua a produzir a tool com o binário novo
  (`codex debug context` ou um turno curto).
- Reconfirmar `~/.codex/agents/claude-*.toml` mantém `model_provider = "claude_code"` efectivo
  (teste `role_tests` + spawn real na Fase 6).

### Fase 5 — build e deploy (≈25 min de build)
1. `cargo build --release --bin codex --bin codex-code-mode-host --bin codex-command-runner --bin codex-windows-sandbox-setup`.
2. Hot-swap no vendor do npm (`%APPDATA%\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\`):
   `Move-Item` de cada exe em uso para `<nome>-pre-sync0902-<hhmm>.exe`, `Copy-Item` dos novos
   (`codex.exe` + `codex-code-mode-host.exe` em `bin\`, os outros dois em `codex-resources\`).
3. `bin\codex.exe --version` (o fork já não reporta 0.0.0 desde `97361a83d3`).
4. **Reiniciar o Codex Desktop** — obrigatório para o app-server carregar o binário novo, e é o
   que fecha o incidente do §3 (mata os `node` órfãos; apagar depois a pasta
   `…/spine-workbench/<versão-antiga>` vazia).

### Fase 6 — validação real (15 min)
1. Retomar a thread `01a05582…` no Desktop: o plugin `spine-workbench` tem de inicializar
   (é o teste do #42284 em produção). Reinstalar o plugin pela CLI com o Desktop aberto e retomar
   outra vez: agora tem de recarregar sem reiniciar.
2. Um turno real com spawn de agente `claude_code` e `wait_agent` (roles + mailbox), e um
   `/agents` na TUI ligada ao app-server (frota) e sem thread primária (sessões recentes).
3. Logs: `logs_2.sqlite` / app-server log sem `required MCP servers failed to initialize`.
4. Push para `fork` (`git@github.com:bobaoapae/codex.git`).

### Fase 7 — memória e docs (5 min)
- Actualizar a memória de sync (sync 02/09: HEAD `f59905647a`, 138 commits, merge commit, as 4
  decisões, classes novas de quebra: `resolve_usage_hints` 3.º arg, `GoalRuntime::invalidate_turn_lineage`
  removida, `CompactedItem`/`CompactedHistoryMetadata` campos novos, `ListenerTaskContext.thread_unload_delay`,
  `ResponseItem::ConfigurationUpdate`).
- Memória do cache de plugins: marcar como resolvido pelo upstream #42284 a partir deste sync.
- Este documento: anotar o merge commit e o que divergiu do previsto.

### Critérios de paragem
- Qualquer conflito fora dos 29 ou hunk diferente do inventário → parar e reanalisar esse ficheiro
  antes de continuar (não resolver "à vista").
- Falha de teste que não esteja no catálogo de dívida nem seja explicada pelas decisões acima →
  diagnosticar antes do deploy.
- `just fork-invariants` vermelho → não deployar.

---

# Apêndice — referências

**Compressão:** `rollout/src/compression.rs:41,53,63` · `compression_capabilities.rs` ·
`compression_worker.rs:128,161,165,298-307` · `compression_{writer,validation,journal,cleanup}.rs` ·
`core/src/thread_manager.rs:72,393-430` · `features/src/lib.rs:1108-1121` · upstream `9d0eae74cd` ·
leitores: `rollout/src/seekable_reader.rs:46,63`, `thread-store/src/local/model_context.rs:135`,
`tui/src/resume_picker_transcript_preview.rs:152`, `cli/src/doctor.rs:2339`.

**agents_overview:** `tui/src/app/agents_overview.rs:41,45,117` (fork) · upstream
`agents_overview_threads.rs:37,104,111,126` · `agents_fleet.rs` · `agents_fleet_tests.rs:19,40,72` ·
upstream `f40e08478c`, `746798b2f7`, `9d57be71ba`, `a7913390f7`.

**Plugins:** `core-plugins/src/manager.rs:525-585,715-830,869-900` · `store.rs:169-195,744-775` ·
`loader.rs:822-834,1398-1420` · upstream `50fffd5ed3` (#42284) · patch testado com
`git apply --check --3way`.

**Merge:** árvores `cca47f96cb` (normal) e `e83101bcd1` (`-X ignore-space-change`); base `88f776588f`;
alvo `f59905647a`.

---

# Execução (02/09) — o que aconteceu

**Merge:** `89433ec271`, feito com `git merge --no-commit -X ignore-space-change origin/main` sobre
`dcfcb570b2` (não `f59905647a`: o upstream avançou 3 commits durante a análise —
`dc0dc4f15d`, `301a7c5e01`, `dcfcb570b2`, todos em `windows-sandbox-rs`/`voice-host`/`third_party/voice`).
Confirmado antes de começar que a lista de conflitos continuava a ser exactamente os mesmos 29
ficheiros do inventário. Ponto de retorno: tag `pre-sync-20260902`.

Nenhum conflito fora dos 29, e nenhum hunk diferente do inventário. As quatro decisões foram
aplicadas como escritas.

## Desvios ao previsto

O §4 acertou em todos os 29 conflitos. O §4.1 acertou em 9 das 10 quebras previstas (a #10
desapareceu com a decisão C′, como antecipado). Apareceram **oito** quebras que o inventário não
previa — todas da mesma família (um tipo partilhado ganhou um campo, ou um `match` exaustivo ganhou
uma variante), mas em ficheiros que o §4.1 não tinha varrido:

| ficheiro | causa |
|---|---|
| `rollout/src/lib.rs:87` | `pub use compression::spawn_rollout_compression_worker_with_capabilities` sobreviveu fora do hunk de conflito (a decisão C′ apaga a função) |
| `thread-store/src/local/search_index_extractor.rs:71` | `match` sobre `RolloutItem` sem `RetainedContext`/`TokenUsageRecord` |
| `thread-store/src/local/search_index_extractor_tests.rs:113` | `AgentMessageItem` ganhou `questions` |
| `core/src/agent/mailbox/mod.rs:210` | mesmo `match` sobre `RolloutItem` |
| `core/src/unified_exec/mod.rs:83` | `pub(crate) use …MAX_COMMAND_SUMMARY_BYTES` ficou órfão ao apagar `command_for_display` (§4 #12) |
| `app-server/src/request_processors/turn_processor.rs:1172` | o upstream trocou `let (thread_id, thread)` por `let (_, thread)` em `turn_steer_inner` (a `seal_realtime_transcript_before_user_input` do fork saiu com o #41924); a `history_recovery_required_error` do fork ainda precisa do `thread_id` |
| `tui/src/app/recovery.rs:147` | `AppServerSession::resume_thread` ganhou `&LocalSettings` como 1.º argumento (#42202) |
| `tui/src/app/agents_overview_threads.rs:178` | o ficheiro novo do upstream constrói `ThreadListParams` sem os campos do fork (`thread_classes`, `root_thread_id`, `terminal_outcomes`) — resolvidos com `None` (vista daemon-wide) |
| `tui/src/app/tests/buffered_replay.rs:21` | `ThreadItem::AgentMessage` ganhou `questions` |

**Lição para o próximo sync:** o §4.1 varreu os literais e `match` a partir dos tipos que os
*conflitos* mudaram. Faltou o passo simétrico — varrer os ficheiros **exclusivos do fork** por cada
tipo partilhado que o upstream mudou na janela (`RolloutItem`, `ResponseItem`, `AgentMessageItem`,
`ThreadItem`, `ThreadListParams`, assinaturas de `AppServerSession`). Sete das oito saíam desse
varrimento. As duas restantes (`lib.rs`, `unified_exec/mod.rs`) são re-exports órfãos: vale a pena,
depois de cada remoção decidida, correr `grep` pelo símbolo apagado em todo o `codex-rs`.

**Fora dos conflitos, também:** `codex-cli` continua a depender de `codex-backend-client` (entrada
do fork em `Cargo.lock`, mantida); `features/src/lib.rs` ficou com o upstream, logo
`local_thread_store_shared_compression` passa a flag removido/aceite; `fork-invariants.toml` perdeu
a entrada `rollout.include_shared_capability` e o README do app-server perdeu a secção do gate.
Os dois `.json.zst` de schema foram regenerados com
`python app-server-protocol/scripts/write_schema_fixtures.py` (e `--experimental`).

## Portão de testes

`just fork-invariants` passa 48/48 (49 menos o invariante de compressão removido pela decisão C′).
Focados: `codex-rollout` 128/128, `codex-core` 188/188 nos filtros do plano, `codex-app-server`
45/45, TUI 218/219, `codex-thread-store` 269/272.

`cargo test --workspace --no-fail-fast` deu 82 falhas em 14 alvos. Repetidas as duas suites de
integração isoladas (`--test-threads=4`), caem para 37 — metade do que a corrida completa acusa é
contenção, não código. Contra a baseline `pre-sync-20260902` (worktree separada, target próprio) o
merge **melhorou** muito: `codex-core tests/all.rs` passou de **151 falhas para 17**.

Nenhuma das 37 é regressão:

| falhas | veredicto |
|---|---|
| 13 das 17 do core, 10 das 11 antigas do app-server | falham igual na baseline pré-sync |
| `suite::v2::plugin_install::plugin_install_starts_mcp_oauth_through_configured_http_proxy` | passa isolada — contenção de porta/proxy |
| `suite::cyber_exec_policy::*` (4) | não é regressão: os testes correm `git version` e afirmam que o output da tool contém essa string. O guard do fork (`unified_exec.rs`, *"destructive Git override requires an explicit command approval"*) já rejeitava o comando antes do sync — mas a mensagem de erro do fork era ``exec_command failed for `git version`: …`` e continha a string, pelo que a asserção passava por acidente. O #42113 tirou o comando da mensagem e expôs a rejeição. Corrigir, se for caso disso, é no guard, não nos testes. |
| TUI `rolling_rate_limit_snapshot_preserves_prior_individual_limit` | separador decimal pt-BR ("8.000" vs "8,000"), balde de locale já catalogado |
| `codex-thread-store` (3) | `workflow_backfill_journal` sem linha `dirty`; reproduzem na baseline |

Nota de operação: `cargo test --workspace` com paralelismo total esgota recursos do `link.exe`
(erro 1201) e deixa um PDB corrompido (LNK1285 na corrida seguinte); usar `-j 4` e, se acontecer,
`cargo clean -p <crate>` antes de repetir.

## Deploy e validação

Build release: 32m31s, quatro binários. Hot-swap no vendor do npm às 21:22 (cópias antigas em
`*-pre-sync0902-2122.exe`). `bin\codex.exe --version` → `codex-cli 0.146.1`.

Validado com turnos reais no binário novo:
- `spawn_agent` com role `claude-opus` (`model_provider = "claude_code"`) + `wait_agent` → o
  subagente respondeu `PONG`. O carve-out de `model_provider` em roles continua efectivo.
- `update_plan` produz a tool: a checklist renderizou (`✓ check` / `• report`) e o turno respondeu
  `DONE`. A secção explícita `[tools.update_plan]` do `config.toml` neutraliza a passagem do
  upstream para opt-in.

`git push fork main` → `ba01741fd7..b27060f46a`.

Falta (exige o Desktop, acção do utilizador): reiniciar o Codex Desktop, retomar a thread
`01a05582…` para ver o `spine-workbench` inicializar, reinstalar o plugin pela CLI com o Desktop
aberto e retomar de novo (prova do #42284 sem reiniciar), e um `/agents` na TUI com e sem thread
primária. O log do app-server antigo
(`…/LocalCache/Local/Codex/Logs/2026/09/02/codex-desktop-*-141650-0.log`) tem 54 ocorrências de
`required MCP servers failed to initialize`; com o binário novo não devem voltar.

## `/agents` na TUI — verificado ao vivo (02/09, madrugada)

A decisão A foi exercida contra o binário deployado, não só contra testes. No Windows isto exige um
rodeio que vale a pena registar: `AppServerTarget::LocalDaemon` é **inatingível** — o probe do socket
de controlo (`maybe_probe_default_daemon_socket`, `tui/src/lib.rs:447-475`) é `#[cfg(unix)]` e o gémeo
`#[cfg(not(unix))]` devolve sempre `None`; `codex agents` sem `--remote` aborta com *"`codex agents`
requires `--remote` on this platform"*; e `app-server daemon start` / `remote-control start` recusam
com *"only supported on Unix platforms"*. O único alvo não-Embedded no Windows é
`AppServerTarget::Remote`, contra um `codex app-server --listen ws://…` arrancado à mão.

Montagem: `codex app-server --listen ws://127.0.0.1:41777` num porto dedicado (o app-server do Desktop
é stdio, não colide), e a TUI conduzida por um binding directo à `winpty.dll` via ctypes — o
`winpty.exe` não serve, porque exige uma consola real dimensionada que um chamador headless não tem.

| ramo | comando | renderizou | ausente |
|---|---|---|---|
| **sem** thread primária | `codex agents --remote ws://127.0.0.1:41777` | `Agent command center` + `0 need input   0 working   0 ready` | `Agent fleet` |
| **com** thread primária | `codex --remote ws://127.0.0.1:41777`, depois `/agents` | `Agent fleet` + `root 01a064d3-…  generation 0  open` | `Agent command center` |

Os dois ramos são mutuamente exclusivos na captura, que é exactamente o `early return` da §2.6. Uma
sessão normal já tem `primary_thread_id` antes de se escrever fosse o que fosse (`startup.rs:583-599`),
por isso o ramo "sem thread primária" só se alcança pelo entry point `codex agents`
(`SessionSelection::AgentsOverview`, que não anexa thread nenhuma).

Dois apontamentos do exercício:

- Num root sem frota registada o `agent/fleet/status` devolve *"fleet root agent is not registered"* e o
  dashboard mostra `Fleet status unavailable.` em vez de cair para a listagem — o desvio mantém-se.
- Numa primeira captura a listagem daemon-wide aparecia **depois** do dashboard da frota. Repetindo sem
  enviar `ctrl+c` no fim, não aparece: a transição é causada por essa tecla. O mecanismo não está
  estabelecido — com uma view do bottom-pane aberta, `ctrl+c` não é tecla de "back"
  (`BottomPane::handle_key_event` só trata `Esc` enquanto há view no `view_stack`; um `ctrl+c` literal
  cai no handler genérico da view, que a `AgentsOverviewView` não liga). Fica registado como facto
  observado, não como explicação.
- **Lacuna de cobertura:** nenhum teste chama `open_agents_overview` com `primary_thread_id = Some(…)`.
  Os quatro call sites de teste correm todos com `None` (`agents_overview_tests.rs:232,:273,:620` e
  `tests/session_lifecycle_requests.rs:2449`), e `agents_fleet_tests.rs` testa só o *bookkeeping* de
  `apply_agents_fleet_status`/`apply_agents_fleet_operation` sem passar pelo `open_*`. O `early return`
  que implementa a decisão A não tem, hoje, teste que o exerça ponta a ponta.

### Reprodução independente

Um segundo agente repetiu o exercício sem conhecimento da primeira corrida, contra um app-server
próprio em `ws://127.0.0.1:8931`, e obteve as mesmas duas strings: `Agent command center` por
`codex agents --remote`, e `Agent fleet` + `root 01a064d9-…  generation 0  open` por `codex --remote`
seguido de `/agents`. Duas corridas independentes, portos e roots diferentes, mesmo resultado.

Dois detalhes úteis que saíram daí: o listener websocket expõe `GET /readyz` e `/healthz` com 200
imediato (sinal de prontidão melhor do que fazer scan do banner em stderr), e o keybinding global
`tui.keymap.global.open_agents` vem **desligado** por omissão (`built_in_defaults()` dá
`default_bindings![]`), portanto o slash command é o único gatilho sem override de config.
