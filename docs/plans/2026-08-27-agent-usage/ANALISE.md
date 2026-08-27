# Uso de agentes — análise dos últimos 7 dias e proposta de config

> Somente leitura: nada em `~/.codex`, em `config.toml` ou no código foi alterado.
> Janela: 2026-08-20 00:00 → 2026-08-27 (UTC-3). Fonte: rollouts em
> `~/.codex/sessions/2026/08/**` (`history.jsonl` só recebe prompts do TUI — tem **1**
> linha na janela — por isso o Desktop foi lido direto dos rollouts).
> Scripts: `scratchpad/prompt-analysis/{extract,analyze,turns}.py`.

Pergunta do dono: *«Revise config do codex para incentivar o uso natural disso [agentes
paralelos], com base nos meus últimos prompts, pra eu não ter que ficar pedindo sempre;
identifique se algo no próprio codex melhoraria esse uso natural».*

Resposta curta: **o dono não precisa pedir quando a thread está em `ultra`; precisa pedir
sempre que a thread está em qualquer outro esforço** — e isso não é o modelo "esquecendo",
é o harness dizendo a ele, por escrito, *"Do not spawn sub-agents unless the user …
explicitly ask"*. O modo multi-agente é derivado do esforço de raciocínio
(`codex-rs/core/src/session/multi_agents.rs:267-273`): só `ultra` liga o modo proativo;
`max`, `xhigh`, `high` e `medium` caem em `ExplicitRequestOnly`. Três sessões grandes da
semana (1.482 tool calls somadas, todas "Executar o plano X.md") rodaram single-agent por
esse motivo. O segundo problema é que, mesmo em proativo, o texto das tools empurra o
modelo para "continue locally" e "omit `agent_type` unless explicitly asked", e as
descrições dos roles do dono dizem *o que* cada role é, não *quando* escolhê-lo.

---

## 1. Os números

### 1.1 Método (e três armadilhas dos dados)

- 479 arquivos na janela + 2 sessões raiz iniciadas antes (17/08 e 19/08) que continuaram
  dentro dela. Um rollout **filho** com `fork_turns: "all"` (97 spawns na semana)
  **copia a transcrição inteira do pai**, inclusive `session_meta` e os prompts do
  usuário: 202 arquivos-fork replicavam os mesmos prompts. Deduplicação por `client_id`
  da `user_message` e uso do **primeiro** `session_meta` do arquivo.
- 35 sessões raiz `originator = codex_exec` são os smoke tests desta semana
  (`chatgpt-web/*`, `claude-sonnet-5`, `gpt-5.6-sol` via `codex exec`) — excluídas.
- Mensagens `<codex_delegation>` (33 turnos) são cards do Desktop vindos de outras
  threads, não prompts do dono — contadas à parte.

### 1.2 Volume

| | valor |
|---|---|
| Sessões raiz interativas ativas na janela | **13** (12 Desktop, 1 TUI sem prompt) |
| Turnos iniciados pelo dono | **116** (+33 turnos `<codex_delegation>`) |
| Turnos/dia | 20: 14 · 21: 17 · 22: **36** · 23: 21 · 24: 4 · 25: 4 · 26: 7 · 27: 13 |
| Turnos curtos (< 25 chars: "sim", "aprovado", "pronto") | 25 |
| Projetos (por sessão) | `surftank-reborn` 10 · `ane-awesome-utils` 2 · `AutoSerializer` 1 |
| Modelo/esforço da raiz | `gpt-5.6-sol`: ultra 9 sessões · medium 2 · high 1 · max 1 |

### 1.3 Quanto o dono pediu agentes explicitamente

| medida | valor |
|---|---|
| Turnos com pedido explícito ("agentes", "paralelo", "delegando", "spawn") | **13 / 116 (11 %)** |
| Turnos que citam um role/modelo pelo nome (opus, sonnet, luna, chatgpt pro, claude, sol) | 20 / 116 (17 %) |
| **Primeiro turno da sessão** com pedido explícito | **9 / 13 sessões** |
| Sessões `ultra` cujo primeiro prompt já pedia agentes | 8 / 9 |
| Sessões `medium`/`high` (3) — pedido explícito | 0 → **0 spawns** em 1.482 tool calls |

Ou seja: o dono aprendeu que precisa pedir *na abertura* — 7 das 9 aberturas usam o mesmo
molde, `executar por completo plano (X.md) lembre-se de usar opus/sonnet/lunna todos no
máximo conforme complexidade da tarefa, e em paralelo onde possível`. Nas 3 sessões em que
ele **não** colou o molde (por acaso as de esforço médio/alto), nenhum agente nasceu.

### 1.4 O que os agentes fizeram

| medida | valor |
|---|---|
| `spawn_agent` chamados pela raiz na janela | **436** (352 em turnos do dono, 47 em turnos de delegação, 37 na sessão `01a0415d-3598-…` de 27/08, aberta pelo Desktop sem `user_message` gravada) |
| Turnos do dono que spawnaram | 42 / 116 — 27 **sem** pedido, 15 com pedido |
| Turnos em modo **proativo** que spawnaram | 39 / 78 |
| Turnos em modo **explícito** que spawnaram | 3 / 38 (só quando o prompt era um brief pedindo) |
| Roles (352 spawns em turnos do dono) | claude-sonnet 68 · claude-opus 65 · explorer 62 · executor_luna 42 · **sem role/`default` 45 (13 %)** · luna 28 · tester 17 · worker 16 · executor_sol 6 · doc-writer 3 · **chatgpt-pro 3** |
| `fork_turns` | none 168 · **all 97** · 3: 61 · 4: 22 · 2: 4 (Claude sempre `none`/pequeno — bom) |
| Spawns de subagentes (profundidade 2) | 0 (`agents.max_depth` default 1 — `config/mod.rs:4044`) |
| `wait_agent`/`wait` | **1.909 chamadas; 1.479 de 1.823 medidas (81 %) terminaram por timeout**; timeout pedido pelo modelo: 30 s em 54 %, 60 s em 26 % |
| `interrupt_agent` | 85 |
| Providers dos filhos | claude_code 154 threads · openai 100 · chatgpt_web 3 |

### 1.5 Classes de tarefa (turnos substantivos, multi-rótulo)

| classe | turnos | agentes pedidos? | o que o dono espera |
|---|---|---|---|
| **Executar plano `.md` inteiro** (implementação multi-arquivo + testes + docs) | 8 aberturas + ~10 continuações | sim, no molde | time explorer→executor→tester, Claude para o difícil |
| Validação em dispositivo (mac/iOS/Android/Godot, prints, login→hall) | ~20 | não | fluxo sequencial, mas com investigação/relatório delegáveis |
| Design/Figma/imagem de referência (ChatGPT) | ~12 | "use o mcp chatgpt pro" (8×) | geração de imagem e referência visual — via MCP `chatgpt`, **não** via role |
| Review/segunda opinião de plano ("confrontar modelos diferentes") | 2 | sim | opus + chatgpt-pro + sol em paralelo |
| Ops (commit/push/merge, docker, CI) | ~10 | não | local, correto não delegar |
| Perguntas curtas / aprovações | ~30 | — | — |

### 1.6 Prompts representativos (verbatim)

1. `executar por completo plano (docs/PLANO-SISTEMA-E2E-UI-SCRIPTAVEL-20260823.md) lembre-se de usar opus/sonnet/lunna todos no máximo conforme complexidade da tarefa, e em paralelo onde possível.` — 23/08 14:44, ultra, 9 spawns. **Repetido quase idêntico em 24/08 02:34 (43 spawns), 25/08 21:43 (13), 26/08 16:15 (9).**
2. `Executar por completo o plano (docs/PLANO-BUILD-E-TESTE-DISPOSITIVOS-20260822.md) delegando para gantes opus/sonnet/luna todos no máximo conforme dificuldade da tarefa e o que puder ser feito em paralelo fazer.` — 22/08 18:24, ultra, 58 spawns na sessão.
3. `Execute apenas a parte dos 3 repo individuais, sem mexer em SurfTank Reborn. docs/PLANO-UNIFICACAO-STACK-TRANSMISSAO-20260823.md  use lunna/sonnet/opus conforme avaliação da complexidade da tarefa, e o que der para fazer em paralelo sem conflito faça.` — 23/08 16:23, 20 spawns.
4. `Executar integralmente o plano client/docs/PLANO-CORRECAO-E2E-20260821.md` — 22/08 02:37, **medium, 142 tool calls, 0 spawns**; a sessão seguiu 24 turnos / 795 tool calls sem um agente.
5. `Executar o plano fielmente ( docs/PLANO-REMOCAO-VARIAVEIS-AMBIENTE-20260822.md)` — 22/08 16:50, **medium, 151 tool calls, 0 spawns**.
6. `Executar o plano (C:\Users\Joao\.claude-wrapper\views\2\plans\graceful-bubbling-sloth.md)` — 21/08 23:34, **high, 288 tool calls, 0 spawns**.
7. `use agente segundario luna maximo para comparar imagen figma com a reconstruida em godot, pra vc ter avaliação LLM, para ver se está fazendo correto.` — 20/08 23:46 (o dono ensinando um padrão de validação visual delegada).
8. `use agentes luna para analisar cada uma das screenshot por problemas visuais, componentes ocultando outros e outros problemas visuais, colete disso e faça um relatorio de problemas…` — 22/08 02:04, **high/explícito**: só spawnou (3 luna) porque o pedido veio.
9. `Revise o plano por completo o plano …curious-churning-plum.md, use agentes paralelos de modelos diferentes para confrontar, você sol fable 5 e chatgpt pro via o mcp chatgpt. Pode enviar arquivos necessarios como zip pro chatgpt poder analisar. Além de revisar plano, é para encontrar melhorias, e outros bugs escondidos…` — 27/08 01:18, ultra, 5 spawns (opus, explorer, luna×2, default).
10. `vc iniciou outro chat, alguma confusão, eu abri em outra aba e ta pensando ainda, vc apenas desistiu de esperar. volte a esperar` → `acredito que chatgpt pro travou, vamos ignorar e seguir com o que tivemos local mesmo dos nossos agentes.` — 27/08 02:03–02:14: a raiz abandonou a espera pelo `chatgpt-pro` (Pro pensa 10–20 min) e abriu outra conversa.
11. `use o mcp chatgpt pro para fazer um plano completo com base nesse problema.` — 25/08 10:57. Também: `para de gerar imagem aqui, imagem é pra gerar via mcp chatgpt pro` / `não, tem literalmente um MCP chamado chatgpt, é pra usar ele.` — 22/08 10:44–10:45 (sessão medium).
12. `proximo passo importatnte é deixar o macos funcionando, ai vc segue fazendo ios android restante sem precisar me pedir nada` — 23/08 01:18, **ultra/proativo, 158 tool calls, 0 spawns** (mac, iOS e Android são três frentes independentes).
13. `sim pode fazer tudo, pode parar de me perguntar, vc aproveita e salva print de cada etapa vou usar isso pra re-validar as telas também.` — 22/08 01:12, **high/explícito, 223 tool calls, 0 spawns**.
14. `fiz varias mudanças de melhoria server, teste, forma de config e testes. mailtrap sql valkey ta todo configurado. Use isso pra testar completo cadastro login confirmaçao responsavel etc, use serviço de teste de emails gratuito via chrome mcp pra validar recebimento.` — 22/08 00:42, high, 71 tool calls, 0 spawns (um `tester` caberia inteiro aqui).
15. `Apenas valida todo o processo teste windows,android,mac,ios. Do login ao hall, compilar,instalar, iniciar, prints.` — 23/08 13:32, ultra, 4 spawns (opus, sonnet, explorer, executor_luna) — o padrão que o dono quer ver sem pedir.

---

## 2. O gap — onde o modelo devia ter delegado sozinho

### 2.1 Causa A (dominante): o modo multi-agente segue o esforço, não a config

`codex-rs/core/src/session/multi_agents.rs:267-273` (dentro de `effective_multi_agent_mode`, `:245-286`):

```rust
None => match turn_context.effective_reasoning_effort() {
    Some(ReasoningEffort::Ultra) => MultiAgentMode::Proactive,
    _ => … MultiAgentMode::ExplicitRequestOnly,
},
```

O texto injetado como developer message (`context/multi_agent_mode_instructions.rs:7-8`):

- proativo: *"Proactive multi-agent delegation is active … Use sub-agents when parallel work would materially improve speed or quality."*
- explícito: *"Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work."*

Evidência nas sessões (ids completos):

| sessão | esforço | modo visto no rollout | tool calls | spawns | abertura |
|---|---|---|---|---|---|
| `01a026ad-2367-7692-a0b9-b4e38b1e4f5f` | high | explícito | 312 | **0** | "Executar o plano (…graceful-bubbling-sloth.md)" |
| `01a02752-5244-71b2-9dd5-929535485cb2` | medium | explícito | 795 (24 turnos) | **0** | "Executar integralmente o plano client/docs/PLANO-CORRECAO-E2E-20260821.md" |
| `01a02a61-b4c1-7842-807b-525dedf3a927` | medium | explícito | 375 | **0** | "Executar o plano fielmente (docs/PLANO-REMOCAO-VARIAVEIS-AMBIENTE-20260822.md)" |
| `01a01174-084e-73f2-a92c-9c0a9e965cd6` | ultra → **high** em 21/08 22:51 | proativo → explícito | 2.318 | 45, **0 depois da troca** (4 turnos, 362 tool calls) até o dono pedir "use agentes luna" |
| `01a01bcb-9111-7e42-a20b-75054dbc61ee` | **max** | explícito | 2.960 | 68 (briefs do dono pediam; 41 sem role) |

Dois agravantes:

- `max` está **acima** de `ultra` na lista `enabled-reasoning-efforts` do Desktop, mas só
  `Ultra` casa no `match` — `max` vira "não spawne". Bug de expectativa, não de código.
- `plan_mode_reasoning_effort = "high"` no `config.toml` põe **todo turno em modo plano**
  em explícito. O dono planeja no Claude e executa no Codex, então o impacto foi pequeno
  nesta semana, mas é uma armadilha para "planeje isso com o time".

O harness já tem a chave que resolve isso sem código: `[features.multi_agent_v2]
multi_agent_mode_hint_text` (`features/src/feature_configs.rs:285`, lido em
`config/mod.rs:3005-3008`, precedência total em `multi_agents.rs:257-266`). Texto
custom = modo custom, independente do esforço; truncado a **400 tokens**
(`context/world_state/multi_agent_mode.rs:13`).

### 2.2 Causa B: mesmo em proativo, os textos das tools puxam para "faça local"

- `tools/handlers/multi_agents_spec.rs:730` — `agent_type`: *"Omit unless explicitly
  asked."* Resultado: 45/352 spawns sem role (`default` → luna xhigh, fork completo,
  sem instruções de especialista); na sessão `max`, 41 de 68.
- `multi_agents_spec.rs:846-855` — descrição v2 de `spawn_agent`: *"Only call this tool
  for a concrete, bounded subtask that can run independently alongside useful local work;
  otherwise continue locally."* A v1 (`:799-834`) tem uma seção rica "When to delegate /
  Parallel delegation patterns"; a v2 — a que o dono usa (`tool_namespace =
  "collab_agents"`) — não tem nada disso.
- `multi_agents_spec.rs:736` — `fork_turns` *"Defaults to `all`"*: 97 forks completos na
  semana, cada um copiando a transcrição inteira do pai (202 arquivos-fork). Em
  sessões de 1.500+ tool calls isso é o custo dominante de cada spawn e desestimula
  "mais um explorer".
- `session/multi_agents.rs:11-29` (hint da raiz): descreve o *mecanismo* (mailbox,
  `followup_task`), não uma *política* de quando delegar. As seções FORK (`:100-134`)
  cobrem reporting, paciência e disciplina de entrega — a peça "delegação por padrão"
  não existe.

Turnos proativos grandes sem spawn (tudo `gpt-5.6-sol ultra`):

| turno | tool calls | o que caberia delegar |
|---|---|---|
| 23/08 01:18 `01a02ab7` "deixar o macos funcionando, ai vc segue fazendo ios android" | 158 | 3 frentes de dispositivo → 3 agentes (mac/iOS/Android) com o dono como gate de permissão |
| 24/08 16:12 `01a0319e` "mac ta acordado, android ta no mac como emulador" | 129 | idem |
| 25/08 19:37 `01a0319e` "aab é principal… export android e ios nas últimas versões" | 103 | explorer para levantar versões/plugins + executor por plataforma |
| 20/08 23:10 `01a01174` "vc baixou imagem de forma correta ou erro na implementação?" | 92 | explorer (diagnóstico) em paralelo ao conserto |
| 22/08 12:52 `01a02752` "desativar a UI do sistema multi contas… voltar ao plano original" | 144 | executor + tester (sessão medium — Causa A) |

### 2.3 Causa C: as descrições dos roles não dizem "quando"

O orquestrador só vê `name: { description + notas travadas }` (`agent/role.rs:322-394`).
Comparar:

- built-in `explorer` (`role.rs:417-427`): *"Use `explorer` for specific codebase
  questions… You are encouraged to spawn up multiple explorers in parallel… Reuse existing
  explorers…"* — instruções de uso.
- `~/.codex/agents/explorer.toml`: *"Read-only exploration agent. Investigates assigned
  code…"* — descrição de identidade. Como roles do usuário **sombreiam** os built-in com o
  mesmo nome (`role.rs:305-315`), o texto rico foi perdido.
- `executor_luna`: *"Default implementation sub-agent for bounded production code changes
  from a parent-owned plan."*, `tester`: *"Independent testing sub-agent…"* — nenhum diz
  "spawne um por task do plano".
- `claude-opus`/`claude-sonnet`/`chatgpt-pro` dizem *para quê* (bom), mas não *quando em
  relação aos outros* (ex.: "toda task marcada difícil/cross-cutting → opus").

### 2.4 Causa D: `chatgpt-pro` nasceu esta semana e a raiz não sabe esperar por ele

3 spawns (27/08), todos na sessão `01a040ca-9749-7bb0-aa04-2723ccc3bd08`. A raiz esperou
com `wait_agent` curto, concluiu "travou", abriu **outra** conversa (o dono viu a primeira
ainda processando) e o dono mandou "volte a esperar". Pro leva 5–20 min; o hint de
paciência (`multi_agents.rs:115-124`) fala em "generous timeout" sem número, e a nota do
role (`role.rs:351-353`) não avisa a latência. 81 % dos waits da semana expiraram porque o
modelo pede 30 s (a config clampa só para cima a partir de 15 s —
`multi_agents_v2/wait.rs:50-61`).

Separar também dois pedidos que o dono faz com a mesma palavra: **"mcp chatgpt"** (server
Node `chatgpt-pro-mcp`, usado para *gerar imagens de referência* e *criar plano*) e o
**role `chatgpt-pro`** (agente para análise/review/pesquisa; imagens geradas do lado do
ChatGPT chegam só como nota — `docs/chatgpt_web_agents.md`, "What the parent sees"). O
AGENTS.md tem de dizer qual é qual, senão a raiz troca um pelo outro (aconteceu 3× na
sessão `01a02752`).

---

## 3. Proposta de configuração (pronta para colar)

Princípio: a *política* de delegação vai para onde só a **raiz** lê
(`multi_agent_mode_hint_text` + `root_agent_usage_hint_suffix`), o *protocolo* comum vai
para `developer_instructions`/AGENTS.md (todo agente lê — `agent/control/spawn.rs:822-843`
herda `developer_instructions` para filhos sem role), e o *"quando me escolher"* vai para
a `description` de cada role.

### 3.1 `config.toml` — `[features.multi_agent_v2]`

```toml
[features.multi_agent_v2]
hide_spawn_agent_metadata = false
tool_namespace = "collab_agents"
max_concurrent_threads_per_session = 10
# 81% dos waits da semana expiraram porque o modelo pede 30 s. Clampar para cima:
# um wait v2 acorda em qualquer mailbox update, então esperar mais nao custa nada.
min_wait_timeout_ms = 90000
default_wait_timeout_ms = 300000
max_wait_timeout_ms = 3600000

# Modo multi-agente independente do esforco de raciocinio (sem isto so `ultra` e
# proativo; medium/high/max recebem "Do not spawn sub-agents unless ... explicitly ask").
# Limite: 400 tokens (truncado alem disso).
multi_agent_mode_hint_text = """
Proactive multi-agent delegation is active in this session at every reasoning effort and in plan mode. Delegation is the default, not the exception: you are the orchestrator of a team, and the user expects the team to be used without being asked. Spawn sub-agents whenever a task (a) executes a plan document or touches more than ~3 files, (b) has two or more independent fronts (platforms, repositories, modules, questions), (c) needs investigation before implementation, (d) needs independent verification, or (e) benefits from a second model family. Keep local only the immediate blocking step, user-facing questions, git operations the user asked for, and one-file fixes. Always pass `agent_type`: pick the role whose description matches the task; never spawn a role-less `default` agent for real work. This mode stays active until the user says otherwise.
"""

# Playbook da raiz. Vai depois das secoes Reporting / Waiting / Delivery do fork.
root_agent_usage_hint_suffix = """
## Delegation playbook (root only)

Default team for "execute plan X.md" or any multi-file change, without being asked:
1. Read the plan yourself (it is the contract). Map each plan task to an owner and a disjoint write set.
2. Investigation first: one `explorer` per open question, all in parallel, `fork_turns: "3"`.
3. Implementation: one `executor_luna` per plan task (or `claude-sonnet` for bounded/mechanical work); `executor_sol` or `claude-opus` for the one task marked hard or cross-cutting. Tasks with disjoint write sets run at the same time, up to the concurrency limit.
4. Verification: a `tester` per completed task (or `claude-opus` for an independent review of a risky change). Do not re-verify yourself what a tester already ran.
5. Docs: `doc-writer` once, at the end, with the verified facts.
You integrate, resolve conflicts and report; you do not implement while agents are running.

Device and platform work (Windows/macOS/iOS/Android/Godot): one agent per platform in parallel; you keep only the steps that need the user's hands (unlock, approve, cable) and relay them in one message.

Second opinion or plan review: spawn `claude-opus` and `chatgpt-pro` in parallel with the same self-contained brief (`plaintext_message`, task-only context), then reconcile. `chatgpt-pro` replies take 5-20 minutes: wait for it with `wait_agent` at 600000 ms or more, never open a second ChatGPT conversation for the same question.

Image generation and visual references go through the `chatgpt` MCP server, not through the `chatgpt-pro` role.

Local Claude agents share the user's 5-hour window: if two Claude spawns in a row fail on limits, switch that slot to `executor_luna` / `luna` and say so.
"""
```

(`[agents] max_depth` fica em 1: nenhum subagente spawnou nada esta semana e um segundo
nível é onde os `wait` viram polling recursivo.)

### 3.2 `config.toml` — `developer_instructions` (substituição completa)

Mantém integralmente as duas seções que funcionam e acrescenta uma terceira, curta,
porque este texto é herdado por filhos sem role.

```toml
developer_instructions = '''
## Test execution and evidence proportionality

- Treat development tests as rerunnable by default. A failed preflight, environment check, test harness, or test run does not consume an irreversible attempt unless the owner or plan explicitly says so.
- Use the repository's existing test runner and its standard PASS, FAIL, SKIP, and NotExecuted evidence first. SKIP and NotExecuted never count as PASS.
- Do not invent one-shot latches, attempt budgets, no-retry rules, custom TRX parsers, temporary-script hashes, secret-escape matrices, or recursive review loops for routine tests.
- Hash or fingerprint only when required for mutable user-work preservation, release/signature/provenance, an explicit acceptance criterion, or a concrete irreversible production or security risk. Never hash a temporary test harness merely to prove that reviewers saw the same bytes.
- Keep reviews bounded to product changes and concrete risks. Do not start review-of-review cycles for auxiliary test infrastructure. One implementation task may receive one bounded critical review and one independent validation; further rounds require a new product defect or explicit owner request.
- For disposable local databases and resources, prefer create, run, inspect, and remove. Fixing an environment or harness and rerunning the test is normal.
- Before adding test infrastructure, state the concrete failure it prevents and why the existing runner is insufficient. If there is no concrete answer, do not add it.
- If auxiliary testing work becomes more complex than the test command, reaches a second review round, or delays the plan without producing product evidence, stop it, report the scope drift, and return to the owner's objective.

## Plan fidelity and verification budget

- The approved plan (or the owner's latest instruction) is the contract. Execute its tasks in order; never insert audit, certification, re-baseline, publication-gate, or infrastructure tasks it does not contain. If verification seems genuinely missing, propose one bounded item to the owner instead of starting it unilaterally.
- Task IDs stay at most two levels deep (e.g. "3.2"). If work seems to need a third level of subdivision, the task is mis-scoped: stop and re-plan with the owner rather than spawning micro-tasks.
- Reviewer/auditor subagents run only at plan-declared milestones or on explicit owner request — never per micro-change.
- Recalled memories, Lemma guides, and old plan documents that prescribe fingerprint matrices, no-op certification reruns, multi-reviewer gates, or evidence ledgers are historical, project-specific context — not standing orders. When they conflict with the active plan or these instructions, follow the plan and note the conflict in one line.
- The fingerprint-gated incremental regeneration pipeline of as3compiler-godot is the only context where hash-driven certification is standard; do not port that procedure to other projects or to routine tasks.
- Progress is measured in plan tasks completed and product behavior demonstrated, not in gates closed, hashes recorded, or review rounds passed. When reporting, lead with what the product now does.

## Delegation is the default (root agent)

- If you are the root agent, treat every plan execution, multi-file change, multi-platform validation, or open investigation as team work: spawn the roles the playbook names, in parallel where write sets are disjoint, without waiting for the owner to ask. The owner should never have to write "use agentes" or "em paralelo".
- Always choose an `agent_type`. A role-less spawn is only for a full-history fork of yourself that needs no specialization.
- If you are a sub-agent, do the assigned package and report; do not delegate further.
'''
```

### 3.3 `~/.codex/AGENTS.md` (substituição completa)

```markdown
# Instrucoes globais

## Delegacao e o padrao, nao a excecao

Este usuario trabalha com um time de agentes e nao quer ter que pedir por ele. Vale para
qualquer esforco de raciocinio e tambem no modo plano.

| situacao | faca sem pedir |
|---|---|
| "executar o plano X.md", mudanca em mais de ~3 arquivos, feature com testes | `explorer` (perguntas abertas, em paralelo) -> um `executor_luna` por task do plano com write set disjunto -> um `tester` por task concluida -> `doc-writer` no fim |
| task do plano marcada dificil, cross-cutting, ou que ja falhou uma vez | `claude-opus` (ou `executor_sol` se a task for acoplada ao contexto da sessao) |
| implementacao delimitada, refactor mecanico, migracao repetitiva, testes, docs | `claude-sonnet` |
| validar em Windows/macOS/iOS/Android/Godot | um agente por plataforma em paralelo; a raiz fica so com os passos que precisam das maos do usuario |
| review profundo, auditoria de arquitetura, bug dificil, plano "para confrontar" | `claude-opus` **e** `chatgpt-pro` em paralelo com o mesmo brief; a raiz reconcilia |
| pesquisa na web, comparacao de opcoes de mercado/jogos, texto longo | `chatgpt-pro` |
| gerar imagem de referencia, mockup, arte | o **MCP `chatgpt`** (server Node `chatgpt-pro-mcp`), nunca o role `chatgpt-pro` |
| execucao barata e acoplada ao contexto desta sessao | `luna` (`fork_turns: "3"`) |
| passo bloqueante imediato, pergunta ao usuario, git que o usuario pediu, fix de 1 arquivo | local |

Regras:

1. Sempre passe `agent_type`. Spawn sem role (`default`) so para um fork completo de si
   mesmo que nao precisa de especialista.
2. Um agente por task do plano, com ownership de arquivos escrito no brief. Diga sempre
   que ele nao esta sozinho na working tree.
3. A raiz nao implementa enquanto ha agentes rodando: ela le o plano, distribui,
   integra, resolve conflito e reporta.
4. `explorer`/`executor`/`tester`/`luna` recebem `fork_turns: "3"` (ou `"none"` com um
   brief completo). Nunca `"all"` em sessao longa: copia a transcricao inteira.
5. Claude e ChatGPT nao herdam contexto: brief autossuficiente, em `plaintext_message`.

## Agentes Claude

`spawn_agent` com `agent_type: "claude-opus"` ou `"claude-sonnet"`. Sao agentes Codex
comuns: `send_message`, `wait_agent`, `list_agents`, `interrupt_agent` e o `/agents`
funcionam igual aos Luna, e o rollout guarda a conversa. O modelo por tras e o CLI `claude`
local, na assinatura do usuario.

1. Use `plaintext_message` no lugar de `message`; `fork_turns` nao se aplica, entao a
   tarefa que voce manda tem que ser autossuficiente (objetivo, arquivos, criterio de
   aceite, comando de validacao).
2. Ele executa as **proprias ferramentas**. Valide o resultado (diff sempre; build/teste
   dentro do orcamento da task) antes de aceitar.
3. A janela de 5h do Claude e **compartilhada com o uso interativo do usuario**. Se dois
   spawns Claude seguidos falharem por limite, mova esse slot para `executor_luna`/`luna`
   e avise em uma linha.
4. Se mais de um agente for editar o mesmo repo ao mesmo tempo, coordene — eles
   compartilham a working tree, como os Luna.

## Agente ChatGPT Pro

`agent_type: "chatgpt-pro"` roda no chatgpt.com via Chrome. Serve para analise profunda,
review, pesquisa na web e segunda opiniao; com o conector "Codex Native" ele tambem
executa comandos e aplica patches atraves do Codex.

1. Brief autossuficiente em `plaintext_message`; anexe o que ele precisa ler (ou peca que
   ele use o conector).
2. **Ele demora 5 a 20 minutos.** `wait_agent` com `timeout_ms` >= 600000, uma vez por
   rodada. Silencio nao e travamento: cheque `list_agents` antes de qualquer
   `interrupt_agent`. Nunca abra uma segunda conversa para a mesma pergunta.
3. Imagens geradas do lado do ChatGPT nao voltam para o Codex; para imagem de referencia
   use o MCP `chatgpt`.

## Como os agentes reportam

Vale para TODOS os subagentes (Luna, Sol, Claude, ChatGPT) e para a raiz:

1. Um subagente reporta **na resposta final** do seu turno, ou por `send_message` com
   `target: ".."` quando precisa falar no meio da tarefa. Nada mais.
2. **Nunca** use as tools do `codex_app` (`send_message_to_thread`, `create_thread`,
   `fork_thread`, `handoff_thread`, `automation_update`) para reportar. Elas produzem os
   cards "Enviado por ChatGPT de outra tarefa" no thread do usuario e pedem permissao a
   cada chamada. `create_thread`/`fork_thread`/`handoff_thread` sao do **usuario**, nao
   suas.
3. Antes de `interrupt_agent`, cheque `list_agents`: agente com atividade recente esta
   trabalhando, nao travado. Interromper por impaciencia perde o trabalho inteiro.
4. `wait_agent` com timeout longo (minutos), uma vez por rodada. Nao faca polling
   apertado; o retorno vazio significa "ainda trabalhando".
5. Progresso duravel vai para disco: `agent_docs/latest_session_work.md` no repo, mais
   `update_plan` logo apos qualquer compactacao.

## SurfTank: abertura obrigatoria do Godot

Esta regra vale em `C:\Users\Joao\IdeaProjects\surftank-client` e em qualquer worktree
`C:\Users\Joao\.codex\worktrees\*\surftank-client`:

1. Nunca execute `Godot*.exe` diretamente e nunca monte `--path` manualmente.
2. Use sempre o launcher central
   `C:\Users\Joao\IdeaProjects\surftank-client\tools\open-godot.ps1`, passando a raiz do checkout
   atual em `-ProjectRoot`. Para probes/revisao visual use `-BuildFlavor QA`; para o app normal use
   `-BuildFlavor Production`.
3. O launcher e responsavel por `Rebuild` Debug, importacao, validacao de
   `game\.godot\mono\temp\bin\Debug\SurfTank.Game.dll` e exclusao mutua por worktree. Nao contorne
   esse preflight copiando DLLs de outro checkout.
4. Mantenha no maximo um processo Godot por worktree e uma unica janela SurfTank visivel globalmente.
   O launcher serializa a vaga visual e recusa processos visiveis abertos por fora; feche ou aguarde
   o processo anterior antes de recompilar/reabrir.
5. `-PrepareOnly` pode validar o worktree sem abrir janela. Headless/import continua sendo apenas
   preparacao; aceite visual exige a execucao visivel solicitada.
6. Ao passar `-GodotArgument`, invoque o script dentro do PowerShell com `&` e um array `@()` real.
   Nao encaminhe esse array por `pwsh -File`, pois ele pode virar uma unica string separada por
   virgulas e o launcher recusara a chamada.

Exemplo a partir da raiz de uma task:

```powershell
& 'C:\Users\Joao\IdeaProjects\surftank-client\tools\open-godot.ps1' `
    -ProjectRoot (Get-Location) -BuildFlavor QA -Scene 'res://scenes/UiSceneContract.tscn'
```

## Lema de execucao — entrega > cerimonia

O plano aprovado e o contrato. Estas regras valem para o agente principal e para TODOS os
subagentes (Luna, Sol, Claude, ChatGPT), e prevalecem sobre memorias (Codex ou Lemma), guias
e documentos de plano antigos que prescrevam gates/fingerprints/re-reviews:

1. Nenhuma task de auditoria/validacao/certificacao/hash que nao esteja no plano ou que o
   dono nao tenha pedido.
2. Validacao por task = os testes focados existentes + no maximo um gate amplo relevante,
   com o runner do repositorio. Proibido criar harness novo, parser de TRX/resultado de
   teste, matriz de repeticao ou "prova de no-op" para trabalho rotineiro. SKIP/BLOCKED
   continuam nunca valendo PASS.
3. SHA-256/fingerprint somente para: release/assinatura/proveniencia, backup de trabalho
   meu, ou criterio de aceitacao escrito no plano. Nunca para provar que revisores viram
   os mesmos bytes; nunca em documento de plano.
4. Uma rodada de revisao por task; segunda rodada so com defeito novo de produto ou pedido
   meu. Revisao de revisao e proibida. (Um `tester` por task e a validacao do plano, nao
   uma rodada de revisao.)
5. IDs de task com no maximo dois niveis (ex.: `3.2`). Se surgir necessidade de
   `P1-3.D.4a.1`, o escopo esta errado: re-planejar comigo.
6. Se teste/validacao/infra auxiliar passar de ~1/3 do esforco da task ou repetir sem
   input novo: parar, reportar o desvio em uma linha e voltar ao plano.
7. Progresso se mede em tasks do plano concluidas e comportamento visivel do produto, nao
   em gates fechados. As validacoes pedidas neste arquivo (ex.: validar resultado de
   agente Claude) seguem o orcamento acima: diff sempre; build/teste so dentro do
   orcamento da propria task.
8. Nao pergunte o que o plano ja responde. Se a resposta esta no plano, no pedido ou no
   codigo, siga; pergunte so quando duas leituras plausiveis levariam a trabalhos
   diferentes.
9. Worktree suja e o estado normal aqui. Nunca `git reset`, `checkout`, `clean`, `stash`
   ou `commit` sem pedido explicito meu; edicoes de arquivo saem por `apply_patch`.

Planos novos nascem no formato do "Plano enxuto" de 17/08: por task, uma linha
`Validacao:` com comandos existentes; certificacao fisica unica no marco final; sem fases
de auditoria intermediarias. Cada task do plano ja nasce com um dono de agente
(`executor_luna`, `claude-sonnet`, `claude-opus`…) e um write set; a raiz spawna por task.

@C:\Users\Joao\.codex\RTK.md
```

### 3.4 Roles — só a `description` muda (o orquestrador não vê o resto)

Formato que a raiz recebe: `nome: { description + "This role's model is set to … These
settings cannot be changed." + nota local }` (`role.rs:322-394`). As descrições abaixo
começam com **quando** e terminam com **como chamar**; os `developer_instructions` de cada
role ficam como estão.

```toml
# explorer.toml — recupera a orientacao do built-in que este arquivo sombreia
description = """
Use `explorer` for every open question about code, config, tools or a repository before
implementing: purpose, structure, call sites, what a plan task really touches. Read-only,
fast, authoritative: trust its report instead of re-reading the same files yourself.
Spawn several in parallel, one question each, whenever you have more than one question;
reuse an existing explorer for a related follow-up. Pass `fork_turns: "3"`.
"""

# executor_luna.toml
description = """
Default owner of one plan task or one bounded production change. Spawn one per plan task
with a disjoint write set, in parallel, as soon as the task's inputs are known; give it
the task capsule, the files it owns and the acceptance criteria. Tests and docs belong to
`tester` / `doc-writer` unless assigned. Pass `fork_turns: "3"`.
"""

# executor_sol.toml
description = """
The one task in a plan that is genuinely cross-cutting, coupled to the session's own
context, or that an executor_luna already failed at once. Not for routine tasks; at most
one or two per plan. Pass `fork_turns: "4"` so it sees the recent decisions.
"""

# tester.toml
description = """
Independent verification of one completed task: spawn it right after the executor
reports, with the diff, the acceptance criteria and the repository's existing test
command. It adds focused tests and runs the requested gate; it never fixes production
code. Do not re-run what it already ran. Pass `fork_turns: "3"`.
"""

# doc-writer.toml
description = """
Durable documentation only, once per plan at the end (or at a milestone the plan names):
turns verified facts the parent provides into the assigned docs. Not for
`agent_docs/latest_session_work.md` (the parent owns it). Pass `fork_turns: "none"` and a
self-contained brief.
"""

# luna.toml
description = """
Cheap general secondary agent coupled to this session's context: comparisons (Figma vs
screenshot), per-screenshot visual review, log triage, small scoped analyses. Spawn several
in parallel for per-item work. Pass `fork_turns: "3"`.
"""

# claude-opus.toml
description = """
Local Claude Opus (Claude Code CLI). Spawn it for: the hard or cross-cutting task of a
plan, a bug that resisted one attempt, an architecture audit, an independent review of a
risky diff, or a second opinion to contrast with the Luna/Sol result. Self-contained brief
in `plaintext_message` with files, acceptance criteria and validation command. Shares the
user's 5-hour Claude window: if two spawns in a row fail on limits, use executor_luna.
"""

# claude-sonnet.toml
description = """
Local Claude Sonnet (Claude Code CLI). Spawn it for bounded implementation, mechanical
refactors, repetitive migrations, test writing and docs — one per plan task, in parallel
with the Luna executors. Self-contained brief in `plaintext_message` with files owned,
acceptance criteria and validation command.
"""

# chatgpt-pro.toml
description = """
ChatGPT Pro on chatgpt.com in a Chrome tab. Spawn it, in parallel with `claude-opus`, for
plan reviews, deep analysis, web research and market comparisons, and as the second
model family in a "confront the models" request. With the 'Codex Native' connector it can
run commands and apply patches through Codex; otherwise it only sees the brief. Replies
take 5-20 minutes: wait with `wait_agent` at 600000 ms or more and never open a second
conversation for the same question. Not for image generation (use the `chatgpt` MCP
server). Self-contained brief in `plaintext_message`.
"""
```

### 3.5 Outras chaves

- `plan_mode_reasoning_effort = "high"` — manter, mas o `multi_agent_mode_hint_text`
  acima já cobre o modo plano ("and in plan mode"). Sem ele, planejar com o time só
  funciona em `ultra`.
- `[chatgpt_web] max_parallel_turns = 2` e `idle_timeout_ms = 1200000` (defaults) — bons;
  o problema do 27/08 foi a raiz, não o provider.
- Não mexer em `[agents] default_subagent_model`/`_reasoning_effort`: com "always pass
  `agent_type`" eles viram só o fallback do fork completo.

---

## 4. Mudanças no Codex (código/prompt) que valem a pena

Ordenadas por custo/benefício. Todas são carve-outs do fork; a #41165 upstream (27/08,
"Require explicit requests for spawn model overrides") mostra que o upstream caminha na
direção **oposta** (menos autonomia de spawn), então cada uma deve nascer com marcador
`FORK:` e teste próprio, como as seções de hint já existentes.

| # | onde | mudança | efeito esperado | custo / risco |
|---|---|---|---|---|
| 1 | `core/src/session/multi_agents.rs:266-273` `effective_multi_agent_mode` | Novo knob `[features.multi_agent_v2] proactive = "always" \| "ultra" \| "never"` (default `"ultra"` = upstream). `"always"` → `Proactive` em qualquer esforço e em modo plano. No mínimo, incluir `ReasoningEffort::Max` no braço proativo. | Remove a Causa A sem depender de texto custom (que é truncado a 400 tokens e perde o texto built-in). O texto padrão *"Proactive multi-agent delegation is active…"* continua o mesmo. | **Barato** (10 linhas + teste em `multi_agents.rs` `fork_hint_tests`). Risco baixo; o Desktop pode mudar o esforço por thread e hoje isso muda a política silenciosamente. |
| 2 | `core/src/session/multi_agents.rs:100-134` + `config/mod.rs:1505-1529` | Seção FORK "## Delegation playbook" na raiz, ligada por `delegation_playbook_hint: bool` (default `true`), no mesmo molde de `delivery_discipline_hint`. Conteúdo: o playbook de 3.1 em forma genérica (explorer → executor por task → tester por task; sempre `agent_type`; um agente por plataforma; segunda opinião com dois roles). | A política de delegação deixa de viver só no `config.toml` do dono e passa a ter teste; `root_agent_usage_hint_suffix` fica para o específico do projeto. | **Barato** (constante + append + 1 teste). Risco: tamanho do hint da raiz (+~250 tokens por sessão). |
| 3 | `core/src/tools/handlers/multi_agents_spec.rs:730` | `agent_type`: trocar *"Omit unless explicitly asked."* por *"Choose the role whose description matches the task; omit only for a full-history fork of yourself. The selected role applies regardless of how much parent history is inherited."* | Acaba com os 13 % de spawns sem role (e os forks completos que vêm junto). | **Barato** (1 string + `spawn_agent_description` test). Conflito de merge previsível nessa linha a cada sync (upstream mexeu nela em #41165). |
| 4 | `core/src/tools/handlers/multi_agents_spec.rs:844-855` `spawn_agent_tool_description_v2` | Quando o modo é proativo (ou knob #1 = always), substituir *"Only call this tool for a concrete, bounded subtask that can run independently alongside useful local work; otherwise continue locally."* por uma versão curta das seções "When to delegate / Designing delegated subtasks / Parallel delegation patterns" da v1 (`:799-834`), sem o parágrafo *"Do not spawn sub-agents unless…"*. A `usage_hint_text` já entra aqui (`:858-864`), então dá para fazer só por config: `[features.multi_agent_v2] usage_hint_text = "…"`. | A tool deixa de se auto-desaconselhar. | **Barato via config**, médio via código (a descrição é montada em `spec_plan.rs:1198/1251`). Risco: descrição da tool é enviada a cada request; manter < 200 tokens. |
| 5 | `core/src/tools/handlers/multi_agents_spec.rs:736` + `multi_agents_common.rs:51-76` | Default de `fork_turns` para roles com `developer_instructions` próprios: `"3"` em vez de `"all"` (novo knob `[agents] default_fork_turns`, e `[agents] max_fork_turns` como já existe para `claude_code`/`chatgpt_web`), com `notes` no resultado quando ajustar. | 97 forks completos/semana → 0 por acidente; cada spawn deixa de copiar transcrições de milhares de tool calls; spawnar "mais um explorer" fica barato de verdade. | **Médio** (schema, clamp, 3-4 testes). Risco: role que dependia do histórico inteiro perde contexto — mitigado pela nota no resultado e pelo brief. |
| 6 | `core/src/agent/role.rs:305-315` `build_from_configs` | Quando um role do usuário sombreia um built-in de mesmo nome (`explorer`), anexar a `description` built-in **antes** da do usuário (ou um campo opcional `orchestrator_hint` no toml do role que entra só na listagem). | O texto "spawn several explorers in parallel / trust their results" volta a existir mesmo com `explorer.toml` customizado. | **Barato** (concatenação + teste em `role_tests.rs`). Risco: listagem mais longa. |
| 7 | `core/src/agent/role.rs:351-353` nota local `CHATGPT_WEB_PROVIDER_ID` | Acrescentar *"Replies take 5-20 minutes; wait with `wait_agent` at 600000 ms or more, and never start a second conversation for the same question."* (e a latência equivalente para Claude: *"turns take minutes"*). | A raiz para de desistir do `chatgpt-pro` (27/08). | **Barato** (1 string). |
| 8 | `core/src/tools/handlers/multi_agents_v2/wait.rs:50-61` | Backoff: se o `wait` anterior do mesmo turno expirou sem mailbox, dobrar o timeout efetivo até `max_wait_timeout_ms` (estado por turno em `input_queue`), e dizer no `summary` que dobrou. | 81 % de timeouts → tende a zero sem depender do modelo escolher bem. | **Médio** (estado por turno, 2-3 testes em `wait_tests`). Risco: baixo; o wait v2 acorda em qualquer update. |
| 9 | `core/src/context/world_state/multi_agent_mode.rs:13` | Subir `MULTI_AGENT_MODE_MAX_TOKENS` de 400 para 800 (ou não truncar texto custom vindo da config, só o do catálogo). | O `multi_agent_mode_hint_text` de 3.1 cabe com folga e pode carregar o playbook. | **Trivial**. Risco: nenhum além de tokens por sessão. |
| 10 | `core/templates/agents/orchestrator.md:37-42` | O template já tem *"Prefer multiple sub-agents to parallelize your work… process them in parallel by spawning one agent per step"*, mas nenhum `.rs` o inclui (`grep` vazio). Ou ligar como seção opcional do hint da raiz, ou apagar para não enganar quem lê. | Higiene. | **Trivial**. |
| 11 | plano → agentes automático (`update_plan` handler) | Ao registrar um plano com N passos independentes em modo proativo, injetar um lembrete *"N independent steps: spawn one agent per step"* no resultado da tool. | Cobre o caso "plano de N passos executado em série". | **Invasivo** para o ganho: o playbook (#2) + AGENTS.md já dizem isso. Não recomendo agora. |

Não vale: mexer em `agents.max_depth` (subagentes delegando viram polling recursivo) nem
esconder `model`/`reasoning_effort` do spawn (`expose_spawn_agent_model_overrides`) — o
dono usa "todos no máximo" e o clamp da Fase 1 já protege.

---

## 5. Cheat-sheet — como escrever o prompt nos casos que ainda vão precisar de dica

Com 3.1–3.4 aplicados, "executar plano X.md" e "validar em N plataformas" não precisam
de nada. Onde ainda vale uma palavra:

| quer | escreva | evite |
|---|---|---|
| plano executado pelo time | `executar plano docs/PLANO-X.md` (só isso) | o molde "lembre-se de usar opus/sonnet/lunna…" — vira redundante; se ainda precisar, a config não pegou |
| segunda opinião de outra família | `confrontar: opus + chatgpt-pro` | "use o chatgpt" (ambíguo entre MCP e role) |
| imagem de referência / mockup | `gerar via MCP chatgpt` | "chatgpt pro" sozinho |
| pesquisa de mercado / web | `chatgpt-pro: pesquise …` | pedir isso ao Luna (não tem web) |
| um agente por item (screenshots, telas, repos) | `um luna por screenshot` / `um agente por repo` | "analise as screenshots" (a raiz faz em série) |
| esforço máximo num agente | `opus no máximo` já funciona; para Luna: `executor_luna reasoning_effort max` | "todos no máximo" vale só para OpenAI (Claude clampa) |
| não delegar (tarefa curta, git, pergunta) | `local:` / `sem agentes` | — |
| deixar de perguntar e seguir | `siga sem perguntar; prints por etapa` (já funciona; combine com "um agente por plataforma" se for multi-plataforma) | — |
| esperar o ChatGPT Pro | `espere o chatgpt-pro terminar (até 20 min)` — só até #7/#8 entrarem | "volte a esperar" depois do fato |
| thread em esforço baixo (medium/high) | nada, **depois** de `multi_agent_mode_hint_text`; antes disso, todo prompt precisa de "use agentes" | — |

---

## Anexo — inventário de onde cada texto que o modelo lê nasce

| texto | origem | quem lê |
|---|---|---|
| "Proactive … / Do not spawn sub-agents unless…" | `context/multi_agent_mode_instructions.rs:7-8`, modo em `session/multi_agents.rs:245-285` | raiz e subagentes (developer message, `<multi_agent_mode>`) |
| Hint da raiz ("You are `/root`…") + Reporting / Waiting / Delivery (FORK) | `session/multi_agents.rs:11-29, 100-134, 136-205` | raiz |
| Hint do subagente | `session/multi_agents.rs:31-48` | filhos OpenAI |
| Descrição de `spawn_agent` v2 + `agent_type` + `fork_turns` | `tools/handlers/multi_agents_spec.rs:714-760, 836-866`; opções em `tools/spec_plan.rs:1198, 1251` | quem tem a tool |
| Listagem de roles ("Available roles: …") | `agent/role.rs:296-394` (usuário sombreia built-in `:305-315`; built-ins `:403-465`) | idem |
| `developer_instructions` do config | herdado pelos filhos sem role (`agent/control/spawn.rs:822-843`); roles com `developer_instructions` usam o próprio | todos |
| AGENTS.md | `~/.codex/AGENTS.md` → user message `# AGENTS.md instructions`; filtrado para Claude/ChatGPT-web (`claude_code/history.rs:337`) | raiz e filhos OpenAI |
| Knobs `[features.multi_agent_v2]` | `features/src/feature_configs.rs:239-299`; resolução `config/mod.rs:2933-3020`; defaults `:232-245` | — |
