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

/// Outcome of one approval-card pass in the page.
///
/// `confirmed` is intentionally separate from `clicked`: a browser-side
/// click is only an attempt. The connector loop may advance after a result
/// marked `confirmed`, which requires the card's disappearance (or another
/// explicit closed-state postcondition).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApprovalState {
    #[default]
    Missing,
    Pending,
    Rerendered,
    Confirmed,
    Unsupported,
}

/// Result of the approval-card script.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ApprovalResult {
    pub(crate) found: bool,
    /// Whether this pass invoked the button's native `click()` method.
    pub(crate) clicked: bool,
    /// Whether a real postcondition was observed after the click.
    pub(crate) confirmed: bool,
    pub(crate) state: ApprovalState,
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

/// Approves the tool-approval card for `connector_name` if one is showing.
///
/// `prefer_always` picks "Sempre permitir/Allow always" over the one-shot
/// button. The script uses the button's native `click()` method and waits for
/// the card to disappear before reporting `confirmed`; dispatching synthetic
/// mouse events alone is not a proof that ChatGPT accepted the approval.
pub(crate) fn approval_script(connector_name: &str, prefer_always: bool) -> String {
    format!(
        r#"() => {{
    const NAME = {name};
    const PREFER_ALWAYS = {prefer_always};
    const SELECTOR = '[role="dialog"], [data-testid="tool-approval-card"]';
    const ABSENCE_STABLE_MS = 300;
    const visible = (node) => {{
      if (!node || !node.isConnected) return false;
      const style = window.getComputedStyle(node);
      return style.display !== 'none' && style.visibility !== 'hidden' && node.getClientRects().length > 0;
    }};
    const cards = () => Array.from(document.querySelectorAll(SELECTOR))
      .filter((d) => visible(d) && (d.innerText || '').includes(NAME));
    const details = (card, state, clicked, confirmed, button) => JSON.stringify({{
      found: true,
      clicked,
      confirmed,
      state,
      button: button || null,
      buttons: Array.from(card.querySelectorAll('button')).map((b) => (b.innerText || '').replace(/\s+/g, ' ').trim()),
      text: (card.innerText || '').replace(/\s+/g, ' ').slice(0, 200),
    }});
    const current = cards();
    if (!current.length) return JSON.stringify({{ found: false, clicked: false, confirmed: false, state: 'missing' }});
    const card = current[current.length - 1];
    const buttons = Array.from(card.querySelectorAll('button'));
    const byText = (re) => buttons.find((b) => visible(b) && re.test((b.innerText || '').replace(/\s+/g, ' ').trim()));
    const enabled = (button) => button && !button.disabled && button.getAttribute('aria-disabled') !== 'true';
    const alwaysCandidate = byText(/^(sempre permitir|allow always|always allow)$/i);
    const onceCandidate = byText(/^(permitir uma vez|allow once|permitir)$/i);
    const always = enabled(alwaysCandidate) ? alwaysCandidate : null;
    const once = enabled(onceCandidate) ? onceCandidate : null;
    const target = (PREFER_ALWAYS && always) ? always : (once || always);
    if (!target) {{
      return details(card, (alwaysCandidate || onceCandidate) ? 'pending' : 'unsupported', false, false, null);
    }}
    const previousAttempt = Number(card.dataset.codexApprovalAttemptAt || 0);
    if (previousAttempt && Date.now() - previousAttempt < 2000) {{
      return details(card, 'pending', false, false, (target.innerText || '').replace(/\s+/g, ' ').trim());
    }}
    card.dataset.codexApprovalAttemptAt = String(Date.now());
    const label = (target.innerText || '').replace(/\s+/g, ' ').trim();
    try {{
      target.focus();
      target.click();
    }} catch (e) {{
      return details(card, 'pending', false, false, label);
    }}
    const started = Date.now();
    let absentSince = null;
    return new Promise((resolve) => {{
      const check = () => {{
        const live = cards();
        if (!live.length) {{
          if (absentSince === null) absentSince = Date.now();
          if (Date.now() - absentSince >= ABSENCE_STABLE_MS) return resolve(JSON.stringify({{ found: true, clicked: true, confirmed: true, state: 'confirmed', button: label }}));
        }} else {{
          absentSince = null;
          if (!card.isConnected || live[live.length - 1] !== card) return resolve(details(live[live.length - 1], 'rerendered', true, false, label));
        }}
        if (Date.now() - started >= 1500) return resolve(details(card, 'pending', true, false, label));
        setTimeout(check, 100);
      }};
      check();
    }});
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
                match result.state {
                    ApprovalState::Unsupported => warn!(
                        "chatgpt_web connector: approval card found but no known button (buttons: {:?}; text: {:?})",
                        result.buttons, result.text
                    ),
                    ApprovalState::Pending | ApprovalState::Rerendered => {
                        tracing::debug!(
                            state = ?result.state,
                            clicked = result.clicked,
                            "chatgpt_web connector: approval card still pending"
                        );
                    }
                    ApprovalState::Missing | ApprovalState::Confirmed => {}
                }
                result.confirmed
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
