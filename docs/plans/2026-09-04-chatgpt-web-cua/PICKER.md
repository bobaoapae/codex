# O picker de esforço do chatgpt.com

O composer expõe o nível de raciocínio num **slider**, não num submenu de
`menuitemradio`. O item acessível é um `[role="menuitem"][aria-keyshortcuts]`
(`ArrowLeft ArrowRight`) e o texto do seu contentor lê-se

```
<label>, <n> de <total>.        (PT)
<label>, <n> of <total>.        (EN)
```

## Porque é que o ordinal manda

Os rótulos já mudaram pelo menos uma vez sem aviso:

| Posição | Rótulo até 03/09 | Rótulo em 04/09 |
| --- | --- | --- |
| 1 | Instantâneo | Leve |
| 2 | Médio | *(por confirmar)* |
| 3 | Alto | Alta |
| 4 | Extra alto | *(por confirmar)* |
| 5 | Pro | *(por confirmar)* |

Enquanto a seleção era feita por rótulo, `medium|high|extra-high` deixavam de
casar e caíam **em silêncio** no default da conta — o turno corria com o
esforço errado e a única pista era o rótulo «SolLeve» visto às 21:53Z de 03/09.

O `<n> de <total>` não mudou. Por isso `LEVELS`
(`core/src/chatgpt_web/driver/ops.rs`) guarda o **índice 1-based** de cada
nível e é ele que o `menu_select` navega: lê a posição atual, carrega em
`ArrowLeft`/`ArrowRight` `|INDEX - atual|` vezes, e pára. Os rótulos ficam
apenas para (a) reconhecer onde o slider está quando não anuncia posição
(fallback) e (b) reportar de volta — daí a nota «effort level set through the
picker: Alta (3/5)».

Quando o rótulo da posição escolhida não é nenhum dos que o Codex conhece, a
seleção **mantém-se** (o ordinal é a instrução) e o turno ganha uma nota a
dizer que a tabela de rótulos está desatualizada. É o sinal para voltar aqui.

## Como reconfirmar a tabela

Com um chat descartável (mover o slider é transitório, mas fica na conta até à
próxima seleção):

```powershell
codex chatgpt-web discover-menu   # ou o caminho equivalente do driver
```

O ramo do slider de `menu_discover` percorre o controlo de ponta a ponta e
devolve `{ slider: true, current, levels: [{ index, label }] }`. Copiar a
coluna de rótulos para a tabela acima e para `LEVELS` em `ops.rs`.

## Verificação no composer

O nível deixou de viver no botão do modelo: renderiza como uma *pill* própria
(`button.__composer-pill`). O `composer_state` devolve agora `pills`, e a
verificação pós-seleção testa o rótulo esperado contra o botão do modelo **e**
contra todas as pills — antes disso, um nível corretamente aplicado produzia um
aviso de mismatch.

## Confirmação ao vivo (04/09, 20:5x)

Com o binário 0.153.3 já em uso, o pill do composer em `chatgpt.com/` lê:

```
GPT-5.6 SolLeve       (nível 1)
GPT-5.6 SolMédia      (nível 2, depois de um reload)
```

Confirma as duas premissas desta correção: o nível vive mesmo em
`button.__composer-pill` (por isso o `composer_state` passou a devolver
`pills`), e os rótulos novos são «Leve» e «Média» — ambos já na tabela `LEVELS`.

## Bloqueador aberto: o composer não submete (pré-existente)

A nota «Alta (3/5)» **não pôde ser observada num turno real**: desde já — e
independentemente destas alterações — o composer do chatgpt.com deixou de
submeter. Provado com:

- `codex exec -m chatgpt-web/instant` (que nem abre o picker) falha igual, em
  `[send phase: submit] send failed`;
- o binário **anterior** (`codex-cli 0.153.1`, sem nada disto) falha
  exatamente da mesma maneira;
- na página, com o separador **visível** e acabado de recarregar, nenhuma destas
  vias submete: `MouseEvent('click')` sintético, sequência
  `pointerdown/pointerup/click`, `HTMLElement.click()` nativo,
  `form.requestSubmit(sendButton)`, `Enter` no editor, e um clique **confiável**
  via CDP (`Input.dispatchMouseEvent`). O botão
  `[data-testid="send-button"]` existe, não está `disabled`, e o texto fica no
  composer; a URL nunca passa a `/c/<id>` e o stop button nunca aparece.

Ou seja: o seletor está certo e o clique chega — o handler de submit é que
recusa. É uma quebra do caminho de envio, à parte do picker, e fora do âmbito
deste plano. Enquanto durar, o provider `chatgpt_web` não completa turnos.
