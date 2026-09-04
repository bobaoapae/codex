# Análise 04/09/2026 — chatgpt_web a falhar, navegador abre/fecha, janela de terminal do conector, Computer Use no ChatGPT Desktop

Só análise; nada foi alterado no código, no `~/.codex` nem nos processos. Horas em UTC.

## 1. Sessões chatgpt-web (consultor `chatgpt-pro`) — o que aconteceu

Fonte: rollouts em `~/.codex/sessions/2026/09/0{1..4}`, `~/.codex/chatgpt_web/daemon.log`, `daemon.json`, `connector.json`.

| quando | binário | resultado |
|---|---|---|
| 01/09 14:58 | 0.0.0 | `composer did not appear within 25000ms (tab url ?model=gpt-5-6-pro)` |
| 02/09 12:36–13:18 | 0.0.0 | 3 turnos Pro interrompidos (1142 s, 216 s, 812 s) + 3× `This conversation is still generating a reply` (precheck) — a corrida Pro anterior continuava viva na mesma conversa (classe diagnosticada em 30/08) |
| 02/09 17:15–22:38 | 0.0.0 | 4× `the chatgpt-web daemon did not come up within 15s` (`AUTOSTART_TIMEOUT`, `connector/daemon/mod.rs:750`) — sem linha no daemon.log para dizer porquê |
| 03/09 03:31–19:24 | 0.146.1 | Pro a funcionar (17 min / 1035 s / 1118 s / 816 s, resposta de 11 190 chars); 1 abort manual aos 10 min |
| 03/09 21:53 | 0.146.1 | instant: `composer label is 'GPT-5.6 SolLeve' which does not match the requested model` — rótulo novo no picker; as 2 tentativas seguintes responderam |
| 04/09 04:38 e 11:53 | 0.153.1 | `the daemon could not reach chatgpt.com through chrome-mcp` (90 s) |
| 04/09 12:12, 13:26, 14:10, 15:07 | 0.153.1 | `the ChatGPT connector was not ready within 90s` (90 s cada, 0 mensagens do assistente) |

Todas as 6 sessões de 04/09 são sub-agentes spawnados pelo Desktop (`originator: Codex Desktop`, role `chatgpt-pro`, sandbox `danger-full-access`), morrem no gate `wait_verified` antes de tocar no ChatGPT.

### 1.1 Causa raiz de 04/09 (duas fases, ambas ambiente, não código)

`daemon.log` de 04/09 (507 linhas, todas WARN):

1. **04:36:59 → 13:26** — 379× `reconcile failed: chrome-mcp: Chrome extension not connected` (uma por ciclo de 60 s, `BROWSER_UNAVAILABLE_RETRY`). Chrome fechado / extensão chrome-mcp adormecida.
2. **13:26:18 → agora** — 127× `tunnel tunnel_6a902697a0888191963057ca639226fa is not visible to the ChatGPT account logged in Chrome (0 tunnel(s) listed)`. Às 13:26:22 abriu uma nova janela do Chrome (a aba `chrome://newtab/` que o Computer Use listou com esse `lastOpened`).

Verificado ao vivo (eval numa aba chatgpt.com via chrome-mcp, com bearer de `/api/auth/session`):

- conta logada no Chrome: `joaovitorbor@gmail.com`, account `b7000e3e-7276-4615-9c4e-563c50a94509`, `plan_type: pro`, `structure: personal` (única conta);
- `GET /backend-api/aip/connectors/mcp/tunnels` → `200 {"tunnels":[]}`; Developer Mode continua ligado;
- `connector.json`: última verificação `verified_at_ms = 1788406310755` = **03/09 03:31:50Z** — o túnel era visível a esta conta há 36 h.

Conclusão: a partilha do túnel (org SURFTANk no platform.openai.com → esta conta ChatGPT) deixou de existir do lado da OpenAI entre 03/09 03:31 e 04/09 13:26. O tunnel-client continua a ligar-se ao mesmo `tunnel_id` (o túnel existe); o que falta é a associação à conta. Não pude confirmar no platform.openai.com: o Chrome não está logado lá (`/login?next=/settings/organization/tunnels`). O `daemon.json` está `registry_status: "failed"` desde 12:32 (pid 86660).

Não é regressão do binário: entre o sync anterior (`89433ec271`) e `HEAD`, só `d8db0018c8` tocou em `chatgpt_web` (refresh de modelos). O binário live (`vendor/.../codex.exe`) é de 03/09 23:17 (-03:00).

### 1.2 «Navegador abre e fecha sem fazer nada»

É o reconcile do daemon, por desenho (`connector/daemon/registry_api.rs` cabeçalho e `close()` l.741): quando não há aba chatgpt.com logada para emprestar, cria uma aba dedicada, faz os `fetch`, e fecha-a no fim do reconcile. Com o registry em `Failed`, o watcher (`registry.rs:1018-1035`) repete após `FAILURE_BACKOFF_CAP` = 60 s, para sempre. Desde 13:26 foram 127 aberturas/fechos; enquanto a partilha do túnel não voltar, continua.

Agravante do lado do agente: `client.rs:404 wait_verified` só falha cedo em `developer_mode_off`; com `registry_status == "failed"` e razão conhecida (túnel invisível) espera os 90 s inteiros e devolve uma mensagem genérica («run `codex chatgpt-web registry show`»). Cada consulta ao consultor custa 90 s e o Sol não fica a saber a causa.

### 1.3 «O conector fica uma tela de terminal aberta sem nada»

Confirmado ao vivo: janela do Windows Terminal (classe `CASCADIA_HOSTING_WINDOW_CLASS`) com título `C:\Users\Joao\.codex\chatgpt_web\bin\tunnel-client-v0.0.12.exe`; o processo 59916 (`tunnel-client`, filho do daemon 86660) tem um `conhost.exe 0x4` (26088) — console visível. Está vazia porque stdout/stderr são pipes do daemon.

Causa (código do fork): `connector/daemon/tunnel.rs:323-327` põe `CREATE_NO_WINDOW` e depois chama `JobObject::create_without_breakaway().spawn_contained(&mut command)`; `spawn_contained` chama `prepare_suspended_spawn` (`utils/pty/src/win/job.rs:126`), que faz `command.creation_flags(CREATE_SUSPENDED)` — `creation_flags` é um *setter* (substitui, não faz OR), logo `CREATE_NO_WINDOW` perde-se. Como o daemon foi spawnado com `DETACHED_PROCESS` (sem console, `daemon/mod.rs:705`), o filho console (`tunnel-client`, e o `cloudflared` no modo cloudflared) recebe um console novo, e no Windows 11 o terminal por omissão é o Windows Terminal → janela visível. O daemon em si não tem janela (as flags dele são aplicadas diretamente).

Detalhe adicional: `kill_pid` (`daemon/mod.rs:515`) e o `taskkill` de `tunnel.rs:393` correm sem `CREATE_NO_WINDOW` — piscam uma janela quando o daemon mata o filho.

### 1.4 daemon.log cego desde 27/08

O log só tem WARN desde 27/08 15:39 (últimas linhas INFO/DEBUG: «control API on…», «tunnel: ready», «connector verified», chamadas de tool). O filtro é `EnvFilter::try_from_default_env().unwrap_or("info")` (`cli/src/chatgpt_web_cmd.rs:144`). Os daemons de 30/08 e 04/09 foram spawnados pelo app-server do Desktop (86660 é filho de 75116) e herdam o ambiente dele; hipótese: `RUST_LOG=warn` (ou equivalente) no ambiente do Desktop. Consequência prática: não há como saber se o tunnel ficou ready, quando o conector foi verificado, nem porque o daemon «did not come up within 15s» em 02/09.

## 2. Computer Use no ChatGPT (Desktop 26.901.4073, processo `ChatGPT.exe`)

Sessões de 04/09 com chamadas reais: threads `01a06cf0-a834…` (15:02–15:12), `01a06c5e-76a7…`, `01a06c55-b660…` — tarefa: operar o Spine (app nativo Windows). Todas as chamadas foram `js` do servidor `cua_repl` (`namespace: mcp__cua_repl`); nenhuma via `node_repl`.

Sintomas nos rollouts:

- `cua.getState()` → `{"apps":[],"browsers":[Chrome extension "Seu Chrome", "Codex In-app Browser"]}` — nunca há apps;
- `cua.listApps` / `cua.getApp` → `is not a function` (o objeto só tem `initialize, getState, browsers, getBrowser, createBrowserTab, getTab, listBrowsers, listTabs`);
- `import("@oai/sky")` carrega, mas `sky.list_apps()` → **`Trusted RPC service is not configured: sky`** e `node_repl kernel unhandled rejection … kernel reset`;
- o primeiro `cua.getState()` de cada sessão (e após reset) devolve o texto «## Computer Use …» (banner/instruções) em vez do estado — o modelo lê-o como resposta e repete.

Causa (configuração escrita pelo Desktop, não pelo fork):

- `~/.codex/plugins/cache/openai-bundled/unified-computer-use/26.901.31953/.mcp.json` é reescrito pelo app (`app.asar`: `c.env={...,CUA_REPL_ENABLED_SURFACES:e.surfaces.join(','),[BROWSER_USE_AVAILABLE_BACKENDS]:e.browserBackends.join(',')}`) com **`CUA_REPL_ENABLED_SURFACES = "browser"`** e `BROWSER_USE_AVAILABLE_BACKENDS = "chrome,iab"`. `scripts/launch.mjs` só regista `sky: "@oai/sky/service"` em `NODE_REPL_TRUSTED_SERVICES` quando `surfaces` inclui `computer` → o serviço nativo fica fora do `cua_repl`.
- O plugin legado `computer-use@openai-bundled` (servidor `node_repl`, `[mcp_servers.node_repl.env]` no `config.toml` com `SKY_CUA_NATIVE_PIPE=1`, pipe `\\.\pipe\codex-computer-use-fd249c73-…` existe, `codex-computer-use.exe` de 04/09 07:55) aparece `ready` nos logs do Desktop, mas nos rollouts de 04/09 não há nenhuma tool `mcp__node_repl*` — só o hook `Stop → node_repl.turn_ended` o usa. A API nativa (`sky.list_apps()`) que a `guidance.md` deste plugin ensina («Use `node_repl` JavaScript for all Computer Use actions… `import("@oai/sky")`») não está ao alcance do modelo.
- Ou seja: as instruções prometem apps nativas, o runtime exposto (cua_repl) só tem browsers, e o runtime que teria o nativo (node_repl) não expõe tools.
- **Por que `surfaces = browser`: é por plataforma, no app.asar.** O código que monta as surfaces do `cua_repl` faz `p = f && l.platform === "darwin" && t.computerUse && u.enabled && u.paths.serviceAppPath != null; … p && h.push("computer")` — a surface `computer` do `cua_repl` só existe no macOS. No Windows o nativo fica em `computerUse = t.computerUse && (t.computerUseNodeRepl || p) …`, isto é, no caminho legado `node_repl` + `codex-computer-use.exe` (o `.mcp.json` do plugin `computer-use` também é saltado fora do darwin: `a && e.platform !== "darwin" → null`). Logo, no Windows, apps nativas só via `node_repl`; se o `node_repl` não expõe `js` ao modelo (não apareceu em nenhum rollout de 04/09; não consegui provar se está oculto em code mode ou ausente), o Computer Use nativo está inalcançável nesta combinação Desktop 26.901 + fork. `computer_use.windows.always_allowed_app_ids` (só Godot) sugere que já funcionou pelo caminho `node_repl` antes.

Logs do Desktop (`…\Packages\OpenAI.Codex_…\LocalCache\Local\Codex\Logs\2026\09\04`) não registam erro nenhum do `cua_repl`/`node_repl` — só `status=ready`.

## 3. Pontos para o plano (sem ordem)

1. **Túnel** — repor a partilha em platform.openai.com › Settings › Organization › Tunnels (adicionar a conta ChatGPT `b7000e3e-…` em «ChatGPT workspaces») e `codex chatgpt-web registry reconcile`; ou migrar para `tunnel = "cloudflared"` que não depende da partilha. Confirmar antes se a conta que o Desktop/Chrome usa é mesmo a que foi partilhada em 27/08.
2. **Gate do conector** — `wait_verified` falhar rápido com a razão do registry quando `status == failed` (túnel invisível, login, etc.) em vez de 90 s + mensagem genérica; expor a razão no erro do turno.
3. **Reconcile em falha** — não abrir/fechar uma aba a cada 60 s indefinidamente: backoff crescente até parar, ou reutilizar uma aba própria escondida, ou só reconciliar quando houver turno pendente.
4. **Janela do tunnel-client** — em `JobObject::prepare_suspended_spawn` preservar as flags já definidas (`CREATE_SUSPENDED | CREATE_NO_WINDOW`), ou passar as flags ao `spawn_contained`; idem para os `taskkill`. Ver se o `exec` sandbox upstream depende de `creation_flags` ser substituído.
5. **Log do daemon** — filtro fixo (`info`) ou variável própria (`CODEX_CHATGPT_WEB_LOG`) para não herdar o ambiente do Desktop; registar o resultado de cada reconcile (ok/verified) e não só as falhas.
6. **Picker** — tolerar rótulos novos (`GPT-5.6 SolLeve`) sem abortar o turno, ou tratar a mismatch como aviso quando a conversa foi criada.
7. **Computer Use** — no Windows o Desktop nunca liga a surface `computer` do `cua_repl` (só darwin), portanto forçar `CUA_REPL_ENABLED_SURFACES=browser,computer` é experimental (o serviço `sky` JS existe em `@oai/sky/dist`, mas não se sabe se fala com o pipe Windows). Caminhos: (a) verificar por que o `node_repl` não expõe `js` ao modelo (config `[mcp_servers.node_repl]` sem `enabled_tools`/`omit_tools_from`; ver como o core lista as tools dele) e expô-lo, (b) testar o override experimental via `config.toml` do plugin (o Desktop reescreve o `.mcp.json` a cada arranque, como foi feito para `codex-app-tools`), ou (c) alinhar as instruções (guidance.md) com o que está exposto. Testar primeiro `sky.list_apps()` no `node_repl` diretamente.
8. **Instruções vs primeiro `getState()`** — a primeira chamada devolve o banner; vale um aviso no AGENTS.md/role até a OpenAI corrigir.

## 4. Aprofundamento (segunda ronda, 04/09 à tarde)

### 4.1 Túnel: é troca de conta no Chrome, não perda de partilha

`tunnel-client admin tunnels get tunnel_6a90… --admin-key file:…\tunnel.key --json` (leitura, chave runtime):

```json
{ "id": "tunnel_6a902697a0888191963057ca639226fa", "name": "Codex Native",
  "creator": "user-bbSl5PBTX08wggS9iMwCH0vI",
  "workspace_ids": ["fbf63138-24fb-489e-8c2b-49826f916056"],
  "organization_ids": ["org-OupyR2Ovde5J08ldqVb3DIMg"] }
```

- O túnel continua partilhado com a conta ChatGPT `fbf63138-…`, a que `PROGRESSO.md` (27/08, l.305) registou como o `account_id` de `accounts/check` na altura, e com a qual o conector foi verificado até 03/09 03:31Z.
- O Chrome está agora logado em **outra** conta ChatGPT: `joaovitorbor@gmail.com`, user `user-Gs7lIwbOFWGB69gKMIizH770`, account `b7000e3e-…` (Pro, personal, única conta desse user; é a mesma do `auth.json` do CLI). Esse user não pertence à workspace `fbf63138`, por isso `mcp/tunnels` devolve `[]`.
- Linha temporal (`~/.chrome-mcp/daemon.log`): extensão desligada às 04/09 00:23:44Z (Chrome fechado), religada às 13:25:29Z e de novo 13:26:22Z. Entre uma coisa e outra o login do chatgpt.com mudou de `fbf63138` (provavelmente joao@joaoborges.dev) para a gmail.
- Decisão para o plano: (a) voltar a logar o Chrome na conta `fbf63138`; (b) editar o túnel em platform.openai.com › Tunnels e acrescentar `b7000e3e-…` em «ChatGPT workspaces» (precisa de admin key/UI; a runtime key só lê); (c) `tunnel = "cloudflared"`, que não depende de partilha. O registry podia detetar isto sozinho: comparar o `account_id` da aba com `workspace_ids` de `tunnel-client admin tunnels get` e dizer «conta X não está na lista [Y]».

### 4.2 Reconcile / gate: o que o `/healthz` carrega

`wire::HealthResponse` só leva `registry_status` (rótulo). A razão (`RegistryStatus::Failed { reason, retry_at_ms }`, `state.rs:81`) só sai em `codex chatgpt-web registry show`. Logo `wait_verified` não tem como falhar cedo com a causa sem alargar o wire (ou consultar `/v1/registry`). O watcher (`registry.rs:1018`) recalcula `retry_at_ms` a cada falha (`FAILURE_BACKOFF` 2/5/10 s, cap 60 s) e cada tentativa é uma aba nova (`registry_api.rs:151-225`, fecho em l.741), sem teto de tentativas.

### 4.3 Arranque do daemon (falhas «did not come up within 15s» de 02/09)

`daemon/mod.rs::start` (l.243-345): lock de instância, token, servidor MCP público, `build_tunnel_adapter` (await), `tunnel::start` (não bloqueia), registry, control API (só aqui o `/healthz` responde), watchers. `ensure_daemon` (l.750) faz `running_endpoint` a cada 300 ms durante `AUTOSTART_TIMEOUT` = 15 s. Dois modos de falha plausíveis, indistinguíveis sem log: (1) o processo novo sai logo com «another chatgpt-web daemon is already running» porque um daemon anterior ainda segura `daemon.lock` mas já não responde ao `/healthz` (`running_endpoint` exige os dois); (2) `build_tunnel_adapter` demora mais de 15 s. Nenhum WARN foi escrito nesses horários. Hoje não há lock stale (o daemon 86660 está vivo e responde).

### 4.4 daemon.log: causa confirmada

`app.asar`: o Desktop lança o app-server com `RUST_LOG: process.env.RUST_LOG ?? "warn"` (e `LOG_FORMAT=json`). O daemon herda o ambiente do app-server (`spawn_detached` não redefine `RUST_LOG`) e `EnvFilter::try_from_default_env()` em `chatgpt_web_cmd.rs:144` engole o `warn`. Correção: filtro próprio (`CODEX_CHATGPT_WEB_LOG`, default `info`) ou `env_remove("RUST_LOG")` no spawn.

### 4.5 Picker do ChatGPT mudou (afeta instant/medium/high/extra-high)

Lido ao vivo na aba (04/09): o gatilho já não é `[data-animated-slider-trigger]`; é um `button.__composer-pill[aria-haspopup=menu]` cujo texto é o nível atual («Alta»). O menu tem um `menuitem[aria-keyshortcuts="ArrowLeft ArrowRight"]` com `aria-label="Potência"` e texto «Alta, 3 de 5.», mais radios de modelo («GPT-5.6 Sol», «GPT-5.5»). Catálogo (`chatgpt_model list`): `gpt-5-6-instant`, `gpt-5-6` e `gpt-5-6-thinking` têm todos o título «GPT-5.6 Sol»; `gpt-5-6-pro` = «GPT-5.6 Pro».

O que o fork espera (`driver/page_scripts.rs:606, 652-694`; `driver/ops.rs:96-100`): gatilho «N[íi]vel de racioc[íi]nio|Reasoning», labels «Instantâneo|Médio|Alto|Extra alto|Pro». Hoje os labels são «Leve» (1 de 5) e «Alta» (3 de 5); não li os outros três para não mexer no slider. Consequências:
- `chatgpt-web/instant`: nota «composer label is 'GPT-5.6 SolLeve' which does not match…» (03/09 21:53); a falha fatal desse turno foi «conversation id not captured from the URL after the send», provavelmente o primeiro click engolido que o PROGRESSO já descreve.
- `chatgpt-web/medium|high|extra-high`: `menu_select` não encontra gatilho/label, cai em «exact level selection via menu failed … continuing with the model slug default»: o nível pedido **não é aplicado**, fica o default da conta (hoje «Alta»). Silencioso, só nas notas do turno.
- O bridge `chatgpt-pro-mcp` (Claude) também já não lê o picker (`discover` devolve `levels: null`).
- `chatgpt-web/pro` e `thinking` usam `?model=` na URL e não dependem do slider; continuam a funcionar (03/09).

### 4.6 Computer Use: o nativo existe, mudou de sítio, e o modelo deixou de o ver

- Sonda direta ao servidor (`node_repl.exe` com o env de `[mcp_servers.node_repl.env]`): expõe `js`, `js_reset`, `js_add_node_module_dir`, `turn_ended`; instruções «Use `js` for `node_repl` execution…». Uma chamada `js` com `sky.list_apps()` **fora do Desktop** morre em `windows sandbox failed: CreateProcessWithLogonW failed: 5` (o kernel corre sob o sandbox Windows; no meu contexto não há a conta do sandbox, não prova nada sobre o Desktop).
- Histórico nos rollouts: `sky.list_apps()` via `exec` → `tools.mcp__node_repl__js` funcionou **80 vezes entre 01/08 e 03/09 17:12** (listou o Spine, o HA Widget, etc.). Menções a `mcp__node_repl__js` por dia: 01/09 78, 02/09 1050, 03/09 211, **04/09 0**; `js_add_node_module_dir` nas listagens: 03/09 88, 04/09 0. Nas listagens de ALL_TOOLS de 04/09 aparecem `codex_apps`, `spine_workbench`, `codex_app`, `chrome`, `chatgpt` e **não** `node_repl`, embora os logs do Desktop mostrem `server=node_repl status=ready` 140 vezes nesse dia.
- O `cua_repl` (`unified-computer-use`) apareceu pela primeira vez às 04/09 02:22Z (atualização do Desktop para 26.901; o `codex-computer-use.exe` novo é de 04/09 07:55 local). O binário do fork foi trocado à mesma hora (03/09 23:17 -03), coincidência que convém desfazer com um teste controlado (binário 0.146.1 + Desktop 26.901).
- No `app.asar`: no Windows `computerUse === true ⇒ computerUseNodeRepl: true` (o nativo é sempre `node_repl`); `CODEX_ELECTRON_ENABLE_WINDOWS_COMPUTER_USE=1` força os dois; a surface `computer` do `cua_repl` só em darwin. Com o `cua_repl` ativo o Desktop mantém o `node_repl` mas esvazia as instruções de browser dele (`NODE_REPL_INSTRUCTIONS_USE_CASE_BROWSER/CHROME=""`, é o que está no `config.toml`). Por que os tools do `node_repl` deixaram de chegar ao modelo em 04/09 fica em aberto. Hipóteses: (a) o Desktop passa `mcp_servers.node_repl` no `thread/start` com `enabled_tools`/`disabled_tools` (grep do asar em curso); (b) o core do fork esconde ou colide os tools quando dois servidores expõem `js`/`js_reset` (não encontrei dedup por nome curto; o registo é por nome qualificado `mcp__<server>__<tool>`); (c) a exposição passou a `Deferred` (só via `search_tools`) e nenhum agente pesquisou.
- Sintoma derivado: com o `cua_repl` a expor `js` diretamente e o `node_repl` invisível, o modelo usa o `js` errado; as instruções do plugin `computer-use` (guidance.md) e o banner do `cua_repl` («## Computer Use… API Reference… `sky.list_apps()`») prometem a API nativa nesse mesmo `js`.

**Fecho (teste empírico, mesmo dia):**
- O construtor da configuração do `node_repl` no app.asar (`ny(...)`) produz só `{args:[], command, env, env_vars?, startup_timeout_sec:120}` — **sem** `enabled_tools`, `disabled_tools` ou `omit_tools_from`. O Desktop não filtra os tools do `node_repl`.
- Os «0 menções em 04/09» eram viés de amostragem: todos os dumps de `ALL_TOOLS` de 04/09 são filtrados por regex (`/computer|cua|sky|window|native app/`, `/agent|collab|message/`…) e a descrição do `js` do `node_repl` («Execute JavaScript in a persistent `node_repl`…», com os «Use Cases» de browser esvaziados pelo Desktop) não casa com nenhum. Em 03/09 havia 16 dumps completos (110–158 nomes) e o `node_repl` estava em 12.
- Teste real com o binário live 0.153.1: `codex exec -m gpt-5.6-luna -s read-only` a pedir `text(ALL_TOOLS.map(x=>x.name))` devolve 198 nomes, incluindo `mcp__node_repl__js`, `mcp__node_repl__js_add_node_module_dir`, `mcp__node_repl__js_reset` (mais `codex_apps` 139, `chrome` 24, `spine_workbench` 19, `chatgpt` 11). O core do fork expõe o `node_repl` em code mode; o `cua_repl` não aparece no `ALL_TOOLS` porque tem `omit_tools_from: [code_mode, deferred]` (é o `js` direto).
- Mecanismos que investiguei e ficam descartados/observados: `stable_catalog_revision` devolve `None` enquanto algum servidor arranca, por isso a binding do turno não fica presa numa lista incompleta; a tolerância `DEFAULT_OPTIONAL_MCP_STARTUP_GRACE` = 1 s (`mcp_optional_startup_grace_ms`) omite servidores opcionais lentos **do passo corrente**, mas os deltas starting→ready nos logs do Desktop são ~0 s (mediana 0,0 s, máx. 0,8 s em 04/09); o cache de catálogo (`tool_catalog_cache.rs`, TTL 30 min, opt-out por `codex/tool-catalog-cache.cacheable=false`) é em memória, sem ficheiro.
- Conclusão para o plano: o Computer Use nativo continua acessível como `tools.mcp__node_repl__js` dentro do `exec`; o que quebrou em 04/09 é **descoberta**: (1) o `js` direto e visível é o do `cua_repl` (só browser, por design do Desktop no Windows); (2) as instruções injetadas (guidance.md do `computer-use`, banner do `cua_repl`) mandam usar `sky.list_apps()` sem dizer que é o `node_repl` em code mode; (3) a descrição do `node_repl` perdeu as pistas («Use Cases») quando o Desktop passou a esvaziar `NODE_REPL_INSTRUCTIONS_USE_CASE_*`. Correção barata: instrução no AGENTS.md/role («no Windows, Computer Use nativo = `await tools.mcp__node_repl__js({code})` via `exec`; o `js` direto é só browser») e/ou `omit_tools_from` no `cua_repl` via `config.toml` do plugin para o tirar do caminho direto.

### 4.7 Ruído do chrome-mcp

`~/.chrome-mcp/run-daemon.vbs` (tarefa agendada) tenta arrancar um segundo daemon a cada 10 min e falha com `EADDRINUSE 127.0.0.1:8849` (144 vezes por dia). Inofensivo, mas enche o log.

### 4.8 Pais dos consultores falhados (04/09)

Quatro threads Sol diferentes (`01a04eda…`; `01a06c40…` Schrodinger; `01a06c16…` Euclid e Epicurus; `01a06c5e…` Nietzsche e Bacon), todos `spawn_agent` `chatgpt-pro` com `fork_turns: none` para pesquisa web; cada um perdeu 90 s e seguiu sem o consultor. A conversa de 02/09 12:36 (`6a9813e7…`) tinha 52 itens entregues e `message_landed_unanswered: true`: é a corrida Pro longa que depois bloqueou os envios seguintes («still generating»).
