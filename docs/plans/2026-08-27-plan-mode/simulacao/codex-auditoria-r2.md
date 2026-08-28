Não conto como assunções as três decisões que já te tinha perguntado antes do plano anterior:

- `/plans` carrega e abre o popup de implementação.
- O contexto segue compactação normal, sem reinjeção especial.
- O ficheiro contém Markdown puro.

A auditoria abaixo cobre todas as escolhas restantes do plano anterior. As cinco que considero que deviam ter sido perguntas foram entretanto resolvidas contigo.

### Arquitetura e armazenamento

| Decisão inicialmente assumida | Alternativa descartada | Devia ter sido pergunta? |
|---|---|---|
| Resolver o diretório por `$CODEX_HOME/plans`, em vez de fixar literalmente `~/.codex/plans` | Ignorar `CODEX_HOME` customizado | Não; é uma convenção estrutural do Codex. |
| Implementar o store no crate existente `codex-home` | Criar `codex-plan-store` ou colocar IO no `codex-core` | Não; é uma decisão interna e evita aumentar o core. |
| Persistir no core, no encerramento autoritativo do `PlanItem` | Persistir no TUI, app-server ou durante replay | Não; os outros pontos perderiam clientes ou duplicariam ficheiros. |
| Concluir a escrita antes de emitir `ItemCompleted` | Gravar depois da notificação | Não; a ordem evita uma corrida imediata com `plan/list`. |
| Falha de gravação é não fatal e produz warning | Falhar o turno ou ignorar silenciosamente | Não; não se deve perder uma resposta válida por falha do arquivo auxiliar. |
| Persistir planos de todas as threads | Só threads principais ou apenas não efémeras | **Sim. Resolvida agora:** todas as threads. |
| Diretório plano, sem partições por data | Sharding diário/mensal ou SQLite | Não; o volume conhecido é baixo e os ficheiros devem continuar navegáveis. |
| Um ficheiro por bloco final, mesmo para revisões idênticas | Sobrescrever o último plano da thread ou deduplicar | Não; “cada plano final” e “planos anteriores” implicam histórico por ocorrência. |
| Nome `timestamp UTC + UUID v7` | Slug do título, thread ID, turn ID ou UUID isolado | Não; é implementação sem efeito semântico. |
| Reconhecer na listagem apenas nomes gerados pelo Codex | Importar qualquer `.md` colocado manualmente no diretório | Não; importação não faz parte do pedido. |
| Derivar o título do H1 ou primeira linha e limitá-lo a 120 caracteres | Guardá-lo em front matter ou mostrar apenas o ID | Não; decorre da escolha por Markdown sem metadados. |
| Ordenar do mais recente para o mais antigo | Ordem alfabética crescente ou por título | Não; é o comportamento normal de um arquivo histórico. |
| Paginação cursor-based, padrão 50 e máximo 100 | Carregar tudo ou usar offsets instáveis | Não; é a convenção obrigatória para novas APIs de lista. |
| Escrita atómica e permissões restritas | Escrever diretamente no destino com permissões padrão | Não; é uma salvaguarda de integridade e privacidade. |
| Arquivo sem retenção ou delete automático | `plan/delete` ou retenção fixa | **Sim. Resolvida agora:** guardar indefinidamente; remoção manual continua possível. |
| Persistência sempre ativa, sem configuração | Flag `[plans] enabled` | Não; o pedido diz que cada plano deve ser guardado. |
| Guardar exatamente o texto final já processado pelo `PlanItem` | Normalizar Markdown ou guardar o output bruto com citações | Não; reutilizar a representação autoritativa evita divergências. |
| Não guardar thread, cwd ou branch no ficheiro | Front matter de proveniência | Não; a opção “Markdown puro” resolveu esta direção. |

### Contexto e app-server

| Decisão inicialmente assumida | Alternativa descartada | Devia ter sido pergunta? |
|---|---|---|
| Criar um `ContextualUserFragment` específico | Montar `ResponseItem` bruto ou reutilizar um fragmento genérico | Não; o repositório exige um tipo auditável em `core/context`. |
| Contexto oculto no transcript | Mensagem de utilizador visível ou contexto efémero | **Sim. Resolvida agora:** contexto oculto, persistido no rollout. |
| Fragmento com role `user`, mensagem separada, `plan.loaded` e `<saved_plan>` | Role developer, `additionalContext` por turno ou mensagem normal | Não; preserva a hierarquia correta e evita criar uma fronteira visível de turno. |
| Escapar somente os próprios marcadores na cópia injetada | Alterar o Markdown gravado, codificar todo o conteúdo ou aceitar quebra do wrapper | Não; é uma proteção interna sem modificar o artefacto. |
| Limite de 10.000 tokens estimados por plano carregado | Sem limite, truncamento ou múltiplos fragmentos | **Sim. Resolvida agora:** rejeitar o carregamento sem truncar. |
| Planos acima do limite continuam guardados | Recusar ou truncar também a persistência | Não; “cada plano final” continua satisfeito e o ficheiro completo é preservado. |
| Endpoints `plan/list` e `plan/load` | `plan/read`, `thread/plan/load` ou filesystem genérico | Não; refletem diretamente as duas capacidades pedidas. |
| Métodos v2 experimentais | Superfície estável imediata | **Sim. Resolvida agora:** experimentais, com opt-in `experimentalApi`. |
| `plan/list` devolve somente ID, título e criação | Incluir Markdown, cwd, thread, tamanho ou preview | Não; mantém a lista bounded e separa metadados de carregamento. |
| `plan/load` devolve também o Markdown bounded | Devolver só resumo ou exigir um terceiro `plan/read` | Não; o TUI precisa do texto para a opção de sessão limpa. |
| `plan/load` injeta contexto, mas não inicia implementação | Alterar modo e executar automaticamente | Não; “load” e `turn/start` devem continuar operações separadas. |
| Rejeitar carregamento durante turno ativo | Steer do turno corrente ou fila automática | Não; é a opção determinística e segura. |
| Respeitar `ensure_direct_input_allowed` | Permitir bypass em subagentes parent-owned | Não; é uma regra de ownership já existente. |
| `plan/list` usa leitura global partilhada e `plan/load` serialização por thread | Sem serialização ou lock global exclusivo | Não; acompanha a natureza de cada operação. |
| IDs/cursors inválidos são `invalid_params`; missing/busy são `invalid_request` | Converter tudo em erro interno | Não; é mapeamento normal de erros do app-server. |
| Sem API v1, `plan/read`, `plan/delete` ou preview dedicado | Expandir a superfície já nesta entrega | Não; estão fora do requisito list/load. |
| Desktop chama `turn/start` depois de `plan/load` | Prompt de implementação controlado pelo servidor | Não; o cliente deve controlar quando começa a execução. |

### TUI e validação

| Decisão inicialmente assumida | Alternativa descartada | Devia ter sido pergunta? |
|---|---|---|
| TUI usa exclusivamente o app-server | Ler `$CODEX_HOME` diretamente | Não; acesso direto quebra app-servers remotos. |
| `/plans` apenas em thread principal ociosa | Permitir durante turnos ou side conversations | Não; acompanha a rejeição de carga concorrente. |
| Usar `ListSelectionView` pesquisável | Criar um picker full-screen semelhante a `/resume` | Não; é menor e reutiliza o padrão existente. |
| Paginar com “Load older plans…” | Carregar todo o arquivo inicialmente | Não; evita estado ilimitado no TUI. |
| Sem `/plans <id>` inline | Oferecer carregamento direto por ID | Não; não foi pedido e os IDs são opacos. |
| Seleção carrega primeiro e abre o popup existente | Menu completamente novo ou implementação imediata | Esta decisão já tinha sido perguntada e aprovada. |
| Cancelar o popup mantém o plano carregado | Desfazer a injeção | Não; desfazer exigiria reescrever histórico. |
| Tornar o texto de cancelamento neutro | Manter “stay in Plan mode” fora de Plan mode | Não; é correção necessária da cópia reutilizada. |
| Usar o script existente para schemas | Corrigir a receita `just` quebrada como parte da feature | Não; evita scope drift de infraestrutura. |
| Testes focados mais um único gate amplo autorizado | Harness novo ou várias rondas de certificação | Não; segue o orçamento de validação do repositório. |
| Revisão P0 única para o novo fragmento | Nenhuma revisão contextual ou múltiplas auditorias | Não; é exigida pelo tamanho potencial do contexto. |
| Preservar os documentos untracked existentes | Editá-los ou incorporá-los na entrega | Não; são trabalho concorrente do utilizador. |
| Não regenerar clientes SDK adicionais | Expandir a entrega ao SDK Python | Não; Desktop consome o contrato app-server e os schemas gerados. |

