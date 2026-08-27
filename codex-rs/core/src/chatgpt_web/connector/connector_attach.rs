//! FORK: the browser side of the connector mode (C4).
//!
//! Two things have to happen in the ChatGPT tab for a connector turn: the
//! connector must be selected in the composer (the `@mention` pill), and the
//! per-turn tool-approval card must be answered. Both are page scripts (pure
//! function expressions, promise chains, never `async`) plus thin driver
//! helpers that run them through the chrome-mcp daemon.
//!
//! Selection is sticky per conversation (spike S4): once a connector is used in
//! a chat, later messages reach it with no pill. So the mention only runs when
//! the pill is absent — a fresh chat, or the first connector turn.

use crate::chatgpt_web::driver::DriverError;
use crate::chatgpt_web::driver::daemon::DaemonClient;
use crate::chatgpt_web::driver::tabs::TabId;
use crate::chatgpt_web::driver::tabs::TabPool;
use serde::Deserialize;
use std::sync::Arc;
use tracing::warn;

/// How long the approval-card click may take.
const APPROVAL_TIMEOUT_MS: u64 = 8_000;

/// JSON escaper shared with the page scripts (same contract as the driver's
/// `page_scripts::j`, kept private there).
fn j<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Result of the approval-card script.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ApprovalResult {
    pub(crate) found: bool,
    pub(crate) clicked: bool,
    pub(crate) button: Option<String>,
    /// Buttons of a card that matched no known label (for the log).
    pub(crate) buttons: Vec<String>,
    /// First characters of that card's text.
    pub(crate) text: Option<String>,
}

/// Selects the connector (if needed) and appends `text` after its pill, in one
/// page round-trip.
///
/// Used by `ops::send` in place of `set_composer_text` for a connector turn:
/// the plain composer script clears the editor, which would remove the pill.
/// Returns the `{ok, error}` shape `set_composer_text` returns so the send
/// phase machine treats it identically.
pub(crate) fn mention_and_compose_script(connector_name: &str, text: &str) -> String {
    let trigger = connector_name.split_whitespace().next().unwrap_or("codex");
    format!(
        r#"() => {{
    const NAME = {name};
    const TRIGGER = {trigger};
    const TEXT = {text};
    const ed = document.querySelector('#prompt-textarea');
    if (!ed) return JSON.stringify({{ ok: false, error: 'composer (#prompt-textarea) not found' }});
    const pill = () => Array.from(document.querySelectorAll('[data-id^="plugin:"][data-keyword]'))
      .filter((p) => (p.getAttribute('data-keyword') || '') === NAME && p.offsetParent !== null);
    const appendText = () => {{
      ed.focus();
      try {{
        const sel = window.getSelection();
        const range = document.createRange();
        range.selectNodeContents(ed);
        range.collapse(false);
        sel.removeAllRanges();
        sel.addRange(range);
      }} catch (e) {{}}
      const lead = pill().length ? ' ' : '';
      try {{ document.execCommand('insertText', false, lead + TEXT); }} catch (e) {{
        return JSON.stringify({{ ok: false, error: 'could not insert the prompt text' }});
      }}
      return new Promise((r) => setTimeout(r, 150)).then(() => {{
        const got = (ed.innerText || '').trim();
        return JSON.stringify({{ ok: got.length > 0, length: got.length }});
      }});
    }};
    if (pill().length === 1) return appendText();
    ed.focus();
    try {{ document.execCommand('selectAll', false); document.execCommand('delete', false); }} catch (e) {{}}
    try {{ document.execCommand('insertText', false, '@' + TRIGGER); }} catch (e) {{
      return JSON.stringify({{ ok: false, error: 'could not type the mention trigger' }});
    }}
    const rowTitle = (row) => ((row.innerText || '').split('\n')[0] || '').replace(/\s+/g, ' ').trim();
    const findRow = () => Array.from(document.querySelectorAll('.__menu-item[tabindex="0"]'))
      .find((row) => rowTitle(row) === NAME);
    const key = (target, k, code) => target.dispatchEvent(new KeyboardEvent('keydown', {{
      key: k, code: code, keyCode: code === 'ArrowDown' ? 40 : 13, which: code === 'ArrowDown' ? 40 : 13, bubbles: true, cancelable: true,
    }}));
    const t0 = Date.now();
    return new Promise((resolve) => {{
      const waitRow = () => {{
        const row = findRow();
        if (row) return highlight(row, 0);
        if (Date.now() - t0 > 4000) {{
          return resolve(JSON.stringify({{ ok: false, error: 'connector row not found in the mention menu' }}));
        }}
        setTimeout(waitRow, 80);
      }};
      const highlight = (row, steps) => {{
        if (row.getAttribute('data-highlighted') !== null) return commit();
        if (steps > 25) return resolve(JSON.stringify({{ ok: false, error: 'could not highlight the connector row' }}));
        key(ed, 'ArrowDown', 'ArrowDown');
        setTimeout(() => {{
          const again = findRow();
          if (!again) return resolve(JSON.stringify({{ ok: false, error: 'the mention menu closed before selection' }}));
          highlight(again, steps + 1);
        }}, 60);
      }};
      const commit = () => {{
        key(ed, 'Enter', 'Enter');
        const c0 = Date.now();
        const waitPill = () => {{
          if (pill().length === 1) return resolve(appendText());
          if (Date.now() - c0 > 5000) return resolve(JSON.stringify({{ ok: false, error: 'the connector pill did not appear after Enter' }}));
          setTimeout(waitPill, 80);
        }};
        waitPill();
      }};
      waitRow();
    }});
  }}"#,
        name = j(&connector_name),
        trigger = j(&trigger),
        text = j(&text),
    )
}

/// Clicks the tool-approval card for `connector_name` if one is showing.
///
/// `prefer_always` picks "Sempre permitir/Allow always" over the one-shot
/// button. Ported from `resolveChatGptToolConfirmation` + the spike's PT
/// button set. Returns `{found, clicked, button}`; benign absence is `found:
/// false`.
pub(crate) fn approval_script(connector_name: &str, prefer_always: bool) -> String {
    format!(
        r#"() => {{
    const NAME = {name};
    const PREFER_ALWAYS = {prefer_always};
    const cards = Array.from(document.querySelectorAll('[role="dialog"], [data-testid="tool-approval-card"]'))
      .filter((d) => (d.innerText || '').includes(NAME));
    if (!cards.length) return JSON.stringify({{ found: false }});
    const card = cards[cards.length - 1];
    const buttons = Array.from(card.querySelectorAll('button'));
    const byText = (re) => buttons.find((b) => re.test((b.innerText || '').trim()));
    const always = byText(/^(sempre permitir|allow always|always allow)$/i);
    const once = byText(/^(permitir uma vez|allow once|permitir)$/i);
    const target = (PREFER_ALWAYS && always) ? always : (once || always);
    if (!target) {{
      return JSON.stringify({{ found: true, clicked: false, buttons: buttons.map((b) => (b.innerText || '').trim()), text: (card.innerText || '').replace(/\s+/g, ' ').slice(0, 200) }});
    }}
    const r = target.getBoundingClientRect();
    const opts = {{ bubbles: true, cancelable: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2, button: 0 }};
    target.dispatchEvent(new MouseEvent('pointerdown', opts));
    target.dispatchEvent(new MouseEvent('pointerup', opts));
    target.dispatchEvent(new MouseEvent('click', opts));
    return JSON.stringify({{ found: true, clicked: true, button: (target.innerText || '').trim() }});
  }}"#,
        name = j(&connector_name),
        prefer_always = j(&prefer_always),
    )
}

/// Drives the browser-side attach on the tab bound to a conversation.
pub(crate) struct ConnectorAttach<'a> {
    pub(crate) daemon: &'a Arc<DaemonClient>,
    pub(crate) tabs: &'a Arc<TabPool>,
    pub(crate) connector_name: String,
    /// Prefer "Allow always" on the approval card.
    pub(crate) auto_always: bool,
}

impl ConnectorAttach<'_> {
    /// One approval pass on the tab bound to `conversation_id`; safe to call on
    /// a timer during the poll loop. Returns whether a card was clicked.
    pub(crate) async fn approve_on_conversation(&self, conversation_id: &str) -> bool {
        let name = self.connector_name.clone();
        let auto_always = self.auto_always;
        let daemon = Arc::clone(self.daemon);
        self.tabs
            .with_tab_for(Some(conversation_id), move |tab_id| async move {
                Ok::<bool, DriverError>(approve_once(&daemon, tab_id, &name, auto_always).await)
            })
            .await
            .unwrap_or(false)
    }
}

/// Runs the approval script once on a tab.
async fn approve_once(
    daemon: &Arc<DaemonClient>,
    tab_id: TabId,
    connector_name: &str,
    auto_always: bool,
) -> bool {
    match daemon
        .eval_in(
            tab_id,
            approval_script(connector_name, auto_always),
            APPROVAL_TIMEOUT_MS,
        )
        .await
    {
        Ok(value) => match serde_json::from_value::<ApprovalResult>(value) {
            Ok(result) => {
                if result.found && !result.clicked {
                    warn!(
                        "chatgpt_web connector: approval card found but no known button (buttons: {:?}; text: {:?})",
                        result.buttons, result.text
                    );
                }
                result.clicked
            }
            Err(_) => false,
        },
        Err(err) => {
            warn!("chatgpt_web connector: approval probe failed: {err}");
            false
        }
    }
}

#[cfg(test)]
#[path = "connector_attach_tests.rs"]
mod tests;
