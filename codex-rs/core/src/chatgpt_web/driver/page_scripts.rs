//! FORK: verbatim Rust port of `chatgpt-pro-mcp/src/page-scripts.ts`.
//!
//! Page-side scripts evaluated in the ChatGPT tab (MAIN world) via the
//! chrome-mcp daemon's `browser_eval`. Each function returns the JavaScript
//! source of a function expression; the daemon invokes it as
//! `(<expr>)(<args>)` and the result is decoded twice (the script always
//! resolves a JSON *string*, see `DaemonClient::eval_in`).
//!
//! HARD RULES (learned against the real injected runner):
//!  - NEVER use `async` functions — the runner returns {} for them. Build
//!    promise chains and resolve a JSON *string* (uniform contract with
//!    DaemonClient.evalIn, which parses it back).
//!  - The tab is usually a background/hidden tab: rAF never fires and DOM text
//!    of streamed messages may lag. Anything that must be current is read from
//!    the backend API, not the DOM.
//!  - Radix menus only mount their content while the tab is visible. Menu
//!    scripts are fallback paths and the caller activates the tab first.
//!
//! Selector map (verified live against chatgpt.com, 2026-08):
//!  - composer:        #prompt-textarea  (ProseMirror contenteditable inside a form)
//!  - send:            [data-testid="send-button"]
//!  - stop:            [data-testid="stop-button"]
//!  - attach ("+"):    [data-testid="composer-plus-btn"]
//!  - model/reasoning: 2nd form button[aria-haspopup=menu] (label = current level,
//!    e.g. "Pro"); menu has "Modelo"/"Model" and
//!    "Nível de raciocínio"/"Reasoning" submenus of menuitemradio.
//!  - messages:        [data-message-author-role] with data-message-id
//!
//! Porting rules: every JS body is a raw string literal kept byte-for-byte
//! from the TS template (after TS template-literal unescaping, so `\\s` in the
//! TS source is `\s` here). Parameters are interpolated ONLY through
//! [`j`] (= `serde_json::to_string`, the port of the TS `j = JSON.stringify`),
//! and inserted content is never re-scanned for placeholders.

// TODO(M4): consumed by `ops.rs`/`tabs.rs` and the provider once they land.
#![allow(dead_code)]

use serde_json::Map;
use serde_json::Value;

/// Port of `const j = (v) => JSON.stringify(v)`: the only way a parameter
/// reaches a script. `serde_json` never fails on these plain values (strings,
/// numbers, bools, JSON values), so a failure is reported as the JS literal
/// `null` rather than panicking inside a page script.
fn j<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Replace `@@NAME@@` placeholders in `template` in a single left-to-right
/// scan. Inserted values are never re-scanned, so a user-provided string that
/// happens to contain `@@X@@` cannot be expanded. Unknown placeholders are left
/// untouched (they would be a bug in the template and show up in tests).
fn fill(template: &str, params: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(start) = rest.find("@@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("@@") {
            Some(end) => {
                let name = &after[..end];
                match params.iter().find(|(n, _)| *n == name) {
                    Some((_, value)) => {
                        out.push_str(value);
                        rest = &after[end + 2..];
                    }
                    None => {
                        out.push_str("@@");
                        rest = after;
                    }
                }
            }
            None => {
                out.push_str("@@");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Wait until the composer exists (or a login wall is detected).
pub(super) fn wait_ready(timeout_ms: u64) -> String {
    fill(
        r#"() => new Promise((res) => {
    const t0 = Date.now();
    const iv = setInterval(() => {
      const ed = document.querySelector('#prompt-textarea');
      const form = ed ? ed.closest('form') : null;
      const login = !!document.querySelector('[data-testid*="login"], a[href*="/auth/login"]');
      if ((ed && form) || login || Date.now() - t0 > @@TIMEOUT_MS@@) {
        clearInterval(iv);
        res(JSON.stringify({
          ready: !!(ed && form),
          loginRequired: !(ed && form) && login,
          url: location.href,
          ms: Date.now() - t0,
        }));
      }
    }, 250);
  })"#,
        &[("TIMEOUT_MS", j(&timeout_ms))],
    )
}

/// Shape returned by [`composer_state`] (port of the TS `ComposerState`).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ComposerState {
    pub(crate) has_composer: bool,
    pub(crate) url: String,
    pub(crate) model_label: Option<String>,
    /// FORK: every pill in the composer, not just the model button. The effort
    /// level moved out of the model button into a pill of its own
    /// (`button.__composer-pill`), so checking only `model_label` reported a
    /// mismatch for a level that had in fact been applied.
    #[serde(default)]
    pub(crate) pills: Vec<String>,
    pub(crate) send_visible: bool,
    pub(crate) send_enabled: bool,
    pub(crate) generating: bool,
    pub(crate) attachments: u64,
    pub(crate) text: Option<String>,
}

/// One-shot view of the composer: current model label, send/stop, attachments.
pub(super) fn composer_state() -> String {
    r#"() => {
    const ed = document.querySelector('#prompt-textarea');
    const form = ed ? ed.closest('form') : null;
    const modelBtn = form
      ? form.querySelector('button[aria-haspopup="menu"]:not([data-testid="composer-plus-btn"]):not(#composer-plus-btn)')
      : null;
    const send = document.querySelector('[data-testid="send-button"]');
    const stop = document.querySelector('[data-testid="stop-button"]');
    // Attachment chips render above the textarea inside the same form; they are
    // the only images/cards there. Count generously across known variants
    // (group/file-tile is the current one, verified live 2026-08).
    let attachments = 0;
    if (form) {
      attachments = form.querySelectorAll(
        '[class*="group/file-tile"], [data-testid*="attachment"], [id^="textarea-file"], [class*="group/attachment"]'
      ).length;
      if (!attachments) attachments = form.querySelectorAll('img:not([alt=""])').length;
    }
    // FORK: the effort level now renders as its own composer pill.
    const pills = form
      ? Array.from(form.querySelectorAll('button.__composer-pill, [class*="composer-pill"]'))
          .map((el) => (el.textContent || '').replace(/\s+/g, ' ').trim())
          .filter((text) => text.length > 0)
      : [];
    return JSON.stringify({
      hasComposer: !!ed,
      url: location.href,
      modelLabel: modelBtn ? (modelBtn.textContent || '').trim() : null,
      pills,
      sendVisible: !!send,
      sendEnabled: !!send && !send.disabled,
      generating: !!stop,
      attachments,
      text: ed ? (ed.innerText || '').slice(0, 500) : null,
    });
  }"#
    .to_string()
}

/// Replace the composer content with `text` (paste event; execCommand fallback).
pub(super) fn set_composer_text(text: &str) -> String {
    fill(
        r#"() => {
    const TEXT = @@TEXT@@;
    const ed = document.querySelector('#prompt-textarea');
    if (!ed) return JSON.stringify({ ok: false, error: 'composer (#prompt-textarea) not found' });
    ed.focus();
    try { document.execCommand('selectAll', false); document.execCommand('delete', false); } catch (e) {}
    const dt = new DataTransfer();
    dt.setData('text/plain', TEXT);
    ed.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData: dt }));
    return new Promise((res) => setTimeout(res, 300)).then(() => {
      let got = (ed.innerText || '').trim();
      if (!got) {
        ed.focus();
        try {
          document.execCommand('selectAll', false);
          document.execCommand('delete', false);
          document.execCommand('insertText', false, TEXT);
        } catch (e) {}
      }
      return new Promise((r2) => setTimeout(r2, 150)).then(() => {
        got = (ed.innerText || '').trim();
        return JSON.stringify({ ok: got.length > 0, length: got.length });
      });
    });
  }"#,
        &[("TEXT", j(&text))],
    )
}

/// File tiles currently pending in the composer. Each tile carries the file
/// name in aria-label (including any "(1)" rename ChatGPT applies to
/// previously-uploaded names). `legacy` counts older chip variants so callers
/// can tell "no attachments" apart from "tile selector drifted".
pub(super) fn attachment_tiles() -> String {
    r#"() => {
    const ed = document.querySelector('#prompt-textarea');
    const form = ed ? ed.closest('form') : null;
    const tiles = form
      ? Array.from(form.querySelectorAll('[class*="group/file-tile"]')).map((t) => t.getAttribute('aria-label') || '')
      : [];
    let legacy = 0;
    if (form && !tiles.length) {
      legacy = form.querySelectorAll(
        '[data-testid*="attachment"], [id^="textarea-file"], [class*="group/attachment"]'
      ).length;
    }
    return JSON.stringify({ tiles, legacy });
  }"#
    .to_string()
}

/// Detect a blocking popup over the composer and dismiss it when unambiguous.
/// ChatGPT opens one after uploading a file it has seen before (matched by name
/// OR content, account-wide, pending uploads included): "Você já carregou este
/// arquivo." / "You've already uploaded this file." with a single OK button.
/// The file still attaches (renamed "name(N)" on name collisions) — except when
/// the identical content is already pending in the composer, which is silently
/// blocked. Only a single-button dialog is clicked; anything else is reported
/// back untouched so the caller can decide.
pub(super) fn dismiss_upload_dialog() -> String {
    r#"() => {
    const dialogs = Array.from(document.querySelectorAll('dialog, [role="dialog"]'))
      .filter((d) => (d.innerText || '').trim().length > 0);
    if (!dialogs.length) return JSON.stringify({ found: false });
    const d = dialogs[dialogs.length - 1];
    const text = (d.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 300);
    const buttons = Array.from(d.querySelectorAll('button')).filter((b) => (b.innerText || '').trim());
    if (buttons.length === 1) {
      buttons[0].click();
      return JSON.stringify({ found: true, text, dismissed: true });
    }
    return JSON.stringify({
      found: true, text, dismissed: false,
      buttons: buttons.map((b) => (b.innerText || '').trim()),
    });
  }"#
    .to_string()
}

/// Clear the composer (used to abort a half-typed message).
pub(super) fn clear_composer() -> String {
    r#"() => {
    const ed = document.querySelector('#prompt-textarea');
    if (!ed) return JSON.stringify({ ok: false, error: 'composer not found' });
    ed.focus();
    try { document.execCommand('selectAll', false); document.execCommand('delete', false); } catch (e) {}
    return JSON.stringify({ ok: (ed.textContent || '').trim() === '' });
  }"#
    .to_string()
}

/// Click send, then poll until the conversation id shows up in the URL (new
/// chats redirect to /c/<uuid> within ~2s). Reports whether generation started.
pub(super) fn click_send(timeout_ms: u64) -> String {
    fill(
        r#"() => {
    const send = document.querySelector('[data-testid="send-button"]');
    if (!send) return JSON.stringify({ ok: false, error: 'send button not found — composer empty or UI changed' });
    if (send.disabled) return JSON.stringify({ ok: false, error: 'send button disabled — attachment still uploading?' });
    const r = send.getBoundingClientRect();
    send.dispatchEvent(new MouseEvent('click', {
      bubbles: true, cancelable: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2, button: 0,
    }));
    const t0 = Date.now();
    let sawStop = false;
    return new Promise((res) => {
      const iv = setInterval(() => {
        if (document.querySelector('[data-testid="stop-button"]')) sawStop = true;
        const m = location.pathname.match(/\/c\/([0-9a-f-]{20,})/i);
        if ((m && sawStop) || Date.now() - t0 > @@TIMEOUT_MS@@) {
          clearInterval(iv);
          res(JSON.stringify({
            ok: !!m || sawStop,
            conversationId: m ? m[1] : null,
            generating: sawStop,
            url: location.href,
          }));
        }
      }, 300);
    });
  }"#,
        &[("TIMEOUT_MS", j(&timeout_ms))],
    )
}

/// Click the stop button and confirm it goes away.
pub(super) fn click_stop() -> String {
    r#"() => {
    const stop = document.querySelector('[data-testid="stop-button"]');
    if (!stop) return JSON.stringify({ ok: false, error: 'no stop button — nothing generating in this tab' });
    const r = stop.getBoundingClientRect();
    stop.dispatchEvent(new MouseEvent('click', {
      bubbles: true, cancelable: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2, button: 0,
    }));
    const t0 = Date.now();
    return new Promise((res) => {
      const iv = setInterval(() => {
        const still = !!document.querySelector('[data-testid="stop-button"]');
        if (!still || Date.now() - t0 > 8000) {
          clearInterval(iv);
          res(JSON.stringify({ ok: !still, stillGenerating: still }));
        }
      }, 300);
    });
  }"#
    .to_string()
}

/// Authenticated call to ChatGPT's own backend from the page context. The
/// access token comes from /api/auth/session (cookie-authed) and is cached on
/// window for 10 minutes.
///
/// `body == None` is the TS `body: null` (no `Content-Type`, no request body).
pub(super) fn api_call(path: &str, method: &str, body: Option<&Value>) -> String {
    api_call_template(path, method, body, None)
}

/// Same as [`api_call`] but merges `extra_headers` into the request headers
/// (after `Authorization`, so an explicit header wins). Needed by the connector
/// registry (Developer Mode endpoints). Keeps the `window.__cgptmcpTok` cache.
pub(crate) fn api_call_with_headers(
    path: &str,
    method: &str,
    body: Option<&Value>,
    extra_headers: &Map<String, Value>,
) -> String {
    api_call_template(path, method, body, Some(extra_headers))
}

fn api_call_template(
    path: &str,
    method: &str,
    body: Option<&Value>,
    extra_headers: Option<&Map<String, Value>>,
) -> String {
    // The verbatim TS builds `headers: { Authorization: 'Bearer ' + tok }`;
    // the headers variant wraps that same object in `Object.assign` with the
    // extra map so the token line stays identical.
    let (extra_decl, headers_init) = match extra_headers {
        None => (
            String::new(),
            "{ Authorization: 'Bearer ' + tok }".to_string(),
        ),
        Some(extra) => (
            format!("\n    const EXTRA = {};", j(&Value::Object(extra.clone()))),
            "Object.assign({ Authorization: 'Bearer ' + tok }, EXTRA)".to_string(),
        ),
    };
    fill(
        r#"() => {
    const PATH = @@PATH@@;
    const METHOD = @@METHOD@@;
    const BODY = @@BODY@@;@@EXTRA_DECL@@
    const now = Date.now();
    const cached = window.__cgptmcpTok;
    const getTok = (cached && cached.exp > now)
      ? Promise.resolve(cached.tok)
      : fetch('/api/auth/session', { credentials: 'include' })
          .then((r) => r.json())
          .then((s) => {
            if (!s || !s.accessToken) throw new Error('not logged in: /api/auth/session returned no accessToken');
            window.__cgptmcpTok = { tok: s.accessToken, exp: now + 600000 };
            return s.accessToken;
          });
    return getTok
      .then((tok) => {
        const init = { method: METHOD, headers: @@HEADERS_INIT@@ };
        if (BODY !== null) {
          init.headers['Content-Type'] = 'application/json';
          init.body = JSON.stringify(BODY);
        }
        return fetch(PATH, init);
      })
      .then((r) => Promise.all([r.status, r.headers.get('content-type') || '', r.text()]))
      .then(([status, ctype, text]) => {
        let json = null;
        if (ctype.indexOf('json') !== -1) { try { json = JSON.parse(text); } catch (e) {} }
        return JSON.stringify({ status, json, text: json === null ? text.slice(0, 4000) : null });
      })
      .catch((e) => JSON.stringify({ status: 0, error: String(e) }));
  }"#,
        &[
            ("PATH", j(&path)),
            ("METHOD", j(&method)),
            ("BODY", j(&body.unwrap_or(&Value::Null))),
            ("EXTRA_DECL", extra_decl),
            ("HEADERS_INIT", headers_init),
        ],
    )
}

/// Download a conversation asset INSIDE the page (the "signed" download URL is
/// on chatgpt.com and needs the browser's cookies — Node gets a 403) and cache
/// it on window as base64 for chunked pickup via [`read_download_chunk`].
pub(super) fn stage_download(file_id: &str) -> String {
    fill(
        r#"() => {
    const FID = @@FID@@;
    const now = Date.now();
    const cached = window.__cgptmcpTok;
    const getTok = (cached && cached.exp > now)
      ? Promise.resolve(cached.tok)
      : fetch('/api/auth/session', { credentials: 'include' })
          .then((r) => r.json())
          .then((s) => {
            if (!s || !s.accessToken) throw new Error('not logged in');
            window.__cgptmcpTok = { tok: s.accessToken, exp: now + 600000 };
            return s.accessToken;
          });
    return getTok
      .then((tok) => fetch('/backend-api/files/' + encodeURIComponent(FID) + '/download', {
        headers: { Authorization: 'Bearer ' + tok },
      }))
      .then((r) => { if (!r.ok) throw new Error('download endpoint HTTP ' + r.status); return r.json(); })
      .then((jj) => {
        const url = jj.download_url || jj.downloadUrl || jj.url;
        if (!url) throw new Error('no download_url in response: ' + JSON.stringify(jj).slice(0, 200));
        return fetch(url, { credentials: 'include' });
      })
      .then((r) => {
        if (!r.ok) throw new Error('file fetch HTTP ' + r.status);
        const mime = r.headers.get('content-type') || 'application/octet-stream';
        const cd = r.headers.get('content-disposition') || '';
        return r.blob().then((blob) => new Promise((res, rej) => {
          const fr = new FileReader();
          fr.onload = () => res({ dataUrl: fr.result, mime, cd, size: blob.size });
          fr.onerror = () => rej(new Error('FileReader failed'));
          fr.readAsDataURL(blob);
        }));
      })
      .then((out) => {
        const b64 = String(out.dataUrl).replace(/^data:[^,]*,/, '');
        // Keyed by file id so overlapping downloads never clobber each other.
        window.__cgptmcpDl = window.__cgptmcpDl || {};
        window.__cgptmcpDl[FID] = b64;
        return JSON.stringify({ ok: true, size: out.size, b64len: b64.length, mime: out.mime, cd: out.cd });
      })
      .catch((e) => JSON.stringify({ ok: false, error: String(e) }));
  }"#,
        &[("FID", j(&file_id))],
    )
}

/// Read a slice of the staged base64; pass `done=true` on the last call to free it.
pub(super) fn read_download_chunk(file_id: &str, offset: u64, length: u64, done: bool) -> String {
    fill(
        r#"() => {
    const cache = window.__cgptmcpDl || {};
    const b64 = cache[@@FID@@];
    if (typeof b64 !== 'string') return JSON.stringify({ ok: false, error: 'no staged download for this file (stage it first)' });
    const chunk = b64.slice(@@OFFSET@@, @@END@@);
    if (@@DONE@@) delete cache[@@FID@@];
    return JSON.stringify({ ok: true, chunk });
  }"#,
        &[
            ("FID", j(&file_id)),
            ("OFFSET", j(&offset)),
            ("END", j(&offset.saturating_add(length))),
            ("DONE", j(&done)),
        ],
    )
}

/// DOM fallback reader for when the backend API misbehaves.
pub(super) fn dom_turns(max_turns: u64) -> String {
    fill(
        r#"() => {
    const nodes = Array.from(document.querySelectorAll('[data-message-author-role]'));
    const turns = nodes.map((m) => ({
      role: m.getAttribute('data-message-author-role'),
      id: m.getAttribute('data-message-id'),
      model: m.getAttribute('data-message-model-slug') || undefined,
      text: (m.innerText || '').trim().slice(0, 8000),
      images: Array.from(m.querySelectorAll('img'))
        .map((i) => i.src)
        .filter((s) => s && s.indexOf('data:') !== 0),
    }));
    return JSON.stringify({ count: turns.length, turns: turns.slice(-@@MAX_TURNS@@) });
  }"#,
        &[("MAX_TURNS", j(&max_turns))],
    )
}

/// Shape returned by [`dom_progress`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct DomProgress {
    pub(crate) url: String,
    /// The stop button is showing.
    pub(crate) generating: bool,
    /// Elements still carrying `data-streaming-response-status`.
    pub(crate) streaming: u64,
    /// First 120 chars (trimmed) of the last user message on the page.
    pub(crate) last_user_text: String,
    /// Assistant turns rendered after the last user message.
    pub(crate) assistant_turns: u64,
    /// Total rendered characters of those assistant turns.
    pub(crate) assistant_chars: u64,
    /// The last assistant turn has its copy button (ChatGPT renders it only
    /// once the message finished).
    pub(crate) last_assistant_done: bool,
    pub(crate) last_assistant_id: Option<String>,
}

/// FORK (verified live): progress of the current reply read from the DOM —
/// no backend request at all.
///
/// `GET /backend-api/conversation/<id>` is rate limited per account, so the
/// poll loop watches the page instead and only reads the API when the page
/// says the reply finished (or on a slow safety cadence). The rendered text is
/// used for change detection only: `innerText` loses the markdown the API
/// returns, so it is never emitted as answer text.
pub(super) fn dom_progress() -> String {
    r#"() => {
    const msgs = Array.from(document.querySelectorAll('[data-message-author-role]'));
    let lastUser = -1;
    msgs.forEach((m, i) => { if (m.getAttribute('data-message-author-role') === 'user') lastUser = i; });
    const after = lastUser >= 0 ? msgs.slice(lastUser + 1) : [];
    const assistants = after.filter((m) => m.getAttribute('data-message-author-role') !== 'user');
    const last = assistants.length ? assistants[assistants.length - 1] : null;
    const turnOf = (m) => m.closest('[data-turn-id]') || m.closest('article') || m;
    const lastUserText = lastUser >= 0 ? (msgs[lastUser].innerText || '').trim().slice(0, 240) : '';
    let chars = 0;
    assistants.forEach((m) => { chars += (m.innerText || '').length; });
    return JSON.stringify({
      url: location.href,
      generating: !!document.querySelector('[data-testid="stop-button"]'),
      streaming: document.querySelectorAll('[data-streaming-response-status]').length,
      lastUserText,
      assistantTurns: assistants.length,
      assistantChars: chars,
      lastAssistantDone: !!(last && turnOf(last).querySelector('[data-testid="copy-turn-action-button"]')),
      lastAssistantId: last ? last.getAttribute('data-message-id') : null,
    });
  }"#
    .to_string()
}

/// Open the composer model menu and read both submenus (model + reasoning
/// level). Requires a VISIBLE tab. Leaves stale menu overlays behind — the
/// caller reloads the tab afterwards.
pub(super) fn menu_discover() -> String {
    r#"() => {
    const synthClick = (el) => {
      const r = el.getBoundingClientRect();
      const o = { bubbles: true, cancelable: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2, button: 0, pointerId: 1, isPrimary: true };
      el.dispatchEvent(new PointerEvent('pointerdown', o));
      el.dispatchEvent(new PointerEvent('pointerup', o));
      el.dispatchEvent(new MouseEvent('click', o));
    };
    const wait = (pred, ms) => new Promise((res) => {
      const t0 = Date.now();
      const iv = setInterval(() => {
        const v = pred();
        if (v || Date.now() - t0 > ms) { clearInterval(iv); res(v || null); }
      }, 150);
    });
    const itemsOf = (menu) => Array.from(menu.querySelectorAll('[role="menuitemradio"]')).map((it) => ({
      label: (it.textContent || '').trim(),
      checked: it.getAttribute('aria-checked') === 'true',
    }));
    const form = document.querySelector('#prompt-textarea') ? document.querySelector('#prompt-textarea').closest('form') : null;
    const trigger = form ? form.querySelector('button[aria-haspopup="menu"]:not([data-testid="composer-plus-btn"]):not(#composer-plus-btn)') : null;
    if (!trigger) return JSON.stringify({ ok: false, error: 'model menu trigger not found in composer form' });
    const seen = new Set(Array.from(document.querySelectorAll('[role="menu"]')).map((m) => m.id));
    synthClick(trigger);
    return wait(() => {
      const menus = Array.from(document.querySelectorAll('[role="menu"]')).filter((m) => !seen.has(m.id));
      return menus.find((m) => m.querySelectorAll('[role^="menuitem"]').length > 0) || null;
    }, 4000).then((root) => {
      if (!root) return JSON.stringify({ ok: false, error: 'menu content did not mount (tab must be visible)', visibility: document.visibilityState });
      // FORK: the level picker is a slider now. Walk it end to end and record
      // `<label>, <n> de <total>` for every stop — that table is what the
      // ordinal selection is written against, and its labels keep moving.
      const sliderItem = () => root.querySelector('[role="menuitem"][aria-keyshortcuts]');
      if (sliderItem()) {
        const stateOf = () => {
          const it = sliderItem();
          const text = it && it.parentElement ? (it.parentElement.innerText || '') : '';
          const flat = text.replace(/\s+/g, ' ').trim();
          const m = flat.match(/,\s*(\d+)\s*(?:de|of)\s*(\d+)/);
          return {
            label: flat.split(',')[0].trim(),
            index: m ? parseInt(m[1], 10) : null,
            total: m ? parseInt(m[2], 10) : null,
          };
        };
        const press = (k) => {
          const it = sliderItem();
          if (!it) return false;
          it.focus();
          const o = { key: k, code: k, bubbles: true, cancelable: true };
          it.dispatchEvent(new KeyboardEvent('keydown', o));
          it.dispatchEvent(new KeyboardEvent('keyup', o));
          return true;
        };
        const settle = () => new Promise((r) => setTimeout(r, 250));
        const current = stateOf();
        const levels = [];
        const record = (state) => {
          if (state.index !== null && !levels.some((l) => l.index === state.index)) {
            levels.push({ index: state.index, label: state.label });
          }
        };
        // Left wall first, then right, so the walk covers every stop once.
        const walk = (k, steps) => {
          const state = stateOf();
          record(state);
          if (steps >= 12 || !press(k)) return Promise.resolve(state);
          return settle().then(() => {
            const next = stateOf();
            if (next.index === state.index && next.label === state.label) {
              return Promise.resolve(next);
            }
            return walk(k, steps + 1);
          });
        };
        return walk('ArrowLeft', 0)
          .then(() => walk('ArrowRight', 0))
          .then(() => {
            levels.sort((a, b) => a.index - b.index);
            return JSON.stringify({
              ok: true,
              slider: true,
              triggerLabel: (trigger.textContent || '').trim(),
              current: current,
              levels: levels,
              models: null,
            });
          });
      }
      const subs = Array.from(root.querySelectorAll('[role="menuitem"][aria-haspopup="menu"]')).map((it) => (it.textContent || '').trim());
      const openSub = (re) => {
        const it = Array.from(root.querySelectorAll('[role="menuitem"][aria-haspopup="menu"]'))
          .find((x) => re.test((x.textContent || '').trim()));
        if (!it) return Promise.resolve(null);
        const before = new Set(Array.from(document.querySelectorAll('[role="menu"]')).map((m) => m.id));
        synthClick(it);
        return wait(() => {
          const menus = Array.from(document.querySelectorAll('[role="menu"]')).filter((m) => !before.has(m.id));
          return menus.find((m) => m.querySelectorAll('[role="menuitemradio"]').length > 0) || null;
        }, 4000);
      };
      return openSub(/N[íi]vel de racioc[íi]nio|Reasoning/i).then((levelMenu) =>
        openSub(/^Modelo|^Model/i).then((modelMenu) => JSON.stringify({
          ok: true,
          triggerLabel: (trigger.textContent || '').trim(),
          submenus: subs,
          levels: levelMenu ? itemsOf(levelMenu) : null,
          models: modelMenu ? itemsOf(modelMenu) : null,
        })),
      );
    });
  }"#
    .to_string()
}

/// Which submenu [`menu_select`] opens (port of the TS `kind: 'level' | 'model'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuKind {
    Level,
    Model,
}

impl MenuKind {
    /// Port of `const subRe = kind === "level" ? "N[íi]vel de racioc[íi]nio|Reasoning" : "^Modelo|^Model"`.
    fn submenu_regex_source(self) -> &'static str {
        match self {
            Self::Level => "N[íi]vel de racioc[íi]nio|Reasoning",
            Self::Model => "^Modelo|^Model",
        }
    }
}

/// Select a reasoning level or model through the composer menu.
///
/// FORK: the level picker is a slider, and its labels move — on 04/09 the five
/// stops read "Leve … Alta" rather than "Instantâneo … Alto", and every
/// `medium|high|extra-high` request fell through to whatever the account had
/// last used, silently. The accessible text of the slider is
/// `"<label>, <n> de 5."`, so `level_index` (1-based) is the instruction and
/// the label is only how the result is reported back. Without an index the
/// script still walks by label, which is what the model submenu needs.
pub(super) fn menu_select(
    kind: MenuKind,
    label_regex_source: &str,
    level_index: Option<u32>,
) -> String {
    fill(
        r#"() => {
    const TARGET = new RegExp(@@TARGET@@, 'i');
    const SUB = new RegExp(@@SUB@@, 'i');
    const INDEX = @@INDEX@@;
    const synthClick = (el) => {
      const r = el.getBoundingClientRect();
      const o = { bubbles: true, cancelable: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2, button: 0, pointerId: 1, isPrimary: true };
      el.dispatchEvent(new PointerEvent('pointerdown', o));
      el.dispatchEvent(new PointerEvent('pointerup', o));
      el.dispatchEvent(new MouseEvent('click', o));
    };
    const wait = (pred, ms) => new Promise((res) => {
      const t0 = Date.now();
      const iv = setInterval(() => {
        const v = pred();
        if (v || Date.now() - t0 > ms) { clearInterval(iv); res(v || null); }
      }, 150);
    });
    const composerForm = () => {
      const ed = document.querySelector('#prompt-textarea');
      return ed ? ed.closest('form') : null;
    };
    // Every menu trigger in the composer, minus the "+" button. Which of them
    // owns the level picker has changed before, so try each in turn rather
    // than pinning the first.
    const triggers = () => {
      const form = composerForm();
      if (!form) return [];
      return Array.from(form.querySelectorAll('button[aria-haspopup="menu"]'))
        .filter((t) => t.dataset.testid !== 'composer-plus-btn' && t.id !== 'composer-plus-btn')
        // The picker mounts a beat after the composer and renders its label
        // later still; an unlabeled trigger opens an empty menu.
        .filter((t) => (t.textContent || '').trim().length > 0);
    };
    const openMenu = (trigger) => {
      const seen = new Set(Array.from(document.querySelectorAll('[role="menu"]')).map((m) => m.id));
      synthClick(trigger);
      return wait(() => {
        const menus = Array.from(document.querySelectorAll('[role="menu"]')).filter((m) => !seen.has(m.id));
        return menus.find((m) => m.querySelectorAll('[role^="menuitem"]').length > 0) || null;
      }, 4000);
    };
    const closeMenu = () => {
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', code: 'Escape', bubbles: true }));
      return new Promise((r) => setTimeout(r, 200));
    };
    const sliderItemIn = (root) => root.querySelector('[role="menuitem"][aria-keyshortcuts]');
    const usable = (root) => !!(sliderItemIn(root) || root.querySelector('[role="menuitemradio"]'));

    // FORK (verified live): right after a navigation the trigger is not there
    // yet; failing at once inherited whatever level the account last used.
    return wait(() => (triggers().length ? triggers() : null), 6000).then((found) => {
    if (!found) return JSON.stringify({ ok: false, error: 'model menu trigger not found' });
    const tryTrigger = (rest) => {
      if (!rest.length) return Promise.resolve(null);
      const trigger = rest[0];
      return openMenu(trigger).then((root) => {
        if (root && usable(root)) return { trigger: trigger, root: root };
        return closeMenu().then(() => tryTrigger(rest.slice(1)));
      });
    };
    return tryTrigger(found.slice(0, 3)).then((opened) => {
      if (!opened) return JSON.stringify({ ok: false, error: 'menu did not mount (tab must be visible)', visibility: document.visibilityState });
      const trigger = opened.trigger;
      const root = opened.root;
      // FORK (verified live 2026-08-27, labels re-verified 2026-09-04): the
      // picker is a slider (`data-animated-slider-trigger`): one `menuitem`
      // with `aria-keyshortcuts="ArrowLeft ArrowRight"` whose sibling text
      // reads "<label>, <n> de 5.". Synthetic Arrow keydowns move it and the
      // trigger label follows; the selection survives the caller's reload.
      if (sliderItemIn(root)) {
        // "Alta, 3 de 5." -> { label: 'Alta', index: 3, total: 5 }
        const stateOf = () => {
          const it = sliderItemIn(root);
          const text = it && it.parentElement ? (it.parentElement.innerText || '') : '';
          const flat = text.replace(/\s+/g, ' ').trim();
          const m = flat.match(/,\s*(\d+)\s*(?:de|of)\s*(\d+)/);
          return {
            label: flat.split(',')[0].trim(),
            index: m ? parseInt(m[1], 10) : null,
            total: m ? parseInt(m[2], 10) : null,
          };
        };
        const press = (k) => {
          const it = sliderItemIn(root);
          if (!it) return false;
          it.focus();
          const o = { key: k, code: k, bubbles: true, cancelable: true };
          it.dispatchEvent(new KeyboardEvent('keydown', o));
          it.dispatchEvent(new KeyboardEvent('keyup', o));
          return true;
        };
        const settle = () => new Promise((r) => setTimeout(r, 250));
        const done = (state) => settle().then(() => JSON.stringify({
          ok: true,
          selected: state.label,
          index: state.index,
          total: state.total,
          labelMatched: TARGET.test(state.label),
          triggerLabel: (trigger.textContent || '').trim(),
          slider: true,
        }));
        const seenLabels = [];
        const remember = (state) => {
          const entry = state.index ? state.label + ' (' + state.index + ')' : state.label;
          if (seenLabels.indexOf(entry) < 0) seenLabels.push(entry);
        };
        // Ordinal navigation: the stable instruction. Step |INDEX - current|
        // times in the right direction and stop.
        const byIndex = (steps) => {
          const state = stateOf();
          remember(state);
          if (state.index === INDEX) return done(state);
          if (steps >= 12 || state.index === null) return byLabel('ArrowLeft', 0);
          const key = state.index < INDEX ? 'ArrowRight' : 'ArrowLeft';
          if (!press(key)) return Promise.resolve(JSON.stringify({ ok: false, error: 'slider item vanished', available: seenLabels, slider: true }));
          return settle().then(() => {
            const next = stateOf();
            // Wall at either end: the slider has fewer stops than we thought.
            if (next.index === state.index) return Promise.resolve(JSON.stringify({ ok: false, error: 'slider will not move past ' + state.index, available: seenLabels, slider: true }));
            return byIndex(steps + 1);
          });
        };
        // Fallback for a slider that does not announce its position: walk by
        // label, the way this worked before the ordinal was available.
        const byLabel = (k, steps) => {
          const state = stateOf();
          remember(state);
          if (TARGET.test(state.label)) return done(state);
          if (steps >= 12) return Promise.resolve(JSON.stringify({ ok: false, error: 'option not found', available: seenLabels, slider: true }));
          if (!press(k)) return Promise.resolve(JSON.stringify({ ok: false, error: 'slider item vanished', available: seenLabels, slider: true }));
          return settle().then(() => {
            const next = stateOf();
            if (next.label === state.label && k === 'ArrowLeft') return byLabel('ArrowRight', steps + 1);
            if (next.label === state.label && k === 'ArrowRight') return Promise.resolve(JSON.stringify({ ok: false, error: 'option not found', available: seenLabels, slider: true }));
            return byLabel(k, steps + 1);
          });
        };
        return INDEX === null ? byLabel('ArrowLeft', 0) : byIndex(0);
      }
      const sub = Array.from(root.querySelectorAll('[role="menuitem"][aria-haspopup="menu"]'))
        .find((x) => SUB.test((x.textContent || '').trim()));
      if (!sub) return JSON.stringify({
        ok: false,
        error: 'submenu not found',
        available: Array.from(root.querySelectorAll('[role="menuitem"]')).map((x) => (x.textContent || '').trim()),
      });
      const before = new Set(Array.from(document.querySelectorAll('[role="menu"]')).map((m) => m.id));
      synthClick(sub);
      return wait(() => {
        const menus = Array.from(document.querySelectorAll('[role="menu"]')).filter((m) => !before.has(m.id));
        return menus.find((m) => m.querySelectorAll('[role="menuitemradio"]').length > 0) || null;
      }, 4000).then((subMenu) => {
        if (!subMenu) return JSON.stringify({ ok: false, error: 'submenu content did not mount' });
        const options = Array.from(subMenu.querySelectorAll('[role="menuitemradio"]'));
        const target = options.find((o) => TARGET.test((o.textContent || '').trim()));
        if (!target) return JSON.stringify({
          ok: false,
          error: 'option not found',
          available: options.map((o) => (o.textContent || '').trim()),
        });
        synthClick(target);
        return new Promise((r) => setTimeout(r, 600)).then(() => JSON.stringify({
          ok: true,
          selected: (target.textContent || '').trim(),
          labelMatched: true,
          triggerLabel: (trigger.textContent || '').trim(),
        }));
      });
    });
    });
  }"#,
        &[
            ("TARGET", j(&label_regex_source)),
            ("SUB", j(&kind.submenu_regex_source())),
            (
                "INDEX",
                level_index.map_or_else(|| "null".to_string(), |index| index.to_string()),
            ),
        ],
    )
}

#[cfg(test)]
#[path = "page_scripts_tests.rs"]
mod tests;
