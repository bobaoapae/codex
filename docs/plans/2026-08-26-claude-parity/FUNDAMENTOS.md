# Fundamentos: a ideia por trás de cada fase

> Companheiro do [PLANO.md](./PLANO.md). O plano diz *o que* mudar e *onde*; este documento explica
> *por que* cada fase existe, qual mecanismo ela usa, que alternativas foram descartadas e como saber
> que funcionou. A numeração das fases é a mesma.

Base de evidência: 2.607 rollouts de `~/.codex/sessions` (27/07–26/08/2026), 130 threads raiz,
1.852 subagentes Luna/OpenAI e 748 subagentes Claude (`claude_code`), mais 494 mensagens únicas do
usuário lidas uma a uma; mapa completo do código do fork (`codex-rs/core/src/claude_code/*`,
spawn, roles, tools) e inspeção do binário `claude.exe` 2.1.246.

---

## Modelo mental

### Hoje: o Claude é uma caixa preta pendurada no Codex

```
thread raiz (Luna)                       filho Claude
──────────────────                       ────────────────────────────────────────────
spawn_agent(claude-opus) ──► Codex cria sessão filha
                              │  renderiza TODO o histórico/developer message em texto
                              │  (AGENTS.md inteiro, plugins, model_switch, multi_agent_mode…)
                              ▼
                        claude.exe --print --output-format stream-json ...
                              │  stdin: 1 linha (o texto acima) e fecha
                              │  Claude roda suas PRÓPRIAS tools (Bash/Edit/Read…)
                              │  stdout: assistant/text, thinking, tool_use, tool_result, result
                              ▼
                        translate_stream
                              │  text      → mensagem
                              │  thinking  → reasoning
                              │  tool_use  → linha de reasoning "[Bash] cargo test"
                              │  tool_result → DESCARTADO
                              ▼
pai vê: reasoning + resposta final. Sem células de exec, sem diff, sem saída de comando,
sem aprovação, sem limite de conta, sem sinal de "ainda estou trabalhando".
```

Consequências medidas: o pai não distingue "pensando" de "travado", então interrompe (70% dos
`interrupt_agent` miram filhos Claude; 46% das sessões `claude-opus` têm turno abortado); o filho
recebe 37k–1,39M caracteres de preâmbulo que não são a tarefa; decisões de permissão do CLI em modo
`auto` são terminais porque ninguém está do outro lado para responder.

### Depois: o Claude é um agente Codex cujo modelo é servido por outro processo

```
thread raiz (Luna)                       filho Claude
──────────────────                       ────────────────────────────────────────────
spawn_agent(claude-opus) ──► Codex cria sessão filha
                              │  brief = tarefa + turnos DESTE filho (filtro por content_item_kinds)
                              │  system prompt = role + protocolo curto + cwd/roots/modo
                              ▼
                        claude.exe ... --permission-prompt-tool stdio
                              │  stdin ABERTO durante o turno (protocolo de controle)
                              │  initialize{appendSystemPrompt, sdkMcpServers:["codex"]}
                              │
                              │  ◄── can_use_tool ──── Claude quer rodar Bash/Edit
                              │  ──► allow/deny ─────  (UI de aprovação do Codex decide)
                              │
                              │  ◄── mcp_message ───── Claude chama mcp__codex__send_message
                              │  ──► resposta ───────  (ToolRouter real do pai executa)
                              │
                              │  tool_use + tool_use_result (casados por tool_use_id)
                              ▼
                        translate_stream + ProviderExecutedTool
                              │  Bash  → célula de exec com stdout/stderr
                              │  Edit  → FileChange + TurnDiff
                              │  outros→ DynamicToolCall
                              │  result → RateLimitSnapshot da conta (get_usage)
                              ▼
pai vê o mesmo que vê de um filho Luna: células, diffs, aprovações, atividade recente em
list_agents/wait_agent, conta e limites no status. O filho fala com o pai pelos canais nativos.
```

O ponto central: **nada disso exige reimplementar o Claude dentro do Codex**. O CLI já expõe um
protocolo de controle bidirecional (usado pelo SDK oficial); o fork só precisa falar esse protocolo
e traduzir o que sai dele para os eventos e itens que o Codex já sabe exibir.

---

## Fase 0 — hoje, sem Rust: config, AGENTS.md, roles, retirada do MCP

**Problema observado.** Desde 25/08 (Desktop 26.820) subagentes chamam
`codex_app.send_message_to_thread` mirando o thread pai; cada chamada vira um turno de usuário
falso ("Enviado por ChatGPT de outra tarefa") e abre um diálogo de aprovação. O AGENTS.md ainda
documenta o caminho MCP `claude_agents` (zero uso desde 15/08) e instruções que o código já impõe.
O usuário cola em todo prompt as mesmas ordens (worktree suja, "pare de perguntar", "documente").

**Ideia central.** Remover o que já não vale, dizer em texto o que os agentes ainda precisam
ouvir, e cortar a fonte imediata do ruído com a única alavanca que o Desktop não sobrescreve:
`disabled_tools`. Tudo é config e prosa; entra hoje e serve de baseline para medir as fases seguintes.

**Por que assim e não de outro jeito.** `approval_mode = "approve"` no config do usuário não
funciona: a política de plugin só pode *apertar* (`restrict_to` em `config/src/mcp_types.rs`), e o
override do Desktop já define `prompt`. Interceptar a chamada em código seria Fase 7. Deixar o
AGENTS.md como está custa contexto em todo turno e convida o modelo a procurar `claude_spawn`.

**O que muda para o usuário.** Zero diálogos "Allow the codex_app MCP server…", zero cards de
"outra tarefa" no pai; temporariamente a raiz também perde `create_thread`/`fork_thread` (volta na
Fase 7.1). `wait_agent` default de 2 min em vez de 30 s reduz o polling.

**Riscos / como saber que funcionou.** Risco: precisar de `create_thread` da raiz antes da Fase 7
(aceitar o prompt só para essa tool). Sucesso: `grep codex_delegation` no rollout do pai não
encontra tráfego subagente→pai numa sessão nova com `explorer` + `claude-opus`.

---

## Fase 1 — robustez de spawn

**Problema observado.** Spawns Claude falham por motivos que não são do usuário: "Service tier
`priority` is not supported for model `claude-opus-5`" (o texto do role *convida* o modelo a
passar tier), "Reasoning effort `max` is not supported", `message` cifrado em vez de
`plaintext_message`, `fork_turns` ignorado em silêncio. Cada falha é uma rodada perdida do pai.

**Ideia central.** Um spawn é caro de repetir e nenhum desses parâmetros é decisivo: **clampar e
avisar** (`notes` no resultado) em vez de recusar. O modelo aprende pelo resultado, não pelo erro.

**Por que assim e não de outro jeito.** Auto-converter `message` para o Claude é impossível: o
campo é cifrado para o backend OpenAI e o core não tem a chave — só resta descrever melhor o campo
e manter o erro acionável. Silenciar `fork_turns` (comportamento atual) esconde do pai que o filho
não tem contexto; a nota corrige a expectativa.

**O que muda para o usuário.** Spawns que antes falhavam 1–3 vezes passam de primeira; o pai vê
por que um parâmetro foi ajustado.

**Riscos / como saber que funcionou.** Risco baixo (≈80 linhas). Sucesso: `spawn_agent` com
`service_tier:"priority"`, `reasoning_effort:"max"` e `fork_turns:"3"` em um `claude-opus` sucede
com três notas.

---

## Fase 2 — contexto limpo para o filho Claude

**Problema observado.** O preâmbulo renderizado para o filho tem mediana 37k chars e máximo 1,39M:
AGENTS.md inteiro, `<recommended_plugins>`, `<model_switch>`, o developer message de colaboração
("você pode spawnar…") seguido de `<multi_agent_mode>` ("não spawne…"). O stripping atual é por
texto e guloso — corta do marcador até o fim da mensagem, levando junto instruções do role que
vierem depois de `## Memory`.

**Ideia central.** Cada fragmento injetado pelo Codex já carrega uma etiqueta estável
(`content_item_kinds`, ex. `agents_md.instructions`, `plugins.recommendations`,
`multi_agent.mode_instructions`). Filtrar **por etiqueta**, não por texto, e mover as instruções do
role para `appendSystemPrompt`, onde o CLI as trata como sistema (e cacheia).

**Por que assim e não de outro jeito.** Texto muda a cada sync upstream; etiquetas são a API
interna que o próprio Codex usa para anotar conteúdo, preservada até o driver
(`context_manager/updates.rs` mantém o zip 1:1; só o caminho Responses limpa em
`client.rs:998-1007`). Passar o system prompt como flag de linha de comando esbarra no limite do
Windows; via `initialize` não há limite. `fork_turns` volta como opt-in com teto porque agora o
transcrito forkado é limpo — antes era exatamente o vazamento que o usuário reclamou em 19/08.

**O que muda para o usuário.** Filho Claude recebe a tarefa e só a tarefa; menos tokens, menos
contradição, cache de prompt estável entre turnos do mesmo filho.

**Riscos / como saber que funcionou.** Risco: uma etiqueta nova upstream não listada (default:
manter — nunca perder conteúdo do usuário). Sucesso: `RUST_LOG=codex_core::claude_code=debug`
mostra `turn_text` < 4k chars num filho novo; teste "role instruction após `## Memory` sobrevive".

---

## Fase 3 — transporte do protocolo de controle

**Problema observado.** Hoje o Codex escreve uma linha em stdin e fecha o pipe. Isso torna o CLI
surdo: sem canal de volta não há aprovação, não há MCP hospedado pelo cliente, não há `get_usage`,
e o system prompt só entraria por flag.

**Ideia central.** Manter stdin aberto durante o turno e falar o protocolo que o SDK oficial usa:
`control_request`/`control_response`/`control_cancel_request`, começando por
`initialize{appendSystemPrompt, sdkMcpServers}`. É **uma** mudança de transporte que destrava as
Fases 4, 5 (parcialmente), 6 e 8.4.

**Por que assim e não de outro jeito.** A alternativa (um servidor MCP externo + porta + token para
aprovações e tools) foi descartada porque o CLI já oferece tudo pelo stdio que o Codex controla:
sem processo órfão, sem autenticação, morre com o filho por construção. A capacidade foi
confirmada no binário instalado (`--permission-prompt-tool stdio`, `sdkMcpServers`,
`can_use_tool`, `mcp_message`, `get_usage`, `end_session`). Por ser interna ao CLI, entra atrás de
feature flag com fallback automático ao caminho atual se `initialize` falhar.

**O que muda para o usuário.** Nada visível por si só; é a fundação.

**Riscos / como saber que funcionou.** Risco: com stdin aberto o CLI pode não sair sozinho
(mitigação: `end_session` + `kill_process_tree` + idle watchdog já existentes). Sucesso: teste de
fixture — um `control_request` de entrada produz os bytes exatos de `control_response`.

---

## Fase 4 — aprovações e mapeamento de sandbox

**Problema observado.** Em 06/08 dez auditorias `claude-opus` foram perdidas: o Claude não
conseguia ler o segundo repositório ou rodar `dotnet` ("This command requires approval"). Causa
raiz confirmada no binário: em `--permission-mode auto`, sem uma superfície de prompt, toda decisão
"ask" é **terminal**. O Codex mapeava `Never → bypassPermissions` e "qualquer outra coisa → auto",
ignorando o sandbox.

**Ideia central.** Dois mapeamentos explícitos: (1) política Codex (sandbox × approval) → modo de
permissão do CLI + `--add-dir` para todos os roots (+ `--tools` read-only para `ReadOnly`); (2)
`can_use_tool` do CLI → `Session::request_command_approval`/`request_patch_approval` do Codex, ou
seja, **a UI de aprovação do Codex decide pelo Claude**, e `permission_suggestions` vira
`updatedPermissions` para o CLI parar de perguntar a mesma coisa.

**Por que assim e não de outro jeito.** Ligar tudo em `bypassPermissions` resolveria o bloqueio
mas apagaria a única barreira de segurança do filho e ainda suprime `can_use_tool`. Um "MCP de
permissão" seria um processo a mais para o mesmo resultado que o stdio já dá.

**O que muda para o usuário.** Com `approval_policy = on-request`, um `Bash` do Claude abre a mesma
caixa de aprovação de um filho Luna; "negar" chega ao Claude como deny. Com `never` +
full-access nada muda de comportamento, só de transparência (Fase 5).

**Riscos / como saber que funcionou.** Risco: deadlock pai↔filho esperando aprovação (mitigação:
timeout no host → deny sem interromper; `Abort` quando o canal cai). Sucesso: tabela de 8
combinações testada; no Desktop, negar um comando do Claude aparece como recusado no transcript
dele.

---

## Fase 5 — paridade de atividade de tools

**Problema observado.** Sessões Claude gravam 26 linhas próprias na mediana (Luna: 173–320): não
existe registro de tool call. O pai vê "[Bash] cargo test" como reasoning e nunca sabe se passou.
Resultado: 13.942 `wait` expirados sem sinal, interrupções preventivas e "resultados" que são uma
frase de narração interrompida.

**Ideia central.** O CLI já emite `tool_use` e `tool_use_result` estruturados, casáveis por
`tool_use_id`. Traduzir cada par para o item de turno que o Codex já sabe renderizar
(`CommandExecution`, `FileChange` + `TurnDiff`, `McpToolCall`, `DynamicToolCall`) e gravar
`FunctionCall`/`FunctionCallOutput` no histórico — mas por uma variante nova,
`ResponseEvent::ProviderExecutedTool`, que o loop de turno **registra e exibe sem despachar**.
De quebra, a "última atividade" de cada agente passa a existir e aparece no timeout de
`wait_agent`.

**Por que assim e não de outro jeito.** Emitir `FunctionCall` comum faria o `ToolRouter` tentar
executar de novo um comando que o Claude já rodou. Um campo marcador em `FunctionCall` seria
ignorável por qualquer `match` existente; uma variante nova é aditiva e o compilador aponta todos
os sites (`match` exaustivos). Deixar como reasoning — o estado atual — é justamente o que cega o
pai.

**O que muda para o usuário.** No Desktop e na TUI, um filho Claude mostra células de comando com
saída, diffs por arquivo, chamadas MCP; `wait_agent` expirado diz "`/root/x` rodou `cargo test`
há 40 s"; `interrupt_agent` continua matando a árvore de processos.

**Riscos / como saber que funcionou.** Risco: formatos de `tool_use_result` por tool não são
documentados (mappers tolerantes: forma desconhecida vira `DynamicToolCall` com JSON bruto —
nunca perde a célula). Custo único: um replay por thread Claude vivo, porque os fingerprints do
histórico mudam. Sucesso: fixtures reais de stream-json → sequência de eventos esperada; teste de
que `FunctionCall` com namespace `claude_code` nunca chega ao router.

---

## Fase 6 — bridge MCP in-process

**Problema observado.** O filho Claude não tem **nenhuma** tool do Codex: não fala com o pai antes
de terminar, não usa os MCP da sessão (chrome, chatgpt, node_repl), não atualiza plano. Quando
quer reportar, apela ao `codex_app` do Desktop — origem dos cards de "outra tarefa".

**Ideia central.** Anunciar em `initialize` um servidor MCP chamado `codex` hospedado **pelo
próprio processo do Codex** (`sdkMcpServers`); o CLI encaminha `tools/list` e `tools/call` como
`mcp_message` pelo stdio. A lista de tools vem de `Prompt::tools` (o que o filho receberia se fosse
Luna, filtrando o que o Claude já tem) e a execução passa pelo `ToolRouter` real com
`ToolCallSource::DirectPlaintextMessage` — logo `send_message` do Claude é um
`InterAgentCommunication` normal, sem cifra, sem `plaintext_message`.

**Por que assim e não de outro jeito.** Um servidor MCP externo (`codex claude-bridge`, porta,
token) precisaria de autenticação, ciclo de vida e limpeza de órfãos; o stdio elimina os três.
Interceptar `send_message_to_thread` do `codex_app` e "converter" seria adivinhar intenção a partir
de um thread id do Desktop que o core não controla.

**O que muda para o usuário.** O filho Claude conversa com o pai e os irmãos pelos mesmos canais
dos Luna (aparece em `/agents`), usa os mesmos MCP, atualiza o plano — e não tem mais motivo para
tocar o `codex_app`.

**Riscos / como saber que funcionou.** Risco: fan-out do filho no pai (semáforo de 4). Sucesso:
`mcp__codex__send_message` do Claude chega ao pai como mensagem inter-agente; tool negada devolve
erro JSON-RPC sem despacho.

---

## Fase 7 — orquestração: tools do Desktop escopadas, espera informativa, prompts

**Problema observado.** (a) O plugin `codex_app` entrega `create_thread`/`send_message_to_thread`
a subagentes — daí 46 chats paralelos num incidente e 325 blocos `<codex_delegation>` num único
subagente. (b) `wait_agent` acorda com mail de **qualquer** agente e devolve três strings fixas;
`list_agents` não diz role, modelo, conta nem ociosidade. (c) Texto de hint configurado
*substitui* o bundled, então "basta configurar" apagaria orientação importante.

**Ideia central.** (a) `root_only_tools` por servidor MCP, decidido no único ponto que já conhece
a origem da sessão (`apply_mcp_tool_exposure_policy` + `is_non_root_agent()`, o mesmo predicado
que esconde `claude_accounts` de subagentes). (b) Um relógio de atividade por agente, alimentado
em `send_event`, exposto em `list_agents` e em `wait_agent` (que ganha `targets` para esperar
*aquele* agente). (c) Hints **aditivos** em código (`FORK_*_HINT_TEXT`) que sobrevivem a qualquer
config do usuário, mais sufixos configuráveis que nunca substituem.

**Por que assim e não de outro jeito.** `disabled_tools` (Fase 0) é global e tira da raiz o que é
dela. `tool_approval_overrides` fica opcional: só faz sentido se o usuário quiser
`send_message_to_thread` da raiz sem diálogo. O interceptador de mensagens foi descartado (ver
Fase 6). Defaults de espera maiores custam nada — o wait retorna assim que algo chega.

**O que muda para o usuário.** Subagente não vê tools de thread do Desktop; raiz recupera
`create_thread`; `list_agents` responde "quem está fazendo o quê há quanto tempo"; `wait_agent`
espera quem importa e explica por que voltou.

**Riscos / como saber que funcionou.** Risco: drift spec≠implementação após sync (guarda: teste
que valida o resultado real contra o `output_schema` da tool). Sucesso: de um subagente a tool não
existe; `wait_agent(targets:["explorer"])` não acorda com mail de outro agente.

---

## Fase 8 — contas Claude: correção e visibilidade

**Problema observado.** Escolha de conta, failover e cooldown são invisíveis (só `tracing::info`).
`claude_code_accounts.json`/`claude_code_sessions.json` usam temp `json.tmp.<pid>` — dez agentes
no mesmo processo compartilham o mesmo nome e `remove_file`→`rename` abre janela sem arquivo:
perda de escrita silenciosa. `classify_failure` é substring em inglês; `session_lost` casa
qualquer texto com `--resume` e descarta sessões saudáveis (replay integral, a operação mais cara
do provider). O status line mostra limites da conta OpenAI mesmo em turno Claude.

**Ideia central.** Primeiro corrigir (lock + temp único + rename com retry, classificação por
campos estruturados do frame `result`, `session_lost` estreito); depois mostrar
(`RateLimitSnapshot` por turno a partir de `get_usage`, conta em `list_agents`/`wait_agent`/
`/status`, `codex account claude list|use` para o humano).

**Por que assim e não de outro jeito.** O repositório já tem o primitivo de lock com retry
(`message-history`); reinventar seria pior. Fabricar snapshots zerados quando o usage é
desconhecido enganaria o status line — só emitir quando há dado.

**O que muda para o usuário.** `five-hour-limit`/`weekly-limit` corretos durante turno Claude;
`codex account claude list` mostra as contas, uso, cooldowns e a preferida; nenhum estado perdido
com 8 filhos simultâneos ou dois processos Codex.

**Riscos / como saber que funcionou.** Risco baixo. Sucesso: teste de 16 threads gravando chaves
distintas sem perda; "help text mentioning `--resume` does not discard the session".

---

## Fase 9 — anti-cerimônia no core

**Problema observado.** O maior custo de tempo do usuário não foi o Claude: foi o Codex inventando
auditorias, hashes, gates e certificações ("ta a 2 horas nisso", "12 horas já implementando e
não termina nunca"), perguntando o que o plano já responde, mexendo em git state e perdendo o
plano de passos após compactação (o `update_plan` não persiste nada; `build_compacted_history`
o descarta).

**Ideia central.** Transformar as ordens que o usuário repete em **defaults do harness**: o plano
sobrevive à compactação como fragmento explícito ("Current plan (carried across compaction…)"),
um hint de disciplina de entrega na raiz (plano é contrato; sem gates inventados; não perguntar o
que já está decidido), preservação de worktree no role `executor` built-in, e uma guarda macia
(aviso, nunca bloqueio) para criação excessiva de chats do Desktop.

**Por que assim e não de outro jeito.** AGENTS.md é texto que compete com tudo o mais e some em
compactação; um fragmento estruturado e um hint em código não somem. Bloquear `create_thread` na
raiz tiraria uma capacidade legítima; avisar corrige o loop sem custar ao humano.

**O que muda para o usuário.** Depois de `/compact` o modelo continua em "passo 4/6"; menos
perguntas redundantes; menos cerimônia; menos prompts colados.

**Riscos / como saber que funcionou.** Risco: prompt demais engessa (chaves `delivery_discipline_hint`
e `update_plan_survives_compaction` para desligar). Sucesso: sessão longa com plano de 6 passos,
`/compact`, o rollout pós-compactação contém o fragmento do plano e o modelo não renumera.

---

## Fase 10 — opcionais

- **`multi_agent_version: "v2"` para os presets Claude** — hoje o filho Claude recebe o namespace
  v1 de colaboração; só faz sentido quando a bridge (Fase 6) existir, senão é tool que ele não
  alcança.
- **Modelos Claude visíveis no picker** — um flip de `visibility` para poder escolher Claude como
  modelo do thread raiz. Fora do pedido atual ("usado *pela* thread principal"), mas trivial.
- **Streaming parcial (`--include-partial-messages`)** — texto/thinking em deltas, para o filho
  parecer vivo entre células. Conforto, não correção.

---

## Como as fases se encaixam

```
Fase 0  (config/AGENTS.md/roles)      independente — baseline, entra hoje
Fase 1  (spawn robusto)               independente — remove falhas mais frequentes
Fase 2  (contexto limpo)              independente — prepara appendSystemPrompt e fork_turns seguro
Fase 3  (transporte de controle) ─┬─► Fase 4 (aprovações/sandbox) ─┬─► Fase 6 (bridge MCP)
                                  ├─► Fase 5 (tool activity) ──────┘
                                  └─► Fase 8.4 (RateLimitSnapshot via get_usage)
Fase 7  (orquestração)                7.1/7.3 independentes; 7.4–7.6 aproveitam a atividade da Fase 5
Fase 8  (contas)                      8.1–8.3 independentes; 8.4 depende da 3; 8.5/8.6 cosméticos
Fase 9  (anti-cerimônia)              independente; 9.4 depende de 7.1 para a metade "subagente"
Fase 10 (opcionais)                   após 5/6
```

Por que a ordem recomendada (0 → 1 → 2 → 3 → 5 → 4 → 6 → 7 → 8.1 → 8.4/8.5 → 9.1 → 9.2–9.4 →
8.2/8.3 → 10):

1. **Primeiro o que dói hoje e não tem risco** (0–2): cards e diálogos somem, spawns param de
   falhar, o filho recebe só a tarefa. Um dia de trabalho, medível na hora.
2. **Depois a fundação e o payload de visibilidade** (3 → 5): é o que ataca a métrica central
   (46% de aborts em opus) — o pai passa a *ver* o filho. A Fase 5 vem antes da 4 porque é a de
   maior alavancagem e não depende de aprovações.
3. **Então paridade de comportamento** (4 → 6): aprovações e tools compartilhadas. Só aqui o
   "Claude igual a um agente nativo" fica completo.
4. **Por fim o ganho do core apontado pelas sessões** (7–9), que vale mesmo sem Claude: espera
   informativa, tools do Desktop escopadas, contas visíveis e corretas, plano que sobrevive,
   menos cerimônia.

Cada fase é um PR/commit isolado no fork, com `// FORK:` nos pontos de toque em arquivos upstream
(sete funções ao todo; o resto vive em `core/src/claude_code/`), para que os syncs semanais com
`openai/codex` continuem baratos.

---

## Glossário

- **`claude_code`** — provider local do fork que executa `claude.exe`; **filho Claude** = subagente
  com esse provider (roles `claude-opus`/`claude-sonnet`).
- **Protocolo de controle** — mensagens `control_request`/`control_response` trocadas pelo stdio
  do CLI (`initialize`, `can_use_tool`, `mcp_message`, `get_usage`, `end_session`).
- **`content_item_kinds`** — etiquetas que o Codex anexa a cada fragmento injetado no histórico
  (ex. `agents_md.instructions`); base do filtro da Fase 2.
- **`ProviderExecutedTool`** — variante fork-only de `ResponseEvent` para tool calls já executadas
  pelo provider (gravar e exibir, nunca despachar).
- **`codex_app`** — servidor MCP do plugin bundled do Codex Desktop (`create_thread`,
  `send_message_to_thread`, …); origem dos cards "Enviado por ChatGPT de outra tarefa".
- **`root_only_tools`** — chave nova por servidor MCP: tools que só o thread raiz enxerga.
