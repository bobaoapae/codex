# Persistência e reutilização de planos do Plan mode

## Resumo

- Guardar cada `<proposed_plan>` final como um ficheiro distinto em `$CODEX_HOME/plans/` — por omissão `~/.codex/plans/`.
- Centralizar armazenamento e carregamento no `codex-home`; o core grava e TUI/Desktop consomem pelas APIs v2 do app-server.
- O ficheiro conterá apenas o Markdown interno do bloco, sem tags ou front matter.
- O `/plans` carregará o plano na sessão e abrirá o fluxo existente “Implement this plan?”.
- O plano será contexto normal: anexado uma vez ao histórico, sem reinjeção especial após compactação.

## Contratos e interfaces

- Nome dos ficheiros: `YYYYMMDDTHHMMSSZ-<uuid-v7>.md`. Cada revisão ou repetição gera um novo ficheiro.
- `PlanStore` fornecerá `save`, `list` paginado e `load`, com tipos `PlanId`, `SavedPlanSummary`, `SavedPlan` e `PlanPage`.
- A listagem será decrescente por ID/data, com página padrão de 50 e máximo de 100. O título virá do primeiro H1, depois da primeira linha não vazia, limitado a 120 caracteres; fallback: `Untitled plan`.
- Novas APIs v2 experimentais:
  - `plan/list { cursor?, limit? } -> { data: [{ id, title, createdAt }], nextCursor }`
  - `plan/load { threadId, planId } -> { plan: { id, title, createdAt, markdown } }`
- `plan/load` apenas injeta contexto; não muda o modo nem inicia turno. O Desktop inicia depois `turn/start` em Default mode.
- O contexto será um `ContextualUserFragment` separado, com `contentKind = "plan.loaded"` e marcadores `<saved_plan>`. O Markdown mais o wrapper não poderá exceder 10.000 tokens estimados; planos maiores continuam guardados, mas o carregamento é rejeitado sem truncar.

## Implementação

1. **Armazenamento e captura final**
   - Criar o módulo de planos em `codex-home`, com escrita atómica no mesmo diretório, permissões locais restritas quando suportadas e IDs que nunca aceitam caminhos fornecidos pelo cliente.
   - Listar apenas ficheiros regulares, não-symlink, com nomes válidos; ler somente um prefixo limitado para extrair títulos e manter respostas paginadas.
   - No core, ligar a gravação ao caminho autoritativo `ProposedPlanItemState::complete_with_text`, depois de `extract_proposed_plan_text` e `strip_citations`, mas antes de emitir `ItemCompleted`. Isso evita duplicação em replay e garante que o plano já existe quando clientes recebem a conclusão.
   - Falhas de disco emitem `WarningEvent`, mas não invalidam o turno nem o `PlanItem`.
   - **Validação:** `just bazel-lock-update`, `just test -p codex-home` e `just test -p codex-core`.

2. **Carregamento seguro e app-server**
   - Adicionar `SavedPlanContext` em `core/context`, registá-lo como fragmento contextual oculto e expor `CodexThread::inject_saved_plan(SavedPlan)`, reutilizando `inject_response_items` para acrescentar e persistir sem criar turno ou reescrever histórico.
   - Escapar somente ocorrências dos próprios marcadores na cópia injetada; o `.md` permanece intacto. Rejeitar conteúdo acima do limite, em vez de carregar um plano incompleto.
   - Implementar um processador app-server dedicado. `plan/list` usa leitura global partilhada; `plan/load` é serializado por `threadId`, aplica `ensure_direct_input_allowed` e rejeita threads com turno ativo.
   - Mapear ID/cursor malformado para `invalid_params`, plano inexistente ou thread ocupada para `invalid_request`, e falhas reais de armazenamento para erro interno.
   - Atualizar `app-server/README.md`, tipos TypeScript e schemas JSON estáveis/experimentais.
   - **Validação:** executar `python codex-rs/app-server-protocol/scripts/write_schema_fixtures.py` e novamente com `--experimental`, depois `just test -p codex-app-server-protocol` e `just test -p codex-app-server`. Usar o script porque a receita `just write-app-server-schema` atual aponta para um binário inexistente; corrigir essa infraestrutura fica fora deste escopo.

3. **TUI `/plans`**
   - Adicionar `SlashCommand::Plans` como comando disponível apenas na sessão principal ociosa, não durante tarefas nem side conversations.
   - Fazer todas as chamadas pelo `AppServerSession`; o TUI nunca resolve ou lê `$CODEX_HOME` diretamente, preservando app-servers remotos.
   - Mostrar `ListSelectionView` pesquisável com estados loading, vazio, erro/retry e página carregada. Exibir título e data; quando houver `nextCursor`, acrescentar “Load older plans…” para buscar e anexar a página seguinte.
   - Ao selecionar, chamar `plan/load`, mostrar confirmação e abrir o popup de implementação existente. “Implement” envia `Implement the plan.` em Default mode; “clear context” usa o Markdown devolvido para criar a sessão limpa; cancelar mantém o plano carregado sem iniciar turno.
   - Generalizar o texto de cancelamento do popup para “Not now / Keep the current mode”, válido tanto em Plan como em Default mode.
   - **Validação:** `just test -p codex-tui`, rever `cargo insta pending-snapshots -p codex-tui` e aceitar apenas os snapshots intencionais.

## Casos de teste e conclusão

- Dois blocos finais, mesmo idênticos, produzem dois `.md`; respostas sem bloco, fora de Plan mode ou vazias não produzem ficheiro.
- O conteúdo gravado é igual ao `PlanItem.text`, sem tags/front matter e com citações removidas.
- Falha determinística do diretório produz warning e mantém o plano concluído.
- Paginação mantém ordenação estável; IDs com `..`, separadores, caminhos absolutos, symlinks, UTF-8 inválido e conteúdo excessivo são rejeitados.
- Após `plan/load`, a próxima requisição ao modelo contém exatamente um fragmento `plan.loaded`, que não aparece como mensagem normal no transcript.
- Cobrir app-server vazio/list/load, thread inexistente ou ocupada, subagente sem input direto e gate experimental.
- Cobrir snapshots TUI de loading, vazio, lista paginada, erro e popup pós-carregamento.
- Depois dos testes focados, pedir autorização para o único gate amplo `just test`; em seguida executar `just fix -p` nos crates alterados e `just fmt`, sem repetir testes depois de `fix`/`fmt`.

## Assunções

- Não haverá delete, rename, retenção automática, API v1, configuração nova ou `plan/read` nesta entrega.
- Todos os planos finais são persistidos, independentemente do cliente ou origem da thread.
- Compactação segue o comportamento normal do histórico; não será criado estado “plano ativo”.
- Os ficheiros não rastreados em `docs/plans/2026-08-27-plan-mode/` serão preservados sem alterações.
- Como o fragmento carregado pode ultrapassar 1.000 tokens, a mudança de contexto é P0 e recebe uma única revisão manual focada em limites, precedência, marcadores e comportamento de histórico.
