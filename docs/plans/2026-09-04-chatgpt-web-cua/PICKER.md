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
