# Sync upstream 02/09 — duas decisões que precisam de plano

> Somente leitura: **o merge não foi iniciado**. Nada da árvore de trabalho foi tocado; toda a
> análise de conflito saiu de `git merge-tree --write-tree` (árvore `e8bebc2352`), que produz o
> resultado do merge sem aplicá-lo.
>
> Este documento existe para virar entrada do plan mode. Ele descreve **funcionamento**, não
> preferências: as opções aparecem com a consequência de cada uma, e as perguntas em aberto
> estão marcadas como tal.

## Situação do sync

| | |
|---|---|
| base upstream do fork | `88f776588f` (30/08) |
| upstream agora | `1bc8fb16ae` (02/09) |
| commits novos | 128 |
| arquivos em conflito | 29 (~40 hunks) |

**Não conflita e passa intacto:** todo `core/src/ownership/` e `state/src/workflow/lease*` — o
upstream não encostou em nenhum dos dois nesses 128 commits. O pin de versão `0.146.1` também
sobrevive: o upstream não mexeu naquela linha do `[workspace.package]`.

Dos 29 conflitos, 11 são mecânicos (linhas adjacentes em listas de `mod`/`use`/membros do
workspace, `Cargo.lock`, dois `.zst` regeneráveis), 14 são costura à mão sem escolha de produto
(o mais pesado é `session/rollout_reconstruction.rs`, 6 hunks, onde o upstream reestruturou o
replay reverso e os nossos campos `last_plan` / `approved_plan` / `history_recovery_reason`
precisam ser re-encaixados na forma nova), 2 já foram decididos, e **2 precisam de plano** —
são os deste documento.

### Já decidido

- **`tools/registry.rs`** — manter o `tool_name` no `tool_log_payload`: readicionar o parâmetro
  na assinatura nova do upstream e ficar também com as tags de métrica de sandbox que eles
  acrescentaram.
- **Plan mode / `update_plan`** — aplicar os dois lados. Detalhe que mudou a decisão: o nosso
  `resolve_update_plan_enabled` usa `is_none_or` (seção ausente = **ligado**) e o upstream passou
  a usar `is_some_and` (ausente = **desligado**), então o merge desligaria a tool sem aviso. Já
  foi escrito `[tools.update_plan] enabled = true` + `survives_compaction = true` explicitamente
  no `~/.codex/config.toml` (backup: `config.toml.bak-20260902`) para blindar contra a inversão.

---

# Parte 1 — Compressão de rollouts

## 1.1 O problema que a compressão cria

O worker de compressão varre `~/.codex/sessions/` e `~/.codex/archived_sessions/` e converte
rollouts frios (>7 dias) de `.jsonl` para `.jsonl.zst`.

O caso delicado são as **linhagens compartilhadas**. Quando a thread B é forkada da A, B **não**
copia o histórico de A: o `SessionMeta` de B guarda um `history_base: HistoryPosition` com
`thread_id` da origem, `end_ordinal_exclusive` e **`end_byte_offset`** — um deslocamento em bytes
dentro do arquivo de A. Ler o histórico de B exige abrir o arquivo de A e posicionar naquele
offset.

Comprimir A invalida esse offset para qualquer leitor que abra o arquivo diretamente. Só funciona
se o leitor passar pelo `open_rollout_line_reader`, que trata `.jsonl` e `.jsonl.zst` de forma
transparente.

Isso **não é hipotético**: no mesmo commit em que passou a comprimir linhagens compartilhadas, o
upstream teve que corrigir um leitor que abria o arquivo direto —
*"Read rollout files through the compressed-rollout reader when `codex exec resume` determines the
latest turn's working directory"* (`exec/src/lib.rs`, +30/-­). Ou seja: a classe de bug é real e
já mordeu o próprio upstream.

## 1.2 Como o fork resolve hoje

Três peças:

**`RolloutCompressionMode`** (`rollout/src/compression.rs:41`)

```rust
pub enum RolloutCompressionMode {
    Standalone,     // linhagens compartilhadas ficam em JSONL puro
    IncludeShared,  // exige que TODO leitor deste codex_home saiba ler .zst compartilhado
}
```

**`RolloutCompressionCapabilities`** (`rollout/src/compression_capabilities.rs`)

```rust
pub struct RolloutCompressionCapabilities {
    pub cargo: bool,
    pub bazel: bool,
    pub tui: bool,
    pub app_server: bool,
    pub desktop: Option<bool>,   // Option porque o leitor Desktop instalado
}                                // nao pode ser inferido desta biblioteca
```

`all_readers_support_shared()` exige os quatro `true` **e** `desktop == Some(true)`. O `Default`
deixa tudo `false`/`None`, então o padrão é recusar.

**O gate no worker** (`rollout/src/compression_worker.rs:128`)

```rust
let capability_blocked =
    mode == RolloutCompressionMode::IncludeShared && !capabilities.all_readers_support_shared();
...
if capability_blocked {
    metrics::run("skipped_capability_gate");
    debug!("{}", capabilities.shared_compression_diagnostic());
    return Ok(CompressionStats::default());   // não comprime NADA nesta rodada
}
```

Note o efeito: quando o gate bloqueia, o worker **aborta a rodada inteira**, não só as linhagens
compartilhadas. É fail-closed no sentido forte.

E quando roda em `Standalone`, dois filtros protegem as linhagens
(`compression_worker.rs:298-307`):

```rust
if mode == Standalone && reference_index.reference_count(rollout_id) > 0 { skipped_referenced }
if mode == Standalone && meta.meta.history_base.is_some()              { skipped_fork_pointer }
```

O `RolloutReferenceIndex` é o que sabe quais rollouts são apontados por algum fork.

O invariante `fork_invariant_include_shared_fails_closed_without_reader_capabilities`
(`rollout/src/compression_tests.rs`) tranca esse comportamento.

## 1.3 O que o upstream fez

Commit `9d0eae74cd` — *"Include shared histories in rollout compression (#42039)"*, 01/09:

> - Make `local_thread_store_compression` compress cold rollout files across shared and forked
>   histories **without a separate compression mode**.
> - **Retire** `local_thread_store_shared_compression` while continuing to accept it in strict
>   configuration without changing compression behavior.

O diff (`-137/+305` no total, mas na direção da remoção):

- apagou o enum `RolloutCompressionMode` inteiro;
- `spawn_rollout_compression_worker(codex_home)` — sem parâmetro de modo;
- **removeu o `RolloutReferenceIndex` do worker** (`rollout_reference_index.rs` perdeu 133 das
  suas linhas);
- removeu a leitura do `SessionMeta` que checava `history_base`;
- consequentemente removeu os dois `skipped_*`.

Restou: comprime todo arquivo frio, ponto. A segurança passou a depender inteiramente de
"todo leitor usa o reader que trata `.zst`".

Do lado do fork, o upstream **não** tem: `RolloutCompressionCapabilities`, o gate, a extração do
worker para `compression_worker.rs` (eles mantêm `mod worker { ... }` inline dentro de
`compression.rs`), nem os módulos `compression_cleanup` / `compression_journal` /
`compression_validation` / `compression_writer` que o fork separou.

## 1.4 O estado real na máquina

**A compressão está desligada.** Os dois flags são `Stage::UnderDevelopment, default_enabled: false`
(`features/src/lib.rs:1108-1121`) e o `~/.codex/config.toml` não habilita nenhum. Nenhum rollout
foi comprimido até hoje.

Isso muda a urgência: qualquer decisão aqui afeta **código dormente**. O risco só se materializa
se alguém ligar `local_thread_store_compression`.

## 1.5 A premissa do gate, verificada

O `desktop: Option<bool>` existe porque "o leitor Desktop instalado não pode ser inferido desta
biblioteca". Fui verificar no bundle instalado
(`OpenAI.Codex_26.831.2377.0/app/resources/app.asar`, 284,7 MB):

| busca | ocorrências | o que são |
|---|---|---|
| `.zst` | 20 | **todas** da lib `tar` (`.tar.zst`, `.tzst`) — nenhuma sobre rollout |
| `.jsonl` | 145 | import do Claude Code, `realtime-*.jsonl`, `transcription-history.jsonl`, staging de import |
| `rollout` | 320 | majoritariamente strings de erro **vindas do app-server** |
| `readFile` perto de `rollout` | **0** | — |

Os únicos pontos em que o Desktop toca um caminho de rollout:

1. **Import de transcript** — constrói `rollout-${id}.jsonl` num diretório de staging
   (`fork-transcript-imports`) e **escreve** um rollout novo (payload com `agent_nickname`,
   `agent_path`, `base_instructions`, `cli_version: "0.0.0"`).
2. **Handoff** — copia um `rollout.jsonl` para `~/.codex/handoffs/<id>/` e valida
   `basename(e) !== 'rollout.jsonl'` → erro.

Nenhum dos dois fica sob `sessions/` ou `archived_sessions/`, que é onde o worker varre. E as
strings tipo `no rollout found for thread id` / `failed to resolve rollout path` são mensagens do
**app-server** mapeadas para códigos de UI — ou seja, a leitura normal de thread passa pelo nosso
binário.

**Conclusão parcial:** para esta versão do Desktop, a premissa do `desktop: Option<bool>` parece
mais fraca do que quando o gate foi escrito. O Desktop não tem leitor zstd, mas também não parece
ler rollout de `sessions/` diretamente.

**Limite honesto desta verificação:** eu provei a *ausência* de um leitor zstd e a ausência de
`readFile` perto de `rollout` num bundle minificado. Não provei que nenhum caminho do Desktop abre
um rollout de `sessions/` — código minificado e indireção por variáveis podem esconder isso. E a
verificação vale para 26.831; uma versão futura pode mudar.

## 1.6 O risco que sobra, e onde ele está

Se o Desktop não é o problema, o problema é **do nosso lado**: leitores do fork que o upstream não
tem e que podem abrir rollout direto. Candidatos a auditar (nenhum verificado ainda):

- `chatgpt_web/`
- a ponte `claude_code/` e o `sessions.rs` dela
- o subsistema de workflow/fleet (`state/src/workflow/`, `agent/control/fleet.rs`)
- `memories/`
- os scripts em `scripts/`

Esse levantamento é, provavelmente, a primeira tarefa do plano.

## 1.7 Opções

| | o que fica | o que se perde | custo |
|---|---|---|---|
| **A. Manter o gate, portar o worker deles** | capabilities + extração em `compression_worker.rs` + invariante | as simplificações deles | costurar as mudanças do corpo do worker deles no nosso arquivo; conflito recorrente nesse arquivo a cada sync |
| **B. Pegar o deles inteiro** | convergência total com upstream | `RolloutCompressionMode`, `RolloutCompressionCapabilities`, o invariante, a extração em módulos | baixo agora; o risco migra para "se ligar compressão, confie que todo leitor usa o reader certo" |
| **C. Estrutura deles + gate por cima** | fail-closed preservado, estrutura convergida | a extração em `compression_worker.rs` e módulos irmãos | reimplementar capabilities/mode sobre o `mod worker` inline |

Uma quarta possibilidade, que só faz sentido depois da auditoria de 1.6: **B + auditoria** — pegar
o deles e, se a auditoria mostrar que todo leitor do fork usa `open_rollout_line_reader`, aceitar
que o gate virou redundante.

## 1.8 Perguntas em aberto para o plano

1. Algum leitor exclusivo do fork abre rollout sem passar por `open_rollout_line_reader`?
2. O gate deveria continuar abortando a **rodada inteira**, ou só pular as linhagens
   compartilhadas? (Hoje ele aborta tudo.)
3. Se ficarmos com o gate: quem preencheria `desktop: Some(true)`? Hoje nada preenche — o campo é
   inalcançável na prática, o que torna `IncludeShared` inatingível por construção.
4. Faz sentido manter um modo cuja única configuração possível é "desligado"? Se não, a escolha
   real é entre B e C, não entre A e B.

---

# Parte 2 — `tui/src/app/agents_overview.rs`

## 2.1 Correção de escala

Na primeira leitura eu descrevi isso como "duas features concorrentes reescrevendo o mesmo
arquivo". Está errado, e a diferença muda a decisão.

O fork mudou **28 linhas** em `agents_overview.rs`. O dashboard de frota de verdade vive em
**`agents_fleet.rs`, 521 linhas novas, que não conflita com nada**.

## 2.2 O que as nossas 28 linhas fazem

`git diff 88f776588f..main -- codex-rs/tui/src/app/agents_overview.rs` = 3 inserções:

1. **campo no estado**: `pub(super) fleet: super::agents_fleet::AgentsFleetState`
2. **desvio no topo de `open_agents_overview`**: se há `self.primary_thread_id`, chama
   `open_agents_fleet_overview(app_server, root_thread_id)` e retorna. Se não há, mostra uma
   selection view *"No fleet root selected — Open a session before opening the shared agent-fleet
   dashboard."*
3. **early-return em `refresh_agents_overview_threads`**: `if self.agents_overview.fleet.root_thread_id.is_some() { return; }`

Ou seja: é uma **camada de desvio**. O corpo original do upstream continua no arquivo, apenas
deixa de ser alcançado quando existe thread primária.

## 2.3 O que o `agents_fleet.rs` é

Cabeçalho: *"Durable fleet status and lifecycle requests for the `/agents` dashboard. The
app-server owns state and generation compare-and-swap."*

Estado (`AgentsFleetState`): `root_thread_id`, `generation` (CAS), `sealed`, `operation_id`,
`members: Vec<FleetMember>`, `status`, `notice`, dois request-ids em voo, e uma
`Arc<Mutex<AgentsFleetViewState>>`.

Operações: `open_agents_fleet_overview`, `show_agents_fleet_view`, `refresh_agents_fleet_status`,
`apply_agents_fleet_status`, `open_agents_fleet_actions`, `open_agents_fleet_close_confirmation`,
`request_agents_fleet_suspend` / `_resume` / `_close`, `apply_agents_fleet_operation`.

Resumo: `/agents` deixou de ser "o que está carregado nesta TUI" e virou "a frota durável que o
app-server conhece", com suspend/resume/close transacionados por geração.

## 2.4 O que o upstream fez no mesmo arquivo

Quatro commits:

| commit | o quê | tamanho |
|---|---|---|
| `f40e08478c` | *Show recent sessions in the agent command center* (#42104) | +177/-175 |
| `746798b2f7` | *Restore agent navigation after TUI reconnects* (#41918) | +19/-3 |
| `9d57be71ba` | *Separate TUI preferences from server configuration* (#42202) | +10/-4 |
| `a7913390f7` | *Preserve TUI drafts after app-server disconnects* (#41911) | +1/-1 |

O #42104 é uma reescrita das entranhas. O estado mudou de request-id único para:

```rust
pub(super) threads: HashMap<ThreadId, Option<Thread>>,   // None = resume local sem metadata
pub(super) initialized: bool,
pub(super) refresh_thread_ids: HashSet<ThreadId>,
pub(super) refresh_task: Option<tokio::task::AbortHandle>,
pub(super) refresh_notifications: HashMap<ThreadId, Vec<ServerNotification>>,
```

mais um `impl Drop for AgentsOverviewState` que aborta a `refresh_task`. Sumiram os
`ThreadLoadedList*` / `ThreadTurnsList*` / `ThreadRead*` do topo do arquivo. O cabeçalho mudou de
*"overview of loaded root sessions"* para *"overview of recent and locally retained sessions"*.

## 2.5 Os dois pontos de atrito

1. **Cabeçalho do módulo** — trivial, o comentário `//!` de cada lado.
2. **`refresh_agents_overview_threads`** — o nosso early-return está no topo de uma função que o
   upstream reescreveu inteira. Além disso, a nossa segunda inserção lê
   `self.agents_overview.request_id`, campo que o upstream **apagou**. Combinar exige reexprimir o
   early-return contra o estado novo (provavelmente checando `fleet.root_thread_id` antes de tocar
   em `refresh_thread_ids` / `refresh_task`).

O desvio em `open_agents_overview` também precisa ser reposicionado, mas é mecânico: ele roda
antes de qualquer coisa e retorna cedo.

## 2.6 Opções

| | resultado | custo |
|---|---|---|
| **A. Combinar** | `/agents` continua indo para a frota quando há thread primária; o caminho de fallback ganha sessões recentes, correção de reconexão e a separação de preferências do TUI | reexprimir 1 early-return contra o estado novo; reposicionar o desvio |
| **B. Só o nosso** | comportamento atual preservado | perde os 4 commits deles nesse arquivo; o mesmo conflito volta no próximo sync, e maior |
| **C. Só o deles** | convergência | `/agents` volta ao upstream; `agents_fleet.rs` fica órfão (521 linhas sem chamador) |

## 2.7 Perguntas em aberto para o plano

1. Quando **não** há `primary_thread_id`, hoje mostramos *"No fleet root selected"*. Com o
   caminho do upstream restaurado como fallback, faria mais sentido cair na visão de sessões
   recentes deles em vez da mensagem?
2. O `Drop` que aborta a `refresh_task` do upstream precisa rodar também quando estamos na rota da
   frota? (Se o desvio acontece antes de a task ser criada, provavelmente não — confirmar.)
3. `agents_fleet.rs` duplica algo que o #42104 passou a oferecer (listar sessões recentes)? Se
   sim, vale unificar a fonte de dados em vez de manter duas.

---

# Apêndice — referências de arquivo

**Compressão**
- `codex-rs/rollout/src/compression.rs:41` — `RolloutCompressionMode`
- `codex-rs/rollout/src/compression.rs:53,63` — as duas funções de spawn
- `codex-rs/rollout/src/compression_capabilities.rs` — capabilities e diagnóstico
- `codex-rs/rollout/src/compression_worker.rs:128` — `capability_blocked`
- `codex-rs/rollout/src/compression_worker.rs:161` — abort da rodada
- `codex-rs/rollout/src/compression_worker.rs:298-307` — `skipped_referenced` / `skipped_fork_pointer`
- `codex-rs/core/src/thread_manager.rs:418,425` — call sites
- `codex-rs/features/src/lib.rs:1108-1121` — os dois flags, ambos default-off
- upstream `9d0eae74cd` — o commit que retirou o modo

**agents_overview**
- `codex-rs/tui/src/app/agents_overview.rs:41,45,117` — as 3 inserções do fork
- `codex-rs/tui/src/app/agents_fleet.rs` — o dashboard (521 linhas, sem conflito)
- upstream `f40e08478c`, `746798b2f7`, `9d57be71ba`, `a7913390f7`

**Merge**
- árvore do merge-tree: `e8bebc2352e68f4d3d4df45192bab6f87452d8c9`
  (`git show <árvore>:<caminho>` mostra qualquer arquivo já com os marcadores de conflito)
