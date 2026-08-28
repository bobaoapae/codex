# Plan mode — Claude Code (Fable 5 @ max) vs Codex (gpt-5.6-sol @ ultra)

> Somente leitura: nada em `~/.codex`, `~/.claude*`, `config.toml` ou nos prompts internos foi
> alterado. Este documento é análise + proposta; a implementação fica para decisão do dono.
> Scripts e logs da simulação: `scratchpad/sim/{codex_driver.py,claude_driver.py,run*/}`
> (sessão `8f3eccae`). Anexo com os prompts extraídos: [`PROMPTS.md`](PROMPTS.md).

Pergunta do dono: *«Plano do Claude sempre é mais detalhado, pergunta mais coisas pro
usuário, várias rodadas de decisões; codex pergunta pouco e já fica por isso. Simule com
claude e codex, gere um relatório e investigue o código fonte do harness para ver caminhos
de melhora.»*

## 0. Resposta curta

1. **A diferença é sobretudo do harness, e no Codex é intencional.** O prompt de Plan mode
   do Codex (`codex-rs/collaboration-mode-templates/templates/plan.md`) foi **aparado pelo
   upstream em quatro commits** entre janeiro e março de 2026: caiu a «Hard interaction
   rule» (cada turno é *ou* pergunta *ou* plano), caíram «step-by-step edits», «acceptance
   criteria», e entrou «**concise by default** … 3-5 short sections … avoid naming more than
   3 paths … **minimum detail needed for implementation safety, not exhaustive coverage**».
   O texto ainda diz «You SHOULD ask many questions», mas também «If unanswered, proceed
   with the recommended option and record it as an assumption» — e o modelo segue a
   segunda frase.
2. **A tool de perguntas do Codex é mais estreita que a do Claude**: `request_user_input`
   diz ao modelo «Prefer 1 and do not exceed 3» perguntas, «2-3 mutually exclusive choices»,
   sem `multiSelect`, sem `preview`. `AskUserQuestion` aceita 1–4 perguntas, 2–4 opções,
   multi-seleção e previews lado a lado.
3. **O Codex não tem ciclo de revisão.** O plano sai num único bloco `<proposed_plan>` e o
   TUI abre logo «Implement this plan?» (`tui/src/chatwidget/turn_runtime.rs:226`). O
   Claude tem *Phase 3: Review*, *ExitPlanMode → rejeitar com feedback → o modelo revê no
   mesmo turno*, regra «your turn should only end with either AskUserQuestion OR
   ExitPlanMode», «Don't make large assumptions about user intent», ficheiro de plano
   incremental, reforço «Plan mode still active» em **todos** os turnos e reentrada com o
   plano anterior. Nada disto existe no Codex: as instruções de Plan mode entram **uma
   vez** como developer message e só voltam se mudarem
   (`core/src/context/world_state/collaboration_mode.rs:95-120`).
4. **Esforço:** o preset builtin de Plan é **Medium**
   (`models-manager/src/collaboration_mode_presets.rs:25`), o `config.toml` do dono fixa
   `plan_mode_reasoning_effort = "high"`, e o TUI só sobe para `ultra` de forma *efémera*
   quando a sessão já está em `ultra` (`tui/src/chatwidget/session_flow.rs:93`). Nos
   rollouts reais de agosto, os 32 turnos de Plan mode do Desktop correram em `ultra`, mas os
   8 turnos do CLI correram em `high` com `gpt-5.6-luna`. Ou seja: **o esforço não explica a
   diferença no Desktop, mas é uma armadilha no CLI** e para qualquer cliente que use o
   preset (`collaborationMode/list` devolve Medium).
5. **Simulação controlada** (mesma tarefa, mesmo repo, respostas automáticas «opção
   recomendada», §4): o Codex fez 3 perguntas e entregou **912 palavras com 0 paths** em
   22 min; o Claude fez **0 perguntas** na 1.ª ronda e entregou **4 096 palavras com 75 paths
   e 47 refs de linha** em 49 min ($43,6). Confrontados com «lista o que assumiste», ambos
   converteram assunções em perguntas (Codex 5 de 48; Claude 4 de 8). A diferença de detalhe
   é do prompt; a de perguntas está no **fluxo de revisão** (ExitPlanMode com feedback vs
   popup «Implement this plan?»), não no modelo.
6. **Caminhos de melhoria** (§5), por ordem de retorno/esforço: (a) template de Plan mode
   do fork com fase de *review* + checkpoint de decisões obrigatório antes do
   `<proposed_plan>`; (b) alargar o schema de `request_user_input` (1–4 perguntas, 2–4
   opções, multi-seleção) — mudança só no fork, sem protocolo novo; (c) reforço por turno
   (sparse reminder) em Plan mode; (d) opção «Revise: …» no popup «Implement this plan?» que
   mantém o Plan mode e injeta feedback; (e) preset de Plan a herdar o esforço da sessão em
   vez de Medium; (f) persistir o `<proposed_plan>` em `~/.codex/plans/` (que é,
   coincidentemente, a tarefa usada na simulação).

## 1. Como cada harness implementa o Plan mode

| Aspeto | Codex (fork, `codex-rs`) | Claude Code 2.1.247 |
|---|---|---|
| Entrada | `/plan` no TUI → `CollaborationModeMask{mode: Plan}`; app-server `turn/start.collaborationMode` | `EnterPlanMode` (tool, com aprovação) ou `--permission-mode plan` / Shift+Tab |
| Instruções ao modelo | Developer message `<collaboration_mode>` com `plan.md` (9,1 KB), **uma vez** por mudança de modo; catálogo remoto pode substituir (`CollaborationModeMessages.plan`) — hoje `null` para todos os modelos | System reminder `plan_mode` com preâmbulo read-only + **workflow em 5 fases** + nota «ask at any point»; reminder *sparse* **em cada turno** («Plan mode still active…»); `plan_mode_reentry` ao voltar |
| Fases prescritas | 3 «chats» (ground → intent → implementation), sem fase de revisão | 5 fases: Explore (subagentes **Explore**, em paralelo) → Design (subagentes **Plan**, «different perspectives») → **Review** («Use AskUserQuestion to clarify any remaining questions») → Final Plan (ficheiro) → ExitPlanMode |
| Tool de perguntas | `request_user_input`: «Prefer 1 and do not exceed 3», opções «2-3», sem multi-select, sem preview; só em Plan mode; bloqueante | `AskUserQuestion`: 1–4 perguntas, 2–4 opções, `multiSelect`, `preview`; «Users will always be able to select Other» |
| Regra de fim de turno | Nenhuma: o modelo pode terminar com texto livre | «your turn should only end with either using AskUserQuestion OR calling ExitPlanMode» |
| Assunções | «If unanswered, proceed with the recommended option and record it as an assumption in the final plan» | «Don't make large assumptions about user intent … tie any loose ends before implementation begins» |
| Formato do plano | «concise by default», 3-5 secções curtas, ≤3 paths, «minimum detail needed» | «Context section … name the critical files … reference existing functions with their file paths … verification section» |
| Artefacto | Bloco `<proposed_plan>` no texto → `TurnItem::Plan` no rollout; **sem ficheiro** | Ficheiro `plans/<slug>.md` escrito incrementalmente (único write permitido) |
| Aprovação / revisão | Ao ver um plan item, o TUI abre «Implement this plan?» (Yes / Yes, clear context / No, stay in Plan mode). Sem caminho «rejeitar com feedback» | `ExitPlanMode` → aprovar ou rejeitar **com feedback**; a rejeição volta como tool result e o modelo revê no mesmo turno |
| Subagentes | Só via modo proativo (esforço `ultra`); sem papel prescrito no plan prompt | Prescritos nas fases 1 e 2 (Explore/Plan), com N máximo configurado |
| Esforço | Preset Medium → override `plan_mode_reasoning_effort` → efémero Ultra se a sessão está em Ultra | Herda `--effort` / settings da sessão |
| Customização | `settings.developer_instructions` (app-server) substitui o builtin; no TUI não há config para isso | `--plan-mode-instructions` substitui o corpo do workflow (mantém preâmbulo e footer) |

Pontos de código (fork, `codex-rs/`):

- `collaboration-mode-templates/templates/plan.md` — o prompt.
- `models-manager/src/collaboration_mode_presets.rs:20-28` — preset Plan (`reasoning_effort: Medium`, `developer_instructions: plan.md`).
- `core/src/context/world_state/collaboration_mode.rs` — injeção como developer message; `render_diff` devolve `None` se nada mudou (sem reforço por turno).
- `core/src/tools/handlers/request_user_input_spec.rs` — schema/descrição da tool («Prefer 1 and do not exceed 3», «2-3 choices»); `request_user_input.rs:86` — `is_blocking = mode == Plan`.
- `core/src/tools/spec_plan.rs:1093` — a tool só é registada se `tools.experimental_request_user_input.enabled` (default **true**); `tools/src/tool_config.rs:17` — disponível apenas em Plan (Default só com feature `default_mode_request_user_input`).
- `core/src/session/turn.rs:2334` e `core/src/tools/handlers/plan.rs:87` — o único efeito de Plan mode no core é o parser de `<proposed_plan>` e o bloqueio de `update_plan`; **as tools de mutação continuam registadas** (o que trava é o prompt + a sandbox da sessão).
- `tui/src/chatwidget/turn_runtime.rs:226-266` + `tui/src/chatwidget/plan_implementation.rs` — popup «Implement this plan?».
- `tui/src/chatwidget/settings.rs:136-171`, `session_flow.rs:93`, `app/config_persistence.rs:763-795`, `app/thread_routing.rs:1580` — a dança do esforço em Plan mode.
- `tui/src/bottom_pane/request_user_input/mod.rs:61` — o TUI acrescenta «None of the above» + notas (`is_other` é forçado a `true` em `normalize_request_user_input_tool_args`).

## 2. Evidência: o Codex foi deliberadamente aparado

Histórico de `plan.md` (upstream `openai/codex`):

| commit | data | efeito na verbosidade / perguntas |
|---|---|---|
| `2d6757430` #10308 | 2026-01-31 | «**Hard interaction rule**: Every assistant turn MUST be exactly one of: A) a request_user_input tool call, B) the final plan, C) direct answer»; secção «**Ask a lot, but never ask trivia**» |
| `3dd9a37e0` #10329 | 2026-01-31 | remove a hard interaction rule; «Ask a lot…» passa a «Asking questions» («Strongly prefer using the request_user_input tool») |
| `cabb2085c` #9977 | 2026-01-26 | «make plan prompt less detailed»: retira «Step-by-step edits or patches described precisely» e «Acceptance criteria tied to observable outcomes» |
| `50084339a` #13284 | 2026-03-02 | «Adjusting plan prompt for clarity and verbosity»: acrescenta «concise by default», «3-5 short sections», «avoid naming more than 3 paths», «Prefer the minimum detail needed … not exhaustive coverage», «For v1 feature-addition plans, do not invent detailed schema, validation, precedence…» |
| `6bfc58a68` #29301 | 2026-06-21 | follow-ups: repetir o plano anterior se nada mudou |

O Claude foi na direção oposta no mesmo período: a versão 2.1.x acrescentou a fase de
Review, a regra de fim de turno, o *decision workshop* (página onde o utilizador clica em
cada decisão em aberto, oferecido «alongside your first clarifying questions») e a opção
de protótipo. Ver [`PROMPTS.md`](PROMPTS.md) §2.

Dois pormenores do Codex que amplificam a tendência:

- **Instrução única vs reforço por turno.** O `<collaboration_mode>` só é reemitido quando o
  hash das instruções muda. Numa sessão longa de Plan mode, o modelo tem o prompt a 100k+
  tokens de distância e nenhum lembrete; o Claude injeta o *sparse reminder* em todos os
  turnos e o reminder completo na reentrada.
- **O popup «Implement this plan?» aparece ao primeiro `<proposed_plan>`** do turno
  (`saw_plan_item_this_turn`), o que empurra o utilizador para implementar em vez de
  iterar; para dar feedback é preciso escolher «No, stay in Plan mode» e escrever à mão.

## 3. Dados reais deste PC

### 3.1 Codex — rollouts (`~/.codex/sessions/2026/**`, 2 554 ficheiros)

22 threads com turnos em Plan mode (abril–agosto):

- **59 turnos de Plan mode**, **23 chamadas a `request_user_input`**, **28 perguntas** no
  total → **0,4 chamadas/turno, 1,2 perguntas por chamada** (a tool permite 3).
- 28 blocos `<proposed_plan>`; mediana **783 palavras** (mín. 8, máx. 4 085). Os planos de
  agosto no Desktop (`gpt-5.6-sol` @ ultra) tiveram 4 085 / 1 199 / 2 129 palavras — ou seja,
  o modelo *consegue* ser detalhado; o que falta são as rondas de decisão.
- Esforço: 32 turnos `vscode` (Desktop) em `ultra`; 8 turnos `cli` em `high`
  (`gpt-5.6-luna`); 1 em `low` (`gpt-5.4-mini`, smoke test desta sessão).
- Das threads de agosto, várias são forks/resumes da mesma thread de 17/08 (mesmos planos
  repetidos, 0 perguntas) — o `thread/fork` copia o plano mas não recomeça o diálogo.

### 3.2 Claude Code — transcripts (`~/.claude-wrapper/views/2/projects/**`)

24 sessões com plan mode entre 30/07 e 26/08 (excluídas as 3 sessões de 27/08 que são
os testes desta análise); modelos `fable-5` (14), `opus-5` (8), `sonnet-5` (1):

- **32 chamadas a `ExitPlanMode`** (≈ planos apresentados), **56 chamadas a
  `AskUserQuestion`**, **151 perguntas** → **2,7 perguntas por chamada** (a tool permite
  4) e **≈4,7 perguntas por plano apresentado**. No Codex: 23 chamadas / 28 perguntas para
  28 planos → **1,2 por chamada, ≈1,0 por plano**.
- Rondas: 9 sessões têm 2–5 `ExitPlanMode` (plano → feedback → plano revisto) — é o ciclo
  nativo de rejeição-com-feedback; no Codex, revisões aparecem como novos `<proposed_plan>`
  no mesmo thread (4 threads com ≥2 planos) mas sem nenhum `request_user_input` pelo meio.
- 91 subagentes (`Agent`) lançados nas sessões de agosto (mediana 4 por sessão), a maioria
  Explore/Plan nas fases 1–2 do workflow; o Codex só lança subagentes em `ultra`.
- Caveat óbvio: tarefas diferentes. Por isso a simulação controlada em §4.

## 4. Simulação controlada

### 4.1 Setup

- **Tarefa** (idêntica, em PT; [`simulacao/tarefa.md`](simulacao/tarefa.md)): persistir os
  `<proposed_plan>` em `~/.codex/plans/`, `/plans` no TUI para listar/carregar, e exposição no
  app-server para o Desktop. Escolhida por ter ambiguidades reais de produto (nome/formatos,
  por thread vs por bloco, o que é «carregar como contexto», API estável vs experimental,
  retenção, opt-out).
- **Repo**: este fork, `cwd = C:\Users\Joao\RustProjects\codex`, com o `AGENTS.md`/`CLAUDE.md`
  e as skills que cada harness carrega normalmente.
- **Codex**: `codex app-server` (o mesmo caminho do TUI e do Desktop), `turn/start` com
  `collaborationMode = {plan, gpt-5.6-sol, reasoning_effort: ultra, developer_instructions:
  null}` → template builtin; sandbox `read-only`, `approvalPolicy: never`. Driver:
  [`simulacao/codex_driver.py`](simulacao/codex_driver.py).
- **Claude Code**: `claude -p --permission-mode plan --model claude-fable-5 --effort max
  --input-format/--output-format stream-json --permission-prompt-tool stdio`. Driver:
  [`simulacao/claude_driver.py`](simulacao/claude_driver.py).
- **Utilizador simulado**: toda a pergunta é respondida com a **primeira opção** (os dois
  prompts mandam pôr a recomendada primeiro). Ronda 1 = tarefa até ao plano. Ronda 2 = o
  mesmo follow-up para ambos ([`simulacao/followup.md`](simulacao/followup.md)): *«lista todas
  as decisões que tomaste por assunção; para cada uma, a alternativa descartada e se devia ter
  sido uma pergunta; depois o plano final atualizado»*. No Claude, a ronda 2 é a rejeição do
  `ExitPlanMode` com esse texto como feedback (o ciclo nativo); no Codex é um novo
  `turn/start` em Plan mode.
- Caveats: (1) os dois modelos encontraram e leram os rascunhos untracked deste relatório em
  `docs/plans/2026-08-27-plan-mode/` — contaminação simétrica (o Claude citou o §5 P6 na
  secção *Context*); (2) uma amostra por harness — as conclusões de **detalhe** são robustas
  (diferença de 4×), as de **contagem de perguntas** não são (ver §3 e 4.3); (3) o primeiro par de
  runs foi descartado por bugs dos drivers (o do Codex confundia `turn/completed` de
  subagentes com o do thread raiz; o do Claude parava no primeiro `result`, quando o modelo
  termina o turno à espera de subagentes em background).

### 4.2 Resultados

| | Codex `gpt-5.6-sol` @ ultra | Claude Code `fable-5` @ max |
|---|---|---|
| **Ronda 1 — duração** | 22,4 min | 48,9 min |
| Exploração (thread raiz) | 142 comandos shell | 315 Bash + 232 Read + 83 Grep + 7 Glob |
| Subagentes | 4 threads (proativo/ultra), 553 comandos | 7 tarefas: 5 **Explore** + 2 **Plan** (perspetivas backend e TUI) |
| Perguntas ao utilizador | **1 chamada, 3 perguntas** (aos 14m24s; 2–3 opções cada) | **0** — decisões D1–D8 escritas como «assumidas» no plan file |
| Plano | **912 palavras**, 5 secções (Resumo, Contratos, Implementação, Testes, Assunções), **0 paths `.rs`, 0 refs `ficheiro:linha`**, 0 blocos de código | **4 096 palavras**, 10 secções (Context, Decisões, Arquitetura, Partes 1–4, Ordem, Verificação, Riscos), **75 paths `.rs` distintos, 47 refs `:linha`**, 10 blocos de código |
| Tokens / custo | 11,7 M input (11,3 M em cache) + 45 k output (28,7 k raciocínio) no thread raiz; subscrição ChatGPT | 1,28 M cache-read + 161 k cache-write + 54 k output (32 k thinking) só na primeira parte; **$43,6** acumulados |
| **Ronda 2 — duração** | 2,9 min | 2,4 min |
| Reação ao follow-up | **2 chamadas, 5 perguntas** (cobertura de threads, API experimental/estável, visibilidade no transcript, retenção, plano >10k tokens) + tabela de auditoria com **48 decisões**, **5** marcadas «devia ter sido pergunta» — exatamente as 5 que perguntou | **1 chamada, 4 perguntas** (3–4 opções cada: organização dos ficheiros, superfície app-server, UX do `/plans`, opt-out) + auditoria: *«Devia ter perguntado as de preferência antes de escrever o plano»* — D2, D5, D7, D8 reclassificadas |
| Plano revisto | 960 palavras, mesma estrutura | 4 216 palavras; tabela «Confirmadas pelo utilizador» + «Técnicas» + «Assunções secundárias» |

Ficheiros: [`simulacao/perguntas.md`](simulacao/perguntas.md),
[`simulacao/codex-plano-r1.md`](simulacao/codex-plano-r1.md) / [`r2`](simulacao/codex-plano-r2.md),
[`simulacao/codex-auditoria-r2.md`](simulacao/codex-auditoria-r2.md),
[`simulacao/claude-plano-r1.md`](simulacao/claude-plano-r1.md) / [`r2`](simulacao/claude-plano-r2.md),
[`simulacao/claude-auditoria-r2.md`](simulacao/claude-auditoria-r2.md).

### 4.3 O que a simulação mostra

1. **Detalhe: a diferença é do prompt, não do modelo.** O plano do Codex não cita um único
   ficheiro — o template diz «avoid naming more than 3 paths» e «prefer behavior-level
   descriptions», e o modelo obedece à letra. A auditoria da ronda 2 prova que ele *tinha* o
   detalhe (48 decisões com alternativa e justificação, cada uma ancorada em código); só não o
   escreveu. O Claude tem regra oposta («name the critical files … reference existing
   functions with their file paths … verification section») e produziu 4,5× mais texto com
   75 paths.
2. **Perguntas: os dois assumem por defeito.** Nesta tarefa o Claude fez **zero** perguntas
   na ronda 1 — pôs as 8 decisões numa tabela «assumidas» e chamou `ExitPlanMode`; o Codex
   fez 3. Nos dados reais (§3) a média inverte-se (≈4,7 vs ≈1,0 por plano). Ou seja, a
   perceção «o Claude pergunta mais» é verdadeira em média mas tem variância alta; o que é
   estrutural é **onde** as perguntas acontecem: no Claude o `ExitPlanMode` rejeitado com
   feedback é um passo nativo do fluxo (o utilizador lê o plano, recusa, o modelo pergunta e
   revê no mesmo turno), no Codex o popup «Implement this plan?» aparece ao primeiro plano
   e a revisão exige que o utilizador escreva à mão.
3. **O empurrão funciona nos dois.** Com o follow-up «lista o que assumiste», ambos
   converteram assunções em perguntas de qualidade (Codex 5/48, Claude 4/8) em <3 min. É
   isto que o P1.2 (checkpoint de decisões antes do `<proposed_plan>`) e o P4 (opção «Ask me
   the open decisions first» no popup) automatizam.
4. **Multi-agente: o Codex em ultra já explora com subagentes** (4 threads, 553 comandos)
   mas nenhum faz *design*; o Claude usa 2 agentes **Plan** com perspetivas distintas e o
   thread raiz *verifica* as afirmações deles contra o código antes de escrever («I have
   verified every referenced path and line»). Isto explica parte da diferença de precisão
   (47 refs de linha). → P1.5.
5. **Custo/tempo**: o Claude demorou 2,2× mais e custou $43,6; o Codex 25 min dentro da
   subscrição. Um template mais exigente no Codex vai aproximar o tempo — é o preço do
   detalhe, e é opt-in por modo.

## 5. Caminhos de melhoria no harness

> **Estado (27/08/2026): P1–P6 IMPLEMENTADOS** no fork, conforme `PLANO.md`. P7
> (`codex exec --plan`) ficou fora do âmbito. Resumo do que ficou no código:
>
> | # | Estado | Onde |
> |---|--------|------|
> | P1 | feito | `collaboration-mode-templates/templates/plan.md` (PHASE 4, decision checkpoint, regra de fim de turno, formato do plano, sub-agentes) + `plan_mode_instructions()` em `models-manager/src/collaboration_mode_presets.rs`, ligado em `app-server/src/request_processors/turn_processor.rs` e `tui/src/collaboration_modes.rs` (override `$CODEX_HOME/plan_mode.md`) |
> | P2 | feito | `core/src/tools/handlers/request_user_input_spec.rs` (1–4 perguntas, 2–4 opções) |
> | P3 | feito | `core/src/context/plan_mode_reminder.rs` + `core/src/session/plan_reminder.rs`, chamado em `core/src/session/turn.rs`; filtrado para subagentes em `core/src/agent/control/spawn.rs` |
> | P4 | feito | `tui/src/chatwidget/plan_implementation.rs` (+2 itens: «Ask me the open decisions first», «Revise the plan…») e `AppEvent::SetComposerText` |
> | P5 | feito | preset de Plan com `reasoning_effort: None` (herda a sessão); guard de `set_reasoning_effort` só ativo com override |
> | P6 | feito | crate `codex-rs/plans`, hook em `turn.rs`, `plan/list`+`plan/read` no app-server, `/plans` no TUI (`tui/src/chatwidget/saved_plans.rs`, `tui/src/app/plans_picker.rs`) |
>
> A descrição abaixo é a proposta original, mantida como registo do desenho.


Ordenado por retorno ÷ esforço. «Fork-only» = não precisa de protocolo novo nem de mudar
o Desktop; «sync» = risco de conflito nos merges com `openai/codex` (política do fork: mudar
o mínimo de código upstream).

### P1 — Template de Plan mode do fork (prompt-only) — **maior retorno, menor risco**

O que muda no texto (`collaboration-mode-templates/templates/plan.md`):

1. **Regra de fim de turno** (recupera a «Hard interaction rule» de #10308, na forma do
   Claude): *«End a Plan-mode turn only with (a) a `request_user_input` call, (b) a
   `<proposed_plan>`, or (c) a direct answer to a simple question. Never end with prose that
   describes what you would ask.»*
2. **Checkpoint de decisões antes do plano**: *«Before emitting `<proposed_plan>`, list every
   preference/tradeoff you resolved by assumption. If there is at least one, you MUST first
   call `request_user_input` with the highest-impact ones (batch up to N), recommended option
   first. Only proceed without asking when zero such assumptions remain.»* — substitui «If
   unanswered, proceed with the recommended option and record it as an assumption», que hoje
   dá ao modelo a saída fácil.
3. **Fase de revisão** (Phase 3 do Claude): reler os ficheiros críticos, confrontar o plano
   com o pedido original, perguntar o que resta.
4. **Formato do plano**: trocar o parágrafo «concise by default … avoid naming more than 3
   paths … minimum detail needed» pelas regras da Phase 4 do Claude (secção *Context*,
   ficheiros críticos com paths, funções/utilitários existentes a reutilizar com paths,
   sequência e dependências, secção de verificação end-to-end). Manter «compress unaffected
   behavior» para não voltar aos inventários ficheiro-a-ficheiro.
5. **Multi-agente em ultra**: dizer explicitamente que em Plan mode os subagentes servem para
   *grounding* (exploradores) e *perspetivas de design* (2–3 planos alternativos a sintetizar),
   como as fases 1–2 do Claude. Na simulação o modelo já lançou 5–6 exploradores sozinho, mas
   nenhum «designer».

Onde ligar sem tocar no upstream: hoje o TUI e o app-server preenchem
`developer_instructions: null` com `builtin_collaboration_mode_presets()`
(`models-manager/src/collaboration_mode_presets.rs:20`, usado em
`app-server/src/request_processors/turn_processor.rs:385` e em
`tui/src/collaboration_modes.rs:56`). Um único ponto — `plan_preset()` — pode ler
`$CODEX_HOME/plan_mode.md` (ou `[collaboration_modes.plan] instructions_file` no
`config.toml`) e usar o builtin como fallback. Cobre TUI, Desktop e qualquer cliente
app-server, e sobrevive aos syncs porque o ficheiro do upstream fica intacto. Nota: se um
dia o catálogo remoto passar a enviar `collaboration_modes.plan`, ele tem precedência
(`core/src/context/world_state/collaboration_mode.rs:26-36`) — vale um aviso no log.

### P2 — Alargar `request_user_input` (fork-only, sem protocolo novo na versão mínima)

- `core/src/tools/handlers/request_user_input_spec.rs`: «Prefer 1 and do not exceed 3» →
  «1 to 4 related decisions per call; batch decisions that belong to the same design
  choice»; «Provide 2-3 mutually exclusive choices» → «2-4». O TUI já suporta várias
  perguntas por chamada (cabeçalho de progresso em `bottom_pane/request_user_input/render.rs:266`)
  e opções longas.
- Versão completa: `multi_select` na pergunta (`protocol/src/request_user_input.rs`,
  `app-server-protocol/src/protocol/v2/item.rs:1712`, widget do TUI) — mudança de protocolo
  experimental; o Desktop ignoraria o campo até o suportar. Fica para depois.

### P3 — Reforço por turno em Plan mode (fork-only, ~40 tokens/turno)

Nova secção de `world_state` (ou fragmento em `core/src/session/turn_input.rs`) que, quando
`turn_context.mode() == Plan`, injeta em **todos** os turnos: *«Plan mode still active:
read-only; end this turn with request_user_input or a `<proposed_plan>`; unresolved
tradeoffs must be asked, not assumed.»* Hoje `render_diff` em
`world_state/collaboration_mode.rs:95-120` só reemite quando o hash muda — depois de 100k
tokens de exploração o prompt está longe. É exatamente o «sparse reminder» do Claude.

### P4 — Popup «Implement this plan?» com caminho de revisão (fork-only, TUI)

`tui/src/chatwidget/plan_implementation.rs` + `turn_runtime.rs:253`: acrescentar
«**Revise the plan…**» que mantém a máscara Plan e abre o composer com um prefixo
(«Revise the plan: …»), e «**Ask me the open decisions first**» que submete *«Before I
approve: list every design decision you assumed; for each, the discarded alternative and
whether it should have been a question; then re-emit the plan.»* — foi o follow-up usado na
simulação e funciona (§4). Custo: uma `SelectionItem` extra.

### P5 — Esforço do preset de Plan (fork-only, 3 linhas)

`collaboration_mode_presets.rs:25` `reasoning_effort: Some(Some(Medium))` →
`None` (herdar a sessão), e em `tui/src/chatwidget/settings.rs:146` tratar o override como
«nunca abaixo do esforço da sessão». Elimina a armadilha do CLI (8 turnos em `high`) e a
lógica especial só-para-Ultra de `session_flow.rs:93`.

### P6 — Persistir planos (`~/.codex/plans/` + `/plans`)

Foi a tarefa da simulação; os dois planos gerados (`scratchpad/sim/run2_*/plan_*.md`) são
um ponto de partida. Ponto de ancoragem no core:
`core/src/session/turn.rs` (`maybe_complete_plan_item_from_message`). Dá ao Codex o que o
Claude tem com o plan file: reentrada, revisão entre sessões, planos legíveis fora do rollout.

### P7 — `codex exec --plan` (para medir)

`exec/src/lib.rs:996` envia `collaboration_mode: None`. Um flag que envie a máscara Plan
permitiria A/B de templates com o mesmo driver desta simulação sem app-server. Baixa
prioridade; o driver `codex_driver.py` já faz isto via app-server.

### O que **não** recomendo

- Forçar `request_user_input` por código (ex.: rejeitar um `<proposed_plan>` na primeira
  ronda sem perguntas): o modelo responderia com perguntas de enchimento; o checkpoint no
  prompt (P1.2) dá o mesmo efeito sem mecânica.
- Trazer o `--plan-mode-instructions` como flag: o fork já tem `developer_instructions` e
  `AGENTS.md`; o ponto certo é o preset (P1).
