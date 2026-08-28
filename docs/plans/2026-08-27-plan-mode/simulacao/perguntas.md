# Perguntas feitas ao utilizador na simulação (respondidas automaticamente com a 1.ª opção)


## Codex gpt-5.6-sol @ ultra — 8 perguntas


**Ronda 1 · t=864.4s · [Ao carregar]** Depois de selecionar um plano no `/plans`, o que o TUI deve fazer?

- Perguntar implementação (Recommended) — Carrega o plano e reutiliza o popup existente para implementar nesta sessão, numa sessão limpa, ou cancelar.
- Só carregar — Injeta o plano e mostra confirmação; o utilizador inicia a implementação com a próxima mensagem.
- Implementar já — Carrega, muda para Default mode e inicia imediatamente o turno de implementação.
- → escolhido: **Perguntar implementação (Recommended)**

**Ronda 1 · t=864.4s · [Duração]** O plano carregado deve sobreviver a compactação e retoma da sessão?

- Contexto normal (Recommended) — É anexado uma vez ao histórico e segue as regras normais de compactação, sem estado permanente especial.
- Até ser trocado — O core reinjeta o plano após compactação e retoma até outro plano ser carregado.
- → escolhido: **Contexto normal (Recommended)**

**Ronda 1 · t=864.4s · [Ficheiro]** O que deve ficar dentro de cada `.md` em `~/.codex/plans/`?

- Markdown puro (Recommended) — Guarda apenas o conteúdo interno do bloco, sem tags nem front matter; timestamp e ID ficam no nome.
- Com front matter — Acrescenta metadados YAML de origem antes do Markdown para rastreabilidade.
- Bloco completo — Preserva literalmente as tags `<proposed_plan>` e o conteúdo entre elas.
- → escolhido: **Markdown puro (Recommended)**

**Ronda 2 · t=1400.1s · [Cobertura]** Que planos finais devem ser persistidos automaticamente?

- Todas as threads (Recommended) — Inclui sessões principais, subagentes e threads efémeras sempre que produzirem um plano final.
- Não efémeras — Exclui side conversations e outras threads efémeras, mas mantém threads persistentes e subagentes.
- Só principais — Guarda apenas planos de threads raiz iniciadas diretamente pelo utilizador.
- → escolhido: **Todas as threads (Recommended)**

**Ronda 2 · t=1400.1s · [API v2]** Como devem nascer `plan/list` e `plan/load` no app-server v2?

- Experimental (Recommended) — Exige `experimentalApi`, permitindo ajustar o contrato antes de o estabilizar.
- Estável — Fica disponível sem opt-in e passa imediatamente a integrar a superfície compatível.
- → escolhido: **Experimental (Recommended)**

**Ronda 2 · t=1400.1s · [Transcript]** Como o plano carregado deve aparecer na sessão?

- Contexto oculto (Recommended) — É model-visible e persistido no rollout, mas não aparece como mensagem normal do utilizador.
- Mensagem visível — Aparece no transcript como mensagem de utilizador antes da implementação.
- Só próximo turno — É contexto efémero apenas para o próximo `turn/start`, sem permanecer no histórico.
- → escolhido: **Contexto oculto (Recommended)**

**Ronda 2 · t=1434.8s · [Arquivo]** Como gerir o crescimento de `~/.codex/plans/` nesta primeira versão?

- Guardar indefinidamente (Recommended) — Sem retenção ou delete na UI/API; o utilizador ainda pode remover ficheiros manualmente.
- Adicionar delete — Inclui `plan/delete` e uma ação de remoção no `/plans` já nesta entrega.
- Reter últimos 500 — Apaga automaticamente os planos mais antigos ao ultrapassar 500 ficheiros válidos.
- → escolhido: **Guardar indefinidamente (Recommended)**

**Ronda 2 · t=1434.8s · [Plano grande]** O que fazer quando o plano guardado excede 10.000 tokens estimados ao carregar?

- Rejeitar carga (Recommended) — Preserva o ficheiro completo e explica o limite, evitando implementar um plano incompleto.
- Truncar com aviso — Carrega apenas até ao limite com um marcador explícito de conteúdo omitido.
- Dividir em fragmentos — Injeta vários fragmentos limitados, com maior custo de contexto e complexidade.
- → escolhido: **Rejeitar carga (Recommended)**

## Claude Code fable-5 @ max — 4 perguntas


**Ronda 2 · t=3037.4s · [Ficheiros]** Como organizar os ficheiros em ~/.codex/plans/ quando o mesmo thread produz várias revisões do plano?

- Um por thread (Recommended) — Revisões reescrevem o mesmo ficheiro (revision++, updated_at); corpo idêntico não escreve. Lista limpa; histórico de revisões fica só no rollout. É o modelo do plan file do Claude.
- Um por bloco <proposed_plan> — Cada plano final gera um ficheiro novo (revisões idênticas consecutivas são ignoradas). Nada se perde no disco; /plans mostra várias entradas do mesmo thread.
- Por thread + revisões arquivadas — O ficheiro principal é reescrito, e a versão anterior é copiada para plans/history/<stem>.r<N>.md. Mais código no crate; /plans só lista os principais.
- → escolhido: **Um por thread (Recommended)**

**Ronda 2 · t=3037.4s · [Desktop API]** Que superfície queres no app-server para o Desktop?

- plan/list + plan/read (Recommended) — O cliente lê o plano e compõe o turn/start com o template de handoff documentado no README (2 chamadas). Sem duplicar a lógica de turn/start.
- Também plan/load — Método extra { threadId, planId, request? } que lê o plano, monta o mesmo prompt que o TUI e arranca o turno em Default mode. 1 chamada do UI injetado no Desktop; ~150 linhas + teste a mais.
- Só plan/list + plan/read, com handoffPrompt — plan/read devolve também o campo handoffPrompt já montado (prefixo + plano + delimitador), para o Desktop só concatenar o pedido.
- → escolhido: **plan/list + plan/read (Recommended)**

**Ronda 2 · t=3037.4s · [UX do /plans]** Como deve o /plans carregar o plano na sessão atual?

- Oculto + Implement/Attach (Recommended) — Plano injetado como contexto oculto (delimitador do IDE context; transcript/Desktop/export mostram só o pedido). Duas ações: «Implement this plan» (Default mode + «Implement the plan.») e «Attach to my next message».
- Oculto, só Implement — Uma única ação: selecionar o plano arranca logo a implementação em Default mode. Menos estado no widget (sem contexto pendente).
- Plano visível no transcript — Envia prefixo + plano como mensagem do user visível (igual ao «clear context and implement» atual). Sem mecanismo de ocultação; transcript mostra o plano inteiro.
- Oculto + Implement/Attach/Revise — As duas ações acima mais «Revise in Plan mode»: mantém Plan mode e envia o plano com um pedido de revisão (o modelo re-emite <proposed_plan>, que reescreve o ficheiro).
- → escolhido: **Oculto + Implement/Attach (Recommended)**

**Ronda 2 · t=3037.4s · [Opt-out]** Queres uma forma de desligar a gravação de planos?

- Sempre ligado (Recommended) — Sem flag nem config; grava como os rollouts. Zero toques em features/ ou ConfigToml (menos conflitos de sync).
- Opção no config.toml — [plans] enabled = true|false (default true). Toca ConfigToml + `just write-config-schema`; lido no hook do core.
- Feature flag — Feature::Plans em features/src/lib.rs (default on), controlável por `features.plans` e /experimental. Ficheiro com conflitos recorrentes nos syncs.
- → escolhido: **Sempre ligado (Recommended)**