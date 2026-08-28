Tens razão em pedir isto — e a resposta honesta começa por uma meta-decisão: eu classifiquei estas escolhas como «preferências com default razoável» e documentei-as como assunções (tabela D1–D8 do plano) em vez de perguntar, precisamente o padrão que a tua análise de hoje critica no Codex («proceed with the recommended option and record it as an assumption»). Devia ter perguntado as de preferência antes de escrever o plano. Lista completa:

**Decisões de preferência — sim, deviam ter sido perguntas**

| # | O que assumi | (a) Alternativa descartada | (b) Devia perguntar? |
|---|---|---|---|
| D2 | Um ficheiro por **thread**; revisões reescrevem o ficheiro (`revision++`), corpo idêntico não escreve | Um ficheiro por bloco `<proposed_plan>` (histórico completo no disco, lista com repetidos); ou por thread + revisões antigas arquivadas | **Sim** — muda o que aparece em `/plans` e o que se perde |
| D5 | App-server só `plan/list` + `plan/read`; o Desktop compõe o `turn/start` com um template documentado | `plan/load` server-side (lê o plano, monta o prompt, arranca o turno em Default) — menos código no UI injetado no Desktop, mas duplica `turn/start` | **Sim** — depende de como vais integrar no Desktop, que eu não sei |
| D7 | Plano injetado como contexto **oculto** (delimitador `## My request for Codex:`, como o IDE context); o transcript mostra só o pedido | Mensagem do user visível com o plano inteiro (o que o popup «clear context and implement» faz hoje) | **Sim** — preferência de UX no transcript |
| S5/S6 | `/plans` → 2 ações: «Implement this plan» (Default mode + «Implement the plan.») e «Attach to my next message» | Só «Implement»; ou uma 3.ª ação «Revise in Plan mode» | **Sim** — alarguei o pedido («carregar como contexto para implementação») por conta própria |
| D8 | Gravação sempre ligada, sem flag nem opção de config | Opção `[plans] enabled` no `config.toml` (toca `ConfigToml` + schema) ou `Feature::Plans` (toca `features/src/lib.rs`, conflitos de sync) | **Sim, pergunta rápida** — é um opt-out que só tu sabes se queres |

**Decisões técnicas/cosméticas — não perguntaria, mas diz se discordas**

| # | O que assumi | (a) Alternativa descartada | (b) |
|---|---|---|---|
| D1 | Hook no core (`maybe_complete_plan_item_from_message`) | Extension API: nunca vê `TurnItem::Plan`; registar um `TurnItemContributor` altera o streaming de todas as sessões | Não — é o ponto que a tua análise já indicava |
| D3 | YAML front matter dentro do `.md` | Sidecar `.json` / índice | Não — mas é visível quando abres o ficheiro; troco por JSON se preferires |
| D4 | `saved_path` no `PlanItem` (core + v2 `savedPath`) | Novo `EventMsg::PlanSaved` (≈10 `match` exaustivos em 8 crates) | Não |
| D6 | TUI lê via RPC | Ler `~/.codex/plans` no TUI | Não — o TUI já não depende de `codex-core` |
| S1 | Nome `YYYY-MM-DDTHH-MM-SS-<slug>.md` (padrão das notas de memories/rollouts) | `<slug>.md` só, `<thread_id>.md`, slug aleatório à Claude | Não, cosmético |
| S2 | Título = 1.º heading `#` → 1.ª linha → «Plan <data>»; metadados `title, thread_id, turn_id, cwd, model, created_at, updated_at, revision` | Menos/mais campos | Não |
| S4 | Grava em **todas** as sessões (TUI, Desktop, exec, sub-agentes) | Só sessões interativas | Não — mas nota: sub-agentes em Plan mode também geram ficheiros |
| S7 | Picker com tabs «This project»/«All», default «This project» | Lista única com o projeto na descrição | Não |
| S8 | Sem linha «Plan saved» em replay/resume | Mostrar sempre | Não |
| S9 | Métodos **não** experimentais (o Desktop pode não ligar `experimentalApi`) | `#[experimental]` como os métodos novos upstream | Não — mas é um palpite sobre o Desktop |
| S10/S11 | Sem row «Cancel» (Esc volta ao picker); sem cap de tamanho na injeção | — | Não |

Vou perguntar agora as cinco de preferência; o plano é atualizado com as respostas antes de te pedir aprovação.