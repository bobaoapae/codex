# Conexões SSH do app Codex/ChatGPT Desktop — como funcionam e o que o fork precisa

**Data:** 2026-09-03
**Escopo:** entender o mecanismo por trás de *Configurações → Conexões → SSH* no app
desktop, e o que falta para o fork rodar num **Mac remoto** (`joaovitorborges@192.168.80.102`)
ou num **Windows remoto**, servindo **a conta da sessão que está chamando**.
**Status:** análise. Nada foi aplicado, nem no repo nem em `~/.codex`.

---

## 0. Resposta curta

1. **O controle remoto por SSH não é "algo do ChatGPT".** O lado servidor é
   **100 % o binário `codex`** — nenhum agente proprietário roda no host remoto.
   O que é do app desktop é apenas o *cliente*: a lista de hosts, o `ssh` local,
   e o script de bootstrap. Isso está no `app.asar` do pacote MSIX `OpenAI.Codex`,
   num módulo marcado `ssh_websocket_v0`.

2. **O app se conecta assim:** roda `codex app-server --listen unix://` no host
   remoto (via login shell), e depois abre um segundo SSH que faz
   `exec codex app-server proxy`. Esse proxy é o `codex-stdio-to-uds`: liga
   stdin/stdout ao control socket. Por cima do pipe o desktop fala **WebSocket
   JSON-RPC** (`ws://codex-app-server/rpc`). Ou seja: **o fork já é o servidor**,
   basta ele estar no PATH do login shell remoto.

3. **Não é o mesmo que "Controlar este PC".** Aquilo é o relay `remoteControl/*`
   (enrollment por device key contra `chatgpt.com`, pairing code, `codex app-server daemon`).
   O `app-server-daemon` é `#[cfg(unix)]`, mas **a aba SSH não usa o daemon** —
   então o "Unix-only" do daemon **não** bloqueia um Windows remoto.

4. **O ponto que hoje quebra o requisito das 2 contas:** um processo `app-server`
   = um `CODEX_HOME` = um `auth.json` = **uma conta**. O desktop **não manda nada**
   sobre a conta local para o host remoto; a sessão remota autentica e é cobrada
   com o `auth.json` que existir **no Mac**. Os tipos `account/sessions/{add,list,switch,logout}`
   existem no protocolo mas estão **dormentes** (não passam pelo `message_processor`).

5. **Caminho recomendado:** curto prazo, **duas conexões SSH** (uma por conta),
   isoladas por **usuário Unix** no Mac — funciona hoje, sem código. Médio prazo,
   a feature do fork que dá seleção automática por sessão é um **shim de `ssh`**
   no lado Windows que injeta a identidade da conta ativa no payload remoto,
   mais um resolvedor de `CODEX_HOME` por conta no `codex` remoto.

---

## 1. Evidências e onde elas estão

Pacote analisado:

```
OpenAI.Codex_26.825.6671.0_x64__2p2nqsd0c76g0
  app\resources\app.asar          (286 MB, Electron)
  app\resources\codex             (270 MB — o codex que o app usa localmente)
  app\resources\codex-code-mode-host
```

Toda a lógica de SSH está num bundle do `app.asar`, com telemetria prefixada
`ssh_websocket_v0.*` (`proxy_command_starting`, `ensure_remote_app_server`,
`remote_codex_install_started`, …). Constantes relevantes, transcritas do bundle:

```js
uce = `codex`
dce = 'PATH="${CODEX_INSTALL_DIR:-$HOME/.local/bin}:$PATH"; export PATH'
fce = 'CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"; export CODEX_HOME'
Dce = `ws://codex-app-server/rpc`
Oce = '"${CODEX_HOME:-$HOME/.codex}/app-server-control"'
vC  = '"${CODEX_HOME:-$HOME/.codex}/app-server-control/app-server.log"'
yC  = '"${CODEX_HOME:-$HOME/.codex}/app-server-control/forwarded-ssh-agent.sock"'
xC  = { batchMode:"BatchMode=yes", serverAliveIntervalSeconds:15, serverAliveCountMax:12 }
iC  = `https://chatgpt.com/codex/install.sh`
sce = `https://chatgpt.com/codex/install.ps1`
SC  = 86   // exit code sentinela: "codex não achado no PATH"
```

Lado Rust (este repo), os pontos que casam com o cliente:

| Peça | Arquivo |
| --- | --- |
| `--listen unix://` (path vazio → `$CODEX_HOME/app-server-control/…`) | `codex-rs/app-server-transport/src/transport/mod.rs:116` |
| derivação do socket a partir do `CODEX_HOME` | `codex-rs/app-server-transport/src/transport/mod.rs:58` |
| acceptor do control socket + upgrade para WebSocket | `codex-rs/app-server-transport/src/transport/unix_socket.rs:26` |
| `codex app-server proxy` | `codex-rs/cli/src/main.rs:1370` → `codex_stdio_to_uds::run` |
| relay stdio ↔ socket (multiplataforma) | `codex-rs/stdio-to-uds/src/lib.rs` |
| AF_UNIX no Windows | `codex-rs/uds/src/lib.rs:171` (`mod platform` windows) |
| `find_codex_home()` (lê `CODEX_HOME`) | `codex-rs/utils/home-dir/src/lib.rs:13` |
| daemon Unix-only (**não usado pela aba SSH**) | `codex-rs/app-server-daemon/src/lib.rs` |
| tipos multi-conta dormentes | `codex-rs/app-server-protocol/src/protocol/v2/account.rs:194-258` |
| vault de contas do fork | `codex-rs/login/src/auth/vault.rs` |

---

## 2. O fluxo exato, passo a passo

Toda invocação remota passa por um wrapper que **força o login shell** do usuário:

```sh
sh -c '<preâmbulo>' sh '<payload>'
```

O preâmbulo exige `$SHELL` apontando para um executável (senão aborta com
*"Codex remote SSH requires SHELL to point to an executable login shell"*),
e então despacha por família de shell:

| `$SHELL` | como o payload roda |
| --- | --- |
| `csh` / `tcsh` | `$SHELL -i -c 'set loginsh=1; source /etc/csh.login; source ~/.login; exec /bin/sh -c "$CODEX_REMOTE_PAYLOAD"'` |
| `nu` | `$SHELL -l -i -c 'exec /bin/sh -c $env.CODEX_REMOTE_PAYLOAD'` |
| `fish` / `xonsh` | `$SHELL -l -i -c 'exec /bin/sh -c "$CODEX_REMOTE_PAYLOAD"'` |
| qualquer outro | `$SHELL -l -i -c 'CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"; export CODEX_HOME; exec /bin/sh -c "$CODEX_REMOTE_PAYLOAD"'` |

O payload em si começa com `printf '%b' '<marcador aleatório de 8 bytes em octal>'`
(para o cliente descartar banner de login shell) e depois
`PATH="${CODEX_INSTALL_DIR:-$HOME/.local/bin}:$PATH"; export PATH`.

### 2.1 Sequência de conexão

1. **Probe local:** `ssh -G <alvo>` para descobrir se há `ProxyCommand` que
   dependa do PATH do shell do usuário.
2. **Probe do binário:** `command -v codex >/dev/null || exit 86`.
   Exit 86 → erro *"No `codex` found in PATH. Please install the Codex CLI on the remote machine."*
3. **Probe de versão:** `codex --version`, validado contra um mínimo do app.
4. **Instalação (se necessário):** `uname -s` →
   - `darwin` / `linux` → `curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_RELEASE=latest CODEX_NON_INTERACTIVE=1 sh`
   - `cygwin*` / `mingw*` / `msys*` → `CODEX_INSTALL_DIR="$(cygpath -w …)" powershell -ExecutionPolicy ByPass -c '$env:CODEX_RELEASE="latest"; $env:CODEX_NON_INTERACTIVE="1"; irm https://chatgpt.com/codex/install.ps1 | iex'`

   → **o app já prevê host remoto Windows**, desde que o login shell seja POSIX.
5. **Bootstrap do app-server:**

   ```sh
   if [ "${CODEX_SSH_SKIP_APP_SERVER_BOOT:-}" = "true" ]; then exit 0; fi;
   (umask 077;
    mkdir -p -- "${CODEX_HOME:-$HOME/.codex}/app-server-control"
    && (pkill -9 -U "$(id -u)" -f 'codex.*[d]esktop-ssh-websocket-v0.sock' || true)
    && if [ -S "${SSH_AUTH_SOCK:-}" ]; then
         ln -sfn -- "$SSH_AUTH_SOCK" "${CODEX_HOME:-$HOME/.codex}/app-server-control/forwarded-ssh-agent.sock"
       elif [ ! -S "${CODEX_HOME:-$HOME/.codex}/app-server-control/forwarded-ssh-agent.sock" ]; then
         rm -f -- "${CODEX_HOME:-$HOME/.codex}/app-server-control/forwarded-ssh-agent.sock"
       fi
    && : > "${CODEX_HOME:-$HOME/.codex}/app-server-control/app-server.log")
   && SSH_AUTH_SOCK="${CODEX_HOME:-$HOME/.codex}/app-server-control/forwarded-ssh-agent.sock" \
      nohup codex -c features.code_mode_host=true app-server --listen 'unix://' \
      > "${CODEX_HOME:-$HOME/.codex}/app-server-control/app-server.log" 2>&1 &
   ```

6. **Transporte:** segundo SSH, `-T`, rodando
   `<relink do agente> && exec codex app-server proxy`.
   Opções fixas: `-v -o BatchMode=yes -o ConnectTimeout=<n> -o ServerAliveInterval=15 -o ServerAliveCountMax=12`.
   Alvo: **alias** do `~/.ssh/config` se houver, senão `user@host` + `-p port` (+ `-i identity`).
   O `ssh` é resolvido **pelo PATH**.
7. Sobre esse pipe stdio o desktop fala WebSocket JSON-RPC; o `app-server`
   faz `accept_async` em cima da conexão do socket Unix (`unix_socket.rs`), então
   os dois lados falam a mesma coisa.

### 2.2 Outros fatos úteis do cliente

- **Kill:** `pkill -9 -U "$(id -u)" -f 'codex.* app-server.* --listen'` — mata
  **todos** os app-servers daquele usuário Unix (ver §6, armadilha 1).
- **Kill-switch local:** `CODEX_APP_SERVER_FORCE_CLI=1` desativa o transporte SSH.
- **Pular boot remoto:** `CODEX_SSH_SKIP_APP_SERVER_BOOT=true` no ambiente remoto.
- **Provisionamento:** o desktop lê `$CODEX_HOME/codex-app/config.json` (v1):

  ```json
  {
    "version": 1,
    "sshConnectTimeoutSeconds": 30,
    "remoteConnectionMaxRetryAttempts": 3,
    "remoteConnections": [
      { "sshAlias": "mac-joao", "projects": [ { "remotePath": "/Users/…/proj", "label": "Proj" } ] }
    ]
  }
  ```

  As conexões são chaveadas por **alias de SSH** — isto é, `~/.ssh/config` é
  ponto de extensão de primeira classe.
- **Threads são indexadas por host** (`hostKind` = `local` | `ssh`).
- Existe também um transporte **WSL** no mesmo painel de Conexões, que já mapeia
  `CODEX_HOME` do Windows para dentro da distro via `WSLENV` — precedente direto
  de "um CODEX_HOME diferente por conexão".

---

## 3. Onde a conta entra — e por que hoje não atende

- `app-server` é **mono-conta por processo**: o `auth.json` vem do `CODEX_HOME`
  com que o processo subiu. `account/read`, `account/login/start`, `account/logout`
  operam sobre *a conta ativa do app-server*.
- Os tipos `AccountSessionsAdd/List/Switch/Logout` existem em
  `protocol/v2/account.rs` mas **não são despachados** em
  `app-server/src/message_processor.rs` — multi-conta por conexão **não existe** hoje.
- O desktop **não transmite** identidade de conta local no bootstrap SSH. Não há
  `SendEnv`, não há flag, não há campo no `initialize`.
- Consequência prática: **o Mac remoto roda com a conta que estiver logada no
  `~/.codex/auth.json` dele**, e é essa conta que paga — independentemente de
  quem chamou. Trocar de conta no Windows (`codex account switch`, que reescreve
  o slot ativo — `login/src/auth/vault.rs`) **não muda nada no remoto**.

O `CODEX_HOME` é a única variável que decide, de ponta a ponta, *qual instância
remota* responde: ele determina o `auth.json`, o socket de controle
(`$CODEX_HOME/app-server-control/…`, `transport/mod.rs:58`) e o log. Duas contas =
dois `CODEX_HOME` = dois app-servers. Toda a análise abaixo gira em torno de
**como escolher esse `CODEX_HOME` por sessão**.

---

## 4. Viabilidade por plataforma remota

### 4.1 Mac remoto — viável hoje

Nada no caminho é específico de plataforma além do shell POSIX, que o macOS tem.
Requisitos:

1. `sshd` habilitado (Remote Login) e chave publicada — `BatchMode=yes` significa
   **zero prompt**: senha interativa não funciona, tem que ser chave.
2. `$SHELL` do usuário apontando para shell executável (padrão no macOS: `/bin/zsh`).
3. O `codex` do fork achável pelo login shell — ver §6, armadilha 2.
4. `codex-code-mode-host` **ao lado** do `codex` — o desktop força
   `-c features.code_mode_host=true`, e `InstallContext::code_mode_host_program()`
   (`install-context/src/lib.rs:176`) procura o host em `codex-resources/` do pacote
   ou no **mesmo diretório do executável**. Sem ele, o boot sobe mas o code-mode falha.
5. Build `aarch64-apple-darwin` do fork (ou x86_64 + Rosetta).

### 4.2 Windows remoto — viável, mas com mais arestas

O que **não** é problema:

- `app-server --listen unix://` funciona: `codex-uds` tem implementação Windows
  (AF_UNIX + `validate_private_socket_path` + `ensure_non_elevated_peer`),
  e `unix_socket.rs` tem os `#[cfg(windows)]` correspondentes.
- `codex app-server proxy` é neutro de plataforma (`stdio-to-uds/src/lib.rs`).
- O `app-server-daemon` ser Unix-only **não importa**: a aba SSH não o usa.
- O instalador do app já tem branch `windows` (via `cygpath` + `install.ps1`).

O que **é** arestas:

| Aresta | Detalhe |
| --- | --- |
| Login shell POSIX | `sshd` do Windows precisa de `DefaultShell` = `bash.exe` (Git for Windows), e `$SHELL` precisa estar setado e executável, senão o wrapper aborta. |
| `uname -s` | Git Bash devolve `MINGW64_NT-10.0-…` → cai no branch `windows`, ok. |
| `pkill` | Não existe no Git Bash. Está protegido por `|| true` no bootstrap, mas o `killCodexProcess()` do "desconectar" vai falhar. |
| `ln -sfn` do agente | Guardado por `[ -S "$SSH_AUTH_SOCK" ]`, que é falso no OpenSSH do Windows → cai no `rm -f`. Sem forward de agente, mas não quebra. |
| `nohup … &` | Precisa de teste real: o app-server tem que sobreviver ao fim da sessão SSH sob Git Bash. É o risco número 1 desse cenário. |
| Sandbox | Vale o que já sabemos do Windows: `pwsh` do WindowsApps quebra o sandbox (`CreateProcessAsUserW failed: 5`); usar o `pwsh` Win32. |

**Veredito:** Mac remoto é o caminho de menor atrito; Windows remoto é plausível
e vale um experimento, mas com o `nohup` como incógnita principal.

---

## 5. Opções de design para "a conta da sessão que está chamando"

### Opção A — Uma conexão SSH por conta *(sem código, funciona hoje)*

Duas identidades remotas, cada uma com seu `CODEX_HOME`.

**A1 — dois usuários Unix no Mac (recomendado):**

```
# ~/.ssh/config no Windows
Host mac-joao
  HostName 192.168.80.102
  User joaovitorborges
Host mac-contato
  HostName 192.168.80.102
  User codexcontato
```

Cada usuário tem seu `$HOME/.codex` com seu `auth.json`. O isolamento é total:
sockets distintos, e o `pkill -U "$(id -u)"` do app **já** é por usuário.

**A2 — um usuário, dois `CODEX_HOME`:** exige `AcceptEnv CODEX_HOME` no
`sshd_config` do Mac e `SetEnv CODEX_HOME=…` por alias no `~/.ssh/config`.
Funciona, mas herda a armadilha 1 (§6): o "desconectar" de uma conexão mata o
app-server da outra.

- ✅ Zero código, ~15 min de setup.
- ❌ **Manual**: o usuário escolhe a conexão certa por sessão. Não é "automático
  pela sessão que chama".

### Opção B — Shim de `ssh` no Windows *(a feature que atende o requisito)*

O desktop resolve `ssh` **pelo PATH**. Um shim do fork, à frente do `ssh` real:

1. detecta que é payload do Codex (argv contém `CODEX_REMOTE_PAYLOAD`);
2. lê a conta ativa local (identidade do `auth.json` / `session_id` do vault);
3. injeta no payload, logo após o `export PATH`, algo como
   `CODEX_ACCOUNT='<session_id>'; export CODEX_ACCOUNT;`
4. `exec` no `ssh` real. Para qualquer outro argv, passa reto sem tocar.

Por que funciona: o payload roda **depois** do `CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"`
do wrapper, então o que ele exportar prevalece; e **tanto** o `--listen unix://`
**quanto** o `app-server proxy` re-derivam o socket do `CODEX_HOME` no momento da
execução, então os dois lados continuam coerentes.

E, crucialmente: **não exige tocar no `sshd` do Mac** (nada de `AcceptEnv`),
porque a informação viaja dentro do comando, não do ambiente SSH.

### Opção C — Resolvedor de `CODEX_HOME` por conta no `codex` remoto *(par da B)*

Complemento natural da B: em vez do shim conhecer o filesystem do Mac, ele manda
só a **identidade**; o `codex` remoto traduz. Patch cirúrgico em
`codex-rs/utils/home-dir/src/lib.rs`:

- `CODEX_HOME` explícito continua ganhando (precedência intocada);
- senão, se `CODEX_ACCOUNT` estiver setado, resolve por um mapa no host remoto
  (ex.: `$HOME/.codex/profiles.toml`, ou convenção `$HOME/.codex-profiles/<id>`);
- senão, comportamento upstream.

Vantagens: o lado Windows fica agnóstico ao layout do Mac; um mesmo alias SSH
serve as duas contas; e o `codex account` do fork pode ganhar um `export`/`import`
para semear os homes remotos sem refazer login à mão.

Cuidado: `find_codex_home()` é usado em todo lugar — a mudança tem que ser
centralizada nessa função e coberta por teste, senão vira divergência sutil.

### Opção D — `exec-server` como ambiente remoto *(resolve a conta por construção)*

Existe um segundo caminho de "remoto" no repo, independente do app-server:
`codex exec-server` escutando `ws://IP:PORT`, declarado em
`$CODEX_HOME/environments.toml` (`exec-server/src/environment_toml.rs`):

```toml
default = "mac"
[[environments]]
id  = "mac"
url = "ws://192.168.80.102:8081"
```

Aqui a **sessão roda local** (Windows, com a conta local — o problema de conta
simplesmente não existe) e só execução de comando e filesystem vão para o Mac.
No protocolo já há `environment/add`, `environment/info`, `environment/status`,
e seleção por `thread/start` / `turn/start` (`app-server/README.md:266-269`).

**Porém:** o desktop **não implementa** isso — `environment/add` e `execServerUrl`
têm **zero** ocorrências no `app.asar`. Então essa via serve à TUI/CLI/agentes do
fork, **não** ao painel de Conexões do app. E `ws://` puro na LAN pede cuidado
com autenticação.

### Comparativo

| | Automático por sessão | Muda o desktop? | Muda o `sshd` do Mac? | Código no fork | Esforço |
| --- | --- | --- | --- | --- | --- |
| **A1** (2 usuários) | ❌ | não | não | nenhum | ~15 min |
| **A2** (2 `CODEX_HOME`) | ❌ | não | **sim** | nenhum | ~30 min |
| **B + C** (shim + resolver) | ✅ | não | não | shim + `find_codex_home` | 1–2 dias |
| **D** (exec-server) | ✅ (por construção) | não usa | não | config only | 2–4 h, mas fora do app |

---

## 6. Armadilhas conhecidas

1. **`pkill` largo.** `pkill -9 -U "$(id -u)" -f 'codex.* app-server.* --listen'`
   mata todo app-server do usuário. Com duas contas no mesmo usuário Unix,
   desconectar uma derruba a outra. → **argumento decisivo a favor de dois
   usuários Unix (A1)**, ou de aceitar reconexão automática.

2. **`install.sh` pode sobrescrever o fork.** O PATH remoto começa com
   `${CODEX_INSTALL_DIR:-$HOME/.local/bin}`. Se o fork ficar em `~/.local/bin/codex`,
   ele ganha do resto — mas o auto-install do app escreve exatamente ali. O
   auto-install só dispara quando o `codex` some do PATH ou reprova no gate de
   versão, então: manter a versão do fork ≥ mínimo do app (hoje `codex-cli 0.146.1`)
   e ter um passo de redeploy no runbook.

3. **`codex-code-mode-host` é obrigatório.** O desktop força
   `-c features.code_mode_host=true`. O binário é procurado em `codex-resources/`
   ou **no diretório do próprio `codex`** (`install-context/src/lib.rs:176-200`).
   Deployar os dois juntos, sempre.

4. **`BatchMode=yes`.** Sem chave configurada, a conexão falha sem prompt.
   Passphrase precisa de agente carregado no Windows.

5. **`$SHELL` obrigatório.** Se o login shell não estiver setado/executável, o
   wrapper aborta antes de qualquer coisa. Relevante no Windows remoto.

6. **Duas versões do protocolo.** O app-server remoto e o desktop precisam falar
   o mesmo dialeto. Sync de fork que mexa em `app-server-protocol` tem que ser
   deployado nos dois lados juntos.

7. **Sandbox no Windows remoto.** Vale o achado anterior: `pwsh` do WindowsApps
   quebra o sandbox; garantir `shell_detect` no `pwsh` Win32.

---

## 7. Recomendação

**Fase 0 — provar o canal (hoje, sem código).**
Deployar o fork (`codex` + `codex-code-mode-host`) em `~/.local/bin` no Mac,
adicionar `joaovitorborges@192.168.80.102` no painel e conectar. Validar com:

```sh
# no Mac, depois de conectar pelo app
tail -f ~/.codex/app-server-control/app-server.log
ls -la ~/.codex/app-server-control/
pgrep -laf 'app-server --listen'
```

Se o app reclamar de versão, comparar `codex --version` com o do
`app\resources\codex` do pacote.

**Fase 1 — duas contas, já (A1).**
Criar um segundo usuário no Mac, `codex login` em cada `~/.codex`, dois aliases,
duas conexões no app. Isso satisfaz "funcionar com as 2 contas" — com escolha manual.

**Fase 2 — automático (B + C).**
Shim de `ssh` no Windows injetando `CODEX_ACCOUNT` + resolvedor centralizado em
`find_codex_home()`, com `codex account` ganhando `export`/`import` para semear
os homes remotos. Uma conexão no app passa a servir qualquer conta.

**Fase 3 — opcional (D).**
Se o objetivo for "quem chama é quem paga" com o Mac só como músculo de execução,
`exec-server` + `environments.toml` resolve isso sem tocar em conta nenhuma — mas
só pela TUI/CLI do fork, não pelo painel de Conexões.

---

## Anexo — reprodução da investigação

```powershell
Get-AppxPackage | ? { $_.Name -match 'OpenAI|ChatGPT' } | fl Name, InstallLocation
# copiar app.asar para um dir legível (WindowsApps bloqueia traverse no bash)
```

```bash
grep -abo "ssh_websocket_v0.proxy_command_starting" app.asar   # → offset 3640690
dd if=app.asar of=ssh_region.js bs=1 skip=3560000 count=200000
grep -a -o -E ".{80}ssh.{80}" app.asar
grep -a -c "environment/add" app.asar    # → 0 (desktop não usa exec-server)
```
