# Persistência e reutilização de planos do Plan mode

## Resumo

- Guardar cada `<proposed_plan>` final como ficheiro distinto em `$CODEX_HOME/plans/`, por omissão `~/.codex/plans/`.
- Persistir planos produzidos por todas as threads, incluindo subagentes e threads efémeras.
- Centralizar o armazenamento em `codex-home`; o core grava e TUI/Desktop consomem pelas APIs v2 experimentais do app-server.
- O `/plans` carrega o plano como contexto oculto e abre o fluxo existente “Implement this plan?”.
- O contexto é anexado uma vez e segue as regras normais de histórico e compactação.

## Contratos

- Ficheiro: `YYYYMMDDTHHMMSSZ-<uuid-v7>.md`, contendo somente o Markdown interno do bloco, sem tags, front matter ou normalização adicional.
- Cada ocorrência gera novo ficheiro; não há deduplicação, overwrite, retenção ou delete automático.
- `PlanStore` expõe `save`, `list` e `load`, com `PlanId`, `SavedPlanSummary`, `SavedPlan` e `PlanPage`.
- A lista é decrescente, cursor-based, com página padrão de 50 e máximo de 100. O título vem do primeiro H1 ou primeira linha não vazia, limitado a 120 caracteres; fallback `Untitled plan`.
- APIs v2 experimentais:
  - `plan/list { cursor?, limit? } -> { data: [{ id, title, createdAt }], nextCursor }`
  - `plan/load { threadId, planId } -> { plan: { id, title, createdAt, markdown } }`
- `plan/load` apenas injeta contexto. Desktop inicia a implementação posteriormente com `turn/start` em Default mode.
- Planos carregados não podem exceder 10.000 tokens estimados incluindo o wrapper; o excesso é rejeitado sem truncar, mantendo o ficheiro integral.

## Implementação

1. **Store e captura final**
   - Criar `codex-home::plans`, usando sempre o `codex_home` configurado.
   - Escrever por temporário + rename no mesmo diretório, com permissões owner-only onde suportadas e nova UUID em caso de colisão.
   - Listar apenas ficheiros regulares, não-symlink, com nome válido; ignorar temporários e `.md` externos ao formato gerado.
   - Aplicar o cursor antes de ler títulos e manter em memória apenas a página solicitada mais o elemento necessário para calcular `nextCursor`.
   - Integrar a gravação em `ProposedPlanItemState::complete_with_text`, depois da extração final e remoção de citações, mas antes de `ItemCompleted`.
   - Não gravar planos vazios. Em falha de IO, emitir `WarningEvent` e continuar a conclusão normal do plano.
   - **Validação:** `just bazel-lock-update`, `just test -p codex-home` e `just test -p codex-core`.

2. **Contexto seguro e app-server**
   - Adicionar `SavedPlanContext` em `core/context`, com role `user`, mensagem separada, `contentKind = "plan.loaded"` e marcadores `<saved_plan>`.
   - O wrapper explicará que o utilizador selecionou um plano anteriormente gerado como escopo de implementação e que instruções system/developer e mensagens posteriores mantêm precedência.
   - Escapar somente ocorrências literais dos marcadores na cópia injetada; nunca modificar o `.md`.
   - Registar o matcher contextual para impedir que o fragmento seja projetado como mensagem normal.
   - Expor `CodexThread::inject_saved_plan(SavedPlan)`, validando o limite e reutilizando `inject_response_items` para persistência append-only, sem novo turno.
   - Criar tipos v2 em módulo próprio e um processador app-server dedicado. `plan/list` usa leitura global partilhada; `plan/load` é serializado por thread.
   - `plan/load` aplica `ensure_direct_input_allowed`, rejeita turno ativo, resolve apenas `PlanId` validado e devolve o Markdown já limitado.
   - IDs/cursors malformados retornam `invalid_params`; plano inexistente ou thread ocupada retornam `invalid_request`; falhas de storage retornam erro interno.
   - Documentar opt-in experimental e o fluxo `plan/load` seguido de `turn/start` no `app-server/README.md`.
   - Regenerar schemas TypeScript/JSON estáveis e experimentais.
   - **Validação:** executar `python codex-rs/app-server-protocol/scripts/write_schema_fixtures.py` e novamente com `--experimental`, seguido de `just test -p codex-app-server-protocol` e `just test -p codex-app-server`. Não corrigir a receita `just write-app-server-schema` quebrada nesta entrega.

3. **TUI `/plans`**
   - Adicionar `SlashCommand::Plans`, sem argumentos inline, disponível apenas na thread principal quando não há turno ativo.
   - Encaminhar listagem e carregamento pelo `AppServerSession`; nunca ler diretamente o filesystem local.
   - Mostrar `ListSelectionView` pesquisável com estados loading, vazio, erro/retry e lista paginada.
   - Exibir título e data local. Se houver `nextCursor`, acrescentar “Load older plans…” e anexar a página recebida.
   - Ao selecionar um plano, chamar `plan/load`, mostrar uma confirmação curta e abrir o popup de implementação existente.
   - A opção corrente envia `Implement the plan.` com colaboração Default; a opção de contexto limpo usa o Markdown devolvido e o prefixo já existente; cancelar deixa o fragmento carregado sem iniciar turno.
   - Generalizar a opção de cancelamento para “Not now” / “Keep the current mode”, válida tanto em Plan como em Default mode.
   - **Validação:** `just test -p codex-tui`, rever `cargo insta pending-snapshots -p codex-tui` e aceitar somente snapshots intencionais.

## Testes e conclusão

- Um plano final cria exatamente um `.md` cujo corpo corresponde integralmente ao `PlanItem.text`; dois planos iguais criam dois ficheiros.
- Respostas vazias, sem bloco ou fora de Plan mode não criam ficheiros; replay do transcript também não cria duplicados.
- Falha determinística do diretório emite warning e mantém o `PlanItem` concluído.
- Testar ordenação, cursores, diretório inexistente, UTF-8 inválido, symlinks, nomes externos, traversal e colisão de IDs.
- Testar que um plano acima de 10.000 tokens é listado e preservado, mas `plan/load` falha sem injetar conteúdo parcial.
- Após `plan/load`, a próxima requisição ao modelo contém exatamente um fragmento `plan.loaded`, persistido no rollout e ausente da projeção visível.
- Cobrir app-server vazio/list/load, gate experimental, thread inexistente ou ocupada e subagente sem input direto.
- Cobrir snapshots TUI de loading, vazio, lista, paginação, erro e popup pós-carregamento.
- Depois dos testes focados, pedir autorização para o único gate amplo `just test`; depois executar `just fix -p` nos crates alterados e `just fmt`, sem repetir testes após `fix`/`fmt`.

## Limites da entrega

- Sem API v1, `plan/read`, `plan/delete`, importação de Markdown, configuração de retenção ou preview dedicado.
- Os planos permanecem no arquivo até remoção manual.
- Não existe estado permanente de “plano ativo” nem reinjeção especial após compactação.
- Os métodos app-server exigem `experimentalApi`.
- Os documentos não rastreados em `docs/plans/2026-08-27-plan-mode/` permanecem intactos.
- Por poder exceder 1.000 tokens, `SavedPlanContext` recebe uma única revisão manual P0 focada em limites, precedência, marcadores e histórico.
