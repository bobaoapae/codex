//! FORK: port of `chatgpt-pro-mcp/src/api.ts`.
//!
//! ChatGPT backend API, called from inside the page (cookie/session auth stays
//! in Chrome; nothing is exfiltrated beyond the returned JSON). This is the
//! read path — sending messages always goes through the real UI (`ops.rs`).
//!
//! Three layers live here:
//! - the `#[serde(default)]` raw types of `/backend-api/conversation/<id>`;
//! - [`normalize`], the port of `normalizeConversation` (`api.ts:182–280`) plus
//!   the `api_tool_requests` rule borrowed from chat-on-steroids;
//! - [`ChatGptApi`], the port of the `ChatGptApi` class, which only depends on
//!   the [`PageEval`] trait (implemented by the daemon client) and on
//!   `page_scripts::api_call`.

// TODO(M4): consumed by `ops.rs`, `stream.rs` and the provider once they land.
#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::info;

use super::DriverError;
use super::DriverErrorKind;
use super::DriverResult;
use super::page_scripts;
use super::tabs::TabId;

/// `api.ts`: `const backoff = [2000, 5000, 10_000];`
pub(crate) const RATE_LIMIT_BACKOFF: [Duration; 3] = [
    Duration::from_millis(2000),
    Duration::from_millis(5000),
    Duration::from_millis(10_000),
];

/// `api.ts`: `Date.now() - this.modelsCache.at < 300_000`.
pub(crate) const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);

/// The TS `evalIn` passes no timeout for API calls, so the daemon default
/// (30s) applies. Mirrored explicitly because `PageEval::eval` takes one.
pub(crate) const API_EVAL_TIMEOUT_MS: u64 = 30_000;

/// Deserialize JSON `null` (and a missing key, via `#[serde(default)]`) as
/// `T::default()`. The backend sends `null` for many optional fields
/// (`metadata`, `parts`, `title`, ...), and `#[serde(default)]` alone only
/// covers absent keys.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

// ---------------------------------------------------------------------------
// Raw wire types (`/backend-api/conversation/<id>`)
// ---------------------------------------------------------------------------

/// Top-level payload of `GET /backend-api/conversation/<id>`. Unknown fields
/// are tolerated everywhere (no `deny_unknown_fields`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawConversation {
    #[serde(deserialize_with = "null_default")]
    pub(crate) title: String,
    /// Present on the conversation endpoint.
    pub(crate) conversation_id: Option<String>,
    /// Some responses (and the list endpoint) carry `id` instead.
    pub(crate) id: Option<String>,
    pub(crate) create_time: Option<f64>,
    pub(crate) update_time: Option<f64>,
    #[serde(deserialize_with = "null_default")]
    pub(crate) mapping: HashMap<String, RawNode>,
    pub(crate) current_node: Option<String>,
    pub(crate) default_model_slug: Option<String>,
    /// `number | null` on the wire; anything non-numeric is treated as `null`.
    pub(crate) async_status: Option<Value>,
    pub(crate) is_archived: Option<bool>,
    pub(crate) is_starred: Option<bool>,
    pub(crate) gizmo_id: Option<String>,
}

impl RawConversation {
    /// The conversation id, whichever key carried it.
    pub(crate) fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.as_deref().or(self.id.as_deref())
    }
}

/// One entry of `mapping`, keyed by its own `id`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawNode {
    #[serde(deserialize_with = "null_default")]
    pub(crate) id: String,
    pub(crate) parent: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub(crate) children: Vec<String>,
    pub(crate) message: Option<RawMessage>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawMessage {
    #[serde(deserialize_with = "null_default")]
    pub(crate) id: String,
    #[serde(deserialize_with = "null_default")]
    pub(crate) author: RawAuthor,
    pub(crate) create_time: Option<f64>,
    pub(crate) update_time: Option<f64>,
    #[serde(deserialize_with = "null_default")]
    pub(crate) content: RawContent,
    #[serde(deserialize_with = "null_default")]
    pub(crate) status: String,
    pub(crate) end_turn: Option<bool>,
    pub(crate) weight: Option<f64>,
    pub(crate) recipient: Option<String>,
    /// `analysis` (thoughts), `commentary` (tool traffic), `final` (user-facing).
    pub(crate) channel: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub(crate) metadata: RawMetadata,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawAuthor {
    #[serde(deserialize_with = "null_default")]
    pub(crate) role: String,
    /// Tool name on `role: "tool"` messages (e.g. `t2uay3k.sj1i4kz` for image gen).
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawContent {
    /// `text` | `thoughts` | `multimodal_text` | `code` | `user_editable_context` | ...
    #[serde(deserialize_with = "null_default")]
    pub(crate) content_type: String,
    /// Strings and/or asset-pointer objects (`multimodal_text`). `None` when
    /// absent or `null` (`code`/`thoughts` messages), which is what the TS
    /// `Array.isArray(m.content.parts)` distinguishes from `[]`.
    pub(crate) parts: Option<Vec<Value>>,
    /// `code` / `reasoning_recap` style payloads.
    pub(crate) text: Option<String>,
    /// `content_type: "thoughts"`.
    #[serde(deserialize_with = "null_default")]
    pub(crate) thoughts: Vec<RawThought>,
    pub(crate) language: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawThought {
    pub(crate) summary: Option<String>,
    pub(crate) content: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawMetadata {
    /// Links a tool result back to the request it answers.
    pub(crate) parent_id: Option<String>,
    /// Connector request id (`wfr_...`), stamped on both request and result.
    pub(crate) request_id: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub(crate) attachments: Vec<RawAttachment>,
    pub(crate) model_slug: Option<String>,
    pub(crate) default_model_slug: Option<String>,
    pub(crate) is_visually_hidden_from_conversation: Option<bool>,
    pub(crate) async_task_type: Option<String>,
    pub(crate) async_task_id: Option<String>,
    pub(crate) reasoning_status: Option<String>,
    pub(crate) finish_details: Option<Value>,
    pub(crate) invoked_plugin: Option<Value>,
    pub(crate) invoked_resource: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawAttachment {
    #[serde(deserialize_with = "null_default")]
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) mime_type: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) width: Option<u64>,
    pub(crate) height: Option<u64>,
}

// ---------------------------------------------------------------------------
// Normalized types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AssetKind {
    Image,
    File,
}

/// Port of the TS `Asset`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Asset {
    pub(crate) file_id: String,
    pub(crate) kind: AssetKind,
    pub(crate) name: Option<String>,
    pub(crate) width: Option<u64>,
    pub(crate) height: Option<u64>,
    pub(crate) size_bytes: Option<u64>,
}

/// One reasoning block of a `thoughts` message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Thought {
    pub(crate) summary: Option<String>,
    pub(crate) content: Option<String>,
}

/// Port of the TS `Turn`, widened with the fields the provider needs
/// (`thoughts`, `recipient`, `parent_id`, node `id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Turn {
    /// Mapping node id (equals `message_id` on real captures).
    pub(crate) id: String,
    /// `user` | `assistant` | `assistant-thoughts` | `tool`.
    pub(crate) role: String,
    pub(crate) message_id: String,
    pub(crate) content_type: String,
    pub(crate) text: String,
    /// Non-empty only for `assistant-thoughts` turns.
    pub(crate) thoughts: Vec<Thought>,
    pub(crate) status: String,
    pub(crate) end_turn: Option<bool>,
    pub(crate) recipient: Option<String>,
    pub(crate) model_slug: Option<String>,
    pub(crate) create_time: Option<f64>,
    pub(crate) assets: Vec<Asset>,
    /// `metadata.parent_id` (tool results point at their request).
    pub(crate) parent_id: Option<String>,
}

/// An assistant message addressed to a connector (`recipient` starts with
/// `api_tool`) and whether a tool result answering it has landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ApiToolRequest {
    pub(crate) message_id: String,
    /// `metadata.request_id` (`wfr_...`), also visible on the MCP request.
    pub(crate) request_id: Option<String>,
    pub(crate) recipient: String,
    pub(crate) has_result: bool,
}

/// Port of the TS `NormalizedConversation` plus the provider extras.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Conversation {
    pub(crate) id: String,
    pub(crate) title: String,
    /// `default_model_slug`.
    pub(crate) model: Option<String>,
    pub(crate) current_node: Option<String>,
    /// Some node at/after the last real user message is `in_progress`.
    pub(crate) any_in_progress: bool,
    /// TS `isGenerating` (= `anyInProgress`).
    pub(crate) is_generating: bool,
    /// TS `asyncStatus`: numeric `async_status` or `None`.
    pub(crate) async_status: Option<i64>,
    pub(crate) is_archived: bool,
    pub(crate) update_time: Option<f64>,
    pub(crate) turns: Vec<Turn>,
    pub(crate) api_tool_requests: Vec<ApiToolRequest>,
}

impl Conversation {
    /// Index of the last `user` turn (the anchor the poll loop diffs against).
    pub(crate) fn last_user_turn_index(&self) -> Option<usize> {
        self.turns.iter().rposition(|t| t.role == "user")
    }

    /// Turns after the last user turn (the reply being watched).
    pub(crate) fn reply_turns(&self) -> &[Turn] {
        match self.last_user_turn_index() {
            Some(i) => &self.turns[i + 1..],
            None => &[],
        }
    }
}

/// Port of `assetFromPointer` (`api.ts:164–178`).
pub(crate) fn asset_from_pointer(part: &Value) -> Option<Asset> {
    // `const pointer = part.asset_pointer; if (typeof pointer !== "string") return null;`
    let pointer = part.get("asset_pointer")?.as_str()?;
    // `const fileId = pointer.replace(/^[a-z-]+:\/\//i, "");`
    let file_id = strip_pointer_scheme(pointer).to_string();
    // `const isImage = part.content_type === "image_asset_pointer" || typeof part.width === "number";`
    let is_image = part.get("content_type").and_then(Value::as_str) == Some("image_asset_pointer")
        || part.get("width").is_some_and(Value::is_number);
    Some(Asset {
        file_id,
        kind: if is_image {
            AssetKind::Image
        } else {
            AssetKind::File
        },
        name: None,
        width: part.get("width").and_then(Value::as_u64),
        height: part.get("height").and_then(Value::as_u64),
        size_bytes: part.get("size_bytes").and_then(Value::as_u64),
    })
}

/// `pointer.replace(/^[a-z-]+:\/\//i, "")`: drops `file-service://`,
/// `sediment://`, ... and leaves anything without a scheme untouched.
fn strip_pointer_scheme(pointer: &str) -> &str {
    match pointer.find("://") {
        Some(idx)
            if idx > 0
                && pointer[..idx]
                    .bytes()
                    .all(|b| b.is_ascii_alphabetic() || b == b'-') =>
        {
            &pointer[idx + 3..]
        }
        _ => pointer,
    }
}

/// Options of [`normalize_with`]; the TS `opts: { includeThoughts?: boolean }`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NormalizeOptions {
    pub(crate) include_thoughts: bool,
}

/// [`normalize_with`] with thoughts included — the provider streams them as
/// reasoning, so this is the default here (the TS default is `false`).
pub(crate) fn normalize(raw: &RawConversation) -> Conversation {
    normalize_with(
        raw,
        NormalizeOptions {
            include_thoughts: true,
        },
    )
}

/// Port of `normalizeConversation` (`api.ts:182–280`): flatten the mapping
/// tree into the currently-selected linear thread and keep only turns a
/// consumer cares about (user/assistant/tool content addressed to the user;
/// thoughts optional). Plus `api_tool_requests`, which is computed over the
/// unfiltered chain (connector requests are `recipient != all` and would be
/// dropped from `turns`).
pub(crate) fn normalize_with(raw: &RawConversation, opts: NormalizeOptions) -> Conversation {
    // api.ts:187-196
    //   const chain: RawNode[] = [];
    //   let cursor = raw.current_node; const guard = new Set();
    //   while (cursor && raw.mapping[cursor] && !guard.has(cursor)) { guard.add(cursor);
    //     chain.push(raw.mapping[cursor]); cursor = node.parent; }
    //   chain.reverse();
    let mut chain: Vec<&RawNode> = Vec::new();
    let mut cursor = raw.current_node.as_deref();
    let mut guard: HashSet<&str> = HashSet::new();
    while let Some(id) = cursor {
        if id.is_empty() || !guard.insert(id) {
            break;
        }
        let Some(node) = raw.mapping.get(id) else {
            break;
        };
        chain.push(node);
        cursor = node.parent.as_deref();
    }
    chain.reverse();

    // api.ts:198-207
    //   // "Generating" must be scoped to nodes at/after the LAST user message: a
    //   // stray in_progress node stuck earlier in the history (e.g. an old stopped
    //   // generation) would otherwise make the conversation look busy forever.
    //   let lastUserIdx = -1;
    //   chain.forEach((node, i) => { const m = node.message;
    //     if (m && m.author.role === "user" && m.content.content_type !== "user_editable_context") lastUserIdx = i; });
    let last_user_idx = chain.iter().rposition(|node| {
        node.message.as_ref().is_some_and(|m| {
            m.author.role == "user" && m.content.content_type != "user_editable_context"
        })
    });
    //   const anyInProgress = chain.some((node, i) =>
    //     i >= lastUserIdx && lastUserIdx >= 0 && node.message?.status === "in_progress");
    let any_in_progress = last_user_idx.is_some_and(|last| {
        chain.iter().enumerate().any(|(i, node)| {
            i >= last
                && node
                    .message
                    .as_ref()
                    .is_some_and(|m| m.status == "in_progress")
        })
    });

    // chat-on-steroids `extension/fiber.js:807-812` (`requestOf`): a request is
    // an assistant message whose `recipient` starts with `api_tool`.
    // `fiber.js:1036-1046` (`callsOf`): "Results first, so a request can say
    // whether its own answer has arrived. `parent_id` is what pairs them;
    // position does not" — a result is a later `tool` message whose
    // `metadata.parent_id` is the request's message id.
    let mut api_tool_requests: Vec<ApiToolRequest> = Vec::new();
    for (i, node) in chain.iter().enumerate() {
        let Some(m) = node.message.as_ref() else {
            continue;
        };
        if m.author.role != "assistant" {
            continue;
        }
        let Some(recipient) = m.recipient.as_deref().filter(|r| r.starts_with("api_tool")) else {
            continue;
        };
        // FORK (C5, verified live): for a custom MCP connector the `tool`
        // result message is *not* part of the mapping `/backend-api/conversation`
        // returns — the chain goes straight from the `api_tool.call_tool`
        // request to the assistant's next message (whose `metadata.parent_id`
        // names the missing result node). The model only continues once the
        // result has landed, so any later message on the chain also counts as
        // the request having been answered; otherwise a connector turn could
        // never complete.
        let has_result = chain[i + 1..].iter().any(|later| {
            later.message.as_ref().is_some_and(|r| {
                (r.author.role == "tool" && r.metadata.parent_id.as_deref() == Some(m.id.as_str()))
                    || r.author.role == "assistant"
                    || r.author.role == "user"
            })
        });
        api_tool_requests.push(ApiToolRequest {
            message_id: m.id.clone(),
            request_id: m.metadata.request_id.clone(),
            recipient: recipient.to_string(),
            has_result,
        });
    }

    let mut turns: Vec<Turn> = Vec::new();

    // api.ts:211-268 `for (const node of chain) { ... }`
    for node in &chain {
        //   const m = node.message; if (!m) continue;
        let Some(m) = node.message.as_ref() else {
            continue;
        };
        //   const role = m.author.role; const ctype = m.content.content_type;
        let role = m.author.role.as_str();
        let ctype = m.content.content_type.as_str();
        //   if (role === "system") continue;
        if role == "system" {
            continue;
        }
        //   if (ctype === "user_editable_context" || ctype === "model_editable_context") continue;
        if ctype == "user_editable_context" || ctype == "model_editable_context" {
            continue;
        }
        //   // Tool-addressed intermediate steps (searches, code, image-gen internals)
        //   // are noise for consumers; keep only what is addressed to the user, plus
        //   // tool messages that carry assets (generated images arrive on role=tool).
        let mut assets: Vec<Asset> = Vec::new();
        let mut text = String::new();
        let mut thoughts: Vec<Thought> = Vec::new();

        if ctype == "thoughts" {
            //   if (!opts.includeThoughts) continue;
            if !opts.include_thoughts {
                continue;
            }
            //   text = (m.content.thoughts ?? []).map((t) => [t.summary, t.content].filter(Boolean).join("\n")).join("\n\n");
            text = m
                .content
                .thoughts
                .iter()
                .map(|t| {
                    [t.summary.as_deref(), t.content.as_deref()]
                        .into_iter()
                        .flatten()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            thoughts = m
                .content
                .thoughts
                .iter()
                .map(|t| Thought {
                    summary: t.summary.clone(),
                    content: t.content.clone(),
                })
                .collect();
        } else if let Some(parts) = m.content.parts.as_deref() {
            //   } else if (Array.isArray(m.content.parts)) {
            //     const strings = []; for (const part of m.content.parts) {
            //       if (typeof part === "string") strings.push(part);
            //       else if (part && typeof part === "object") { const asset = assetFromPointer(part); if (asset) assets.push(asset); } }
            //     text = strings.join("\n").trim();
            let mut strings: Vec<&str> = Vec::new();
            for part in parts {
                match part {
                    Value::String(s) => strings.push(s),
                    Value::Object(_) => {
                        if let Some(asset) = asset_from_pointer(part) {
                            assets.push(asset);
                        }
                    }
                    _ => {}
                }
            }
            text = strings.join("\n").trim().to_string();
        } else if let Some(t) = m.content.text.as_deref() {
            //   } else if (typeof m.content.text === "string") { text = m.content.text; }
            text = t.to_string();
        }

        //   if (m.recipient && m.recipient !== "all" && assets.length === 0) continue;
        if m.recipient
            .as_deref()
            .is_some_and(|r| !r.is_empty() && r != "all")
            && assets.is_empty()
        {
            continue;
        }
        //   if (role === "tool" && assets.length === 0 && !text) continue;
        if role == "tool" && assets.is_empty() && text.is_empty() {
            continue;
        }
        //   if (!text && assets.length === 0 && m.status !== "in_progress") continue;
        if text.is_empty() && assets.is_empty() && m.status != "in_progress" {
            continue;
        }

        //   for (const att of m.metadata?.attachments ?? []) assets.push({ fileId: att.id,
        //     kind: att.mime_type?.startsWith("image/") ? "image" : "file", name: att.name, sizeBytes: att.size });
        for att in &m.metadata.attachments {
            assets.push(Asset {
                file_id: att.id.clone(),
                kind: if att
                    .mime_type
                    .as_deref()
                    .is_some_and(|mt| mt.starts_with("image/"))
                {
                    AssetKind::Image
                } else {
                    AssetKind::File
                },
                name: att.name.clone(),
                width: None,
                height: None,
                size_bytes: att.size,
            });
        }

        //   turns.push({ role: ctype === "thoughts" ? "assistant-thoughts" : role, messageId: m.id,
        //     contentType: ctype, text, model: m.metadata?.model_slug, createTime: m.create_time,
        //     status: m.status, endTurn: m.end_turn, assets });
        turns.push(Turn {
            id: node.id.clone(),
            role: if ctype == "thoughts" {
                "assistant-thoughts".to_string()
            } else {
                role.to_string()
            },
            message_id: m.id.clone(),
            content_type: ctype.to_string(),
            text,
            thoughts,
            status: m.status.clone(),
            end_turn: m.end_turn,
            recipient: m.recipient.clone(),
            model_slug: m.metadata.model_slug.clone(),
            create_time: m.create_time,
            assets,
            parent_id: m.metadata.parent_id.clone(),
        });
    }

    // api.ts:270-278 `return { id, title: raw.title, model: raw.default_model_slug,
    //   isGenerating: anyInProgress, asyncStatus: typeof raw.async_status === "number" ? raw.async_status : null, turns };`
    Conversation {
        id: raw.conversation_id().unwrap_or_default().to_string(),
        title: raw.title.clone(),
        model: raw.default_model_slug.clone(),
        current_node: raw.current_node.clone(),
        any_in_progress,
        is_generating: any_in_progress,
        async_status: raw.async_status.as_ref().and_then(async_status_number),
        is_archived: raw.is_archived.unwrap_or(false),
        update_time: raw.update_time,
        turns,
        api_tool_requests,
    }
}

/// `typeof raw.async_status === "number" ? raw.async_status : null`, as an
/// integer (the backend uses small ints; a float is truncated).
fn async_status_number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Cheap fingerprint of what a poll observed: message ids, text lengths,
/// thought lengths, statuses, `end_turn` and assets. Two polls with the same
/// fingerprint mean "nothing moved" (the `fingerprint estável` completion rule
/// for asset-only replies). Deterministic within a process (fixed-key SipHash).
pub(crate) fn fingerprint(conv: &Conversation) -> u64 {
    let mut h = DefaultHasher::new();
    conv.turns.len().hash(&mut h);
    for turn in &conv.turns {
        turn.message_id.hash(&mut h);
        turn.role.hash(&mut h);
        turn.text.len().hash(&mut h);
        turn.thoughts
            .iter()
            .map(|t| {
                t.summary.as_deref().map_or(0, str::len) + t.content.as_deref().map_or(0, str::len)
            })
            .sum::<usize>()
            .hash(&mut h);
        turn.status.hash(&mut h);
        turn.end_turn.hash(&mut h);
        turn.assets.hash(&mut h);
    }
    conv.any_in_progress.hash(&mut h);
    conv.async_status.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Page evaluation seam
// ---------------------------------------------------------------------------

/// Evaluate a page script in a tab. Implemented by the daemon client
/// (`daemon.rs`); `api.rs` depends only on this so it can be unit tested with
/// canned responses. `expression` is a function expression the daemon invokes
/// as `(<expr>)()`; implementations return the script's JSON result already
/// decoded (the `evalIn` double decoding). A plain string is tolerated and
/// parsed here as a fallback.
pub(crate) trait PageEval: Send + Sync {
    fn eval<'a>(
        &'a self,
        tab_id: TabId,
        expression: String,
        timeout_ms: u64,
    ) -> BoxFuture<'a, DriverResult<Value>>;
}

/// What `page_scripts::api_call` resolves: `{status, json, text}` on a
/// completed fetch, `{status: 0, error}` when the fetch (or the token lookup)
/// threw. Port of the TS `ApiResponse`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ApiResponse {
    pub(crate) status: u16,
    pub(crate) json: Option<Value>,
    pub(crate) text: Option<String>,
    pub(crate) error: Option<String>,
}

/// `GET /backend-api/conversations` payload.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ConversationList {
    pub(crate) total: u64,
    pub(crate) offset: u64,
    pub(crate) limit: u64,
    #[serde(deserialize_with = "null_default")]
    pub(crate) items: Vec<ConversationSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ConversationSummary {
    #[serde(deserialize_with = "null_default")]
    pub(crate) id: String,
    #[serde(deserialize_with = "null_default")]
    pub(crate) title: String,
    /// ISO-8601 on the list endpoint (unlike the float on the conversation).
    pub(crate) update_time: Option<String>,
    pub(crate) is_archived: Option<bool>,
}

/// Port of the TS `ModelsInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelsInfo {
    pub(crate) default_slug: Option<String>,
    pub(crate) models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ModelEntry {
    #[serde(deserialize_with = "null_default")]
    pub(crate) slug: String,
    #[serde(deserialize_with = "null_default")]
    pub(crate) title: String,
}

/// Raw `GET /backend-api/models` payload (only what `models()` reads).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawModels {
    #[serde(deserialize_with = "null_default")]
    models: Vec<ModelEntry>,
    default_model_slug: Option<String>,
}

/// Port of `private modelsCache`, shared process-wide and keyed by `base_url`
/// (one entry per ChatGPT origin), since `ChatGptApi` values are short-lived.
/// FORK (verified live): the ChatGPT backend rate limits `/backend-api/…`
/// per account, and several turns polling at once kept the account in "Too
/// many requests" for minutes. Every request routed through
/// [`ChatGptApi::with_backend_limiter`] waits its turn here so the whole
/// process never exceeds one backend call per [`BACKEND_MIN_INTERVAL`].
pub(crate) const BACKEND_MIN_INTERVAL: Duration = Duration::from_secs(3);

pub(crate) struct BackendLimiter {
    gate: Semaphore,
    last: Mutex<Option<Instant>>,
    min_interval: Duration,
}

impl BackendLimiter {
    pub(crate) fn new(min_interval: Duration) -> Self {
        Self {
            gate: Semaphore::new(1),
            last: Mutex::new(None),
            min_interval,
        }
    }

    /// Waits until a request may be issued, then records it. Callers are
    /// served in arrival order (one permit), so a burst spreads evenly.
    pub(crate) async fn acquire(&self) {
        let Ok(_permit) = self.gate.acquire().await else {
            return;
        };
        let wait = {
            let last = self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            last.map(|at| self.min_interval.saturating_sub(at.elapsed()))
        };
        if let Some(wait) = wait
            && !wait.is_zero()
        {
            tokio::time::sleep(wait).await;
        }
        let mut last = self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *last = Some(Instant::now());
    }
}

/// The process-wide limiter (one account per Chrome, one Chrome per process).
pub(crate) fn backend_limiter() -> &'static BackendLimiter {
    static LIMITER: LazyLock<BackendLimiter> =
        LazyLock::new(|| BackendLimiter::new(BACKEND_MIN_INTERVAL));
    &LIMITER
}

static MODELS_CACHE: LazyLock<Mutex<HashMap<String, (Instant, ModelsInfo)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Forget the cached model list for `base_url` (tests / after login changes).
pub(crate) fn clear_models_cache(base_url: &str) {
    MODELS_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(base_url);
}

/// Port of the TS `ChatGptApi` class. Every call is a `page_scripts::api_call`
/// evaluated in `tab_id` through `eval`; the tab only needs to be on the
/// chatgpt.com origin (cookies live in the browser).
pub(crate) struct ChatGptApi<'a> {
    eval: &'a dyn PageEval,
    tab_id: TabId,
    /// Origin prefixed to every path. Empty means "relative to the tab's own
    /// origin", which is what the TS does.
    base_url: String,
    backoff: Vec<Duration>,
    eval_timeout_ms: u64,
    /// When set, every call waits on the process-wide backend limiter.
    limiter: Option<&'static BackendLimiter>,
}

impl<'a> ChatGptApi<'a> {
    pub(crate) fn new(eval: &'a dyn PageEval, tab_id: TabId, base_url: impl Into<String>) -> Self {
        Self {
            eval,
            tab_id,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            backoff: RATE_LIMIT_BACKOFF.to_vec(),
            limiter: None,
            eval_timeout_ms: API_EVAL_TIMEOUT_MS,
        }
    }

    /// Override the 429 backoff schedule (tests use zero delays).
    pub(crate) fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
    }

    /// Route every call through the process-wide backend limiter (the
    /// production driver does; tests and the registry keep their own pacing).
    pub(crate) fn with_backend_limiter(mut self) -> Self {
        self.limiter = Some(backend_limiter());
        self
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn tab_id(&self) -> TabId {
        self.tab_id
    }

    fn url(&self, path: &str) -> String {
        if self.base_url.is_empty() {
            path.to_string()
        } else {
            format!("{}{path}", self.base_url)
        }
    }

    /// Port of `call()`: one page-side fetch. Only a page-level failure
    /// (`{status: 0, error}`) is an error here; HTTP statuses are returned.
    pub(crate) async fn call(
        &self,
        path: &str,
        method: &str,
        body: Option<&Value>,
    ) -> DriverResult<ApiResponse> {
        let script = page_scripts::api_call(&self.url(path), method, body);
        if let Some(limiter) = self.limiter {
            limiter.acquire().await;
        }
        tracing::debug!("chatgpt_web backend call: {method} {path}");
        let raw = self
            .eval
            .eval(self.tab_id, script, self.eval_timeout_ms)
            .await?;
        let res = decode_api_response(raw)?;
        // `if (res.error) throw new Error(`ChatGPT API ${path} failed in page: ${res.error}`);`
        if let Some(error) = res.error.as_deref().filter(|e| !e.is_empty()) {
            let kind = if error.contains("not logged in") {
                DriverErrorKind::LoginRequired
            } else {
                DriverErrorKind::Other
            };
            return Err(DriverError::new(
                kind,
                format!("ChatGPT API {path} failed in page: {error}"),
            ));
        }
        Ok(res)
    }

    /// Port of `callOk()`: retry 429 with backoff, then require 2xx and return
    /// the JSON body.
    ///
    /// 429s are transient bursts (heavy polling, several instances at once) and
    /// mean the request was NOT executed — retrying with backoff is safe for
    /// every endpoint we call, PATCHes included (they set idempotent flags).
    async fn call_ok(&self, path: &str, method: &str, body: Option<&Value>) -> DriverResult<Value> {
        let mut attempt = 0usize;
        loop {
            let res = self.call(path, method, body).await?;
            if res.status == 429 && attempt < self.backoff.len() {
                let delay = self.backoff[attempt];
                info!(
                    "ChatGPT API {method} {path} → 429, retrying in {}ms",
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            if !(200..300).contains(&res.status) {
                return Err(http_error(method, path, &res));
            }
            return Ok(res.json.unwrap_or(Value::Null));
        }
    }

    /// `GET /backend-api/conversations?offset&limit&order=updated`.
    pub(crate) async fn list_conversations(
        &self,
        offset: u64,
        limit: u64,
    ) -> DriverResult<ConversationList> {
        let json = self
            .call_ok(
                &format!("/backend-api/conversations?offset={offset}&limit={limit}&order=updated"),
                "GET",
                None,
            )
            .await?;
        Ok(serde_json::from_value(json)?)
    }

    /// `GET /backend-api/conversation/<id>`.
    pub(crate) async fn get_conversation(&self, id: &str) -> DriverResult<RawConversation> {
        let json = self
            .call_ok(&format!("/backend-api/conversation/{id}"), "GET", None)
            .await?;
        Ok(serde_json::from_value(json)?)
    }

    /// [`get_conversation`](Self::get_conversation) followed by [`normalize`].
    pub(crate) async fn read_conversation(&self, id: &str) -> DriverResult<Conversation> {
        Ok(normalize(&self.get_conversation(id).await?))
    }

    /// `PATCH /backend-api/conversation/<id>` with `{title}`, `{is_archived: true}`
    /// or `{is_visible: false}`.
    pub(crate) async fn patch_conversation(&self, id: &str, patch: Value) -> DriverResult<()> {
        self.call_ok(
            &format!("/backend-api/conversation/{id}"),
            "PATCH",
            Some(&patch),
        )
        .await?;
        Ok(())
    }

    /// `GET /backend-api/models?history_and_training_disabled=false`, cached
    /// for [`MODELS_CACHE_TTL`] per `base_url`.
    pub(crate) async fn models(&self) -> DriverResult<ModelsInfo> {
        // `if (this.modelsCache && Date.now() - this.modelsCache.at < 300_000) return this.modelsCache.data;`
        {
            let cache = MODELS_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, data)) = cache.get(&self.base_url)
                && at.elapsed() < MODELS_CACHE_TTL
            {
                return Ok(data.clone());
            }
        }
        let json = self
            .call_ok(
                "/backend-api/models?history_and_training_disabled=false",
                "GET",
                None,
            )
            .await?;
        let raw: RawModels = serde_json::from_value(json)?;
        let data = ModelsInfo {
            default_slug: raw.default_model_slug,
            models: raw.models,
        };
        MODELS_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.base_url.clone(), (Instant::now(), data.clone()));
        Ok(data)
    }
}

/// The `evalIn` contract: the script resolves a JSON string, the daemon client
/// decodes it. A string that still reaches us is decoded here (tolerating a
/// `PageEval` that skipped the second decode); a non-JSON string is an error.
fn decode_api_response(raw: Value) -> DriverResult<ApiResponse> {
    let value = match raw {
        Value::String(s) => serde_json::from_str::<Value>(&s).map_err(|e| {
            DriverError::other(format!(
                "ChatGPT API page script returned a non-JSON string ({e}): {}",
                truncate(&s, 300)
            ))
        })?,
        other => other,
    };
    if !value.is_object() {
        return Err(DriverError::other(format!(
            "ChatGPT API page script returned an unexpected value: {}",
            truncate(&value.to_string(), 300)
        )));
    }
    Ok(serde_json::from_value(value)?)
}

/// `throw new Error(`ChatGPT API ${method} ${path} → HTTP ${res.status}: ${JSON.stringify(res.json ?? res.text).slice(0, 300)}`)`
/// with the status mapped onto [`DriverErrorKind`].
fn http_error(method: &str, path: &str, res: &ApiResponse) -> DriverError {
    let body = match (&res.json, &res.text) {
        (Some(json), _) => json.to_string(),
        (None, Some(text)) => serde_json::to_string(text).unwrap_or_else(|_| text.clone()),
        (None, None) => "null".to_string(),
    };
    let kind = match res.status {
        401 | 403 => DriverErrorKind::LoginRequired,
        404 => DriverErrorKind::ConversationNotFound,
        429 => DriverErrorKind::RateLimited,
        500..=599 => DriverErrorKind::Upstream,
        _ => DriverErrorKind::Other,
    };
    DriverError::new(
        kind,
        format!(
            "ChatGPT API {method} {path} → HTTP {}: {}",
            res.status,
            truncate(&body, 300)
        ),
    )
}

/// `.slice(0, n)` on a char boundary.
fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
