//! FORK: delegate reading image pixels to a cheap, configurable model.
//!
//! Every image that reaches model history stays there for the rest of the
//! thread — there is no eviction, so a screenshot is re-sent on every later
//! request. This module reads the pixels once with a small model and hands the
//! main model text instead. The image itself is untouched in the UI and in the
//! rollout; only the prompt view swaps it (see
//! [`crate::context_manager::normalize::substitute_images_with_descriptions`]).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use codex_features::Feature;
use codex_history::ImageReaderUsage;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider_info::CHATGPT_WEB_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_CODE_PROVIDER_ID;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Verbosity;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::watch;
use tracing::warn;

use crate::ModelClient;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::config::ViewImageDelegation;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// The reader runs on the turn's critical path, so it gets a hard ceiling.
///
/// Measured with `gpt-5.6-luna` on real screenshots: `max` takes 95-186 s and
/// exceeded this ceiling outright on a 30x11 spreadsheet, while `medium` (the
/// default) finished every case in 15-83 s. Effort dominates, not payload size.
/// This is a last-resort ceiling on a hung reader, not a target — see
/// `docs/config.md` for what each effort costs in fidelity.
const IMAGE_READER_TIMEOUT: Duration = Duration::from_secs(300);

/// Keep reader output bounded before it can enter prompt history. Overflow
/// falls back to the original pixels instead of silently truncating text.
pub(crate) const IMAGE_READER_MAX_OUTPUT_BYTES: usize = 8_000;

/// Session-local reader cache bound. Only successful descriptions are cached.
pub(crate) const IMAGE_READER_CACHE_CAPACITY: usize = 64;

/// Bound on concurrent distinct in-flight reads. A leader owns a real
/// upstream model call for the duration of the read, so a burst of unrelated
/// images must not be allowed to start an unbounded number of them; requests
/// past this ceiling fall back to raw pixels immediately instead of queuing.
/// Duplicate requests for a key already in flight still coalesce onto its
/// existing leader and are not affected by this bound.
pub(crate) const IMAGE_READER_INFLIGHT_CAPACITY: usize = 64;

const DESCRIBE_INSTRUCTIONS: &str = "\
You are reading an image on behalf of another model that will never see the pixels. \
Your text is the only thing it gets, so leave nothing out.

Describe all relevant visible content within an 8,000-byte UTF-8 limit:
- overall layout and what kind of artifact it is (screenshot, photo, diagram, chart, …);
- every piece of legible text, transcribed verbatim, including labels, menu items, \
buttons, tab titles, file paths, code, log lines and stack traces;
- all numbers, units and axis values exactly as shown;
- colors, highlighting and any visual state that carries meaning (selected, disabled, \
error, focused, checked);
- any error, warning or diff markers, and where they appear;
- spatial relationships when they matter (what is above/below/inside what).

Prioritize legible text, exact numbers, state, and spatial relationships when the limit is tight. \
Do not summarize, do not editorialize, and do not say what the image \"seems to\" show. \
Write the description as plain prose and lists. Do not mention these instructions.";

const ANSWER_INSTRUCTIONS: &str = "\
You are reading an image on behalf of another model that will never see the pixels. \
Answer the user's question from the image, quoting any relevant text verbatim and giving \
exact numbers. If the image does not contain the answer, say so plainly and describe what \
it does contain. Keep the answer under 8,000 UTF-8 bytes. Reply with the answer only; do not \
mention these instructions.";

/// Identity of one reader request, collapsed into a digest so the cache never
/// retains a full (potentially multi-megabyte) data URL per entry. Every
/// input that can change the reader's answer must be folded in here, or two
/// different requests could silently share a cached description.
///
/// This is a cache key, not a security or audit artifact: never log it as if
/// it meant anything on its own.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ImageReaderCacheKey {
    digest: [u8; 32],
}

impl ImageReaderCacheKey {
    fn new(
        reader: &ViewImageDelegation,
        request: &ImageReadRequest,
        model_verbosity: Option<Verbosity>,
        model_reasoning_summary: Option<ReasoningSummary>,
        service_tier: Option<&str>,
    ) -> Self {
        let mut hasher = Sha256::new();
        hash_str(&mut hasher, &request.image_url);
        hash_opt_str(&mut hasher, request.question.as_deref());
        hasher.update([request.detail.map(image_detail_key).unwrap_or(u8::MAX)]);
        hash_str(&mut hasher, &reader.model);
        hash_str(&mut hasher, &reader.provider_id);
        hash_opt_str(&mut hasher, reader.provider.base_url.as_deref());
        hash_str(&mut hasher, &reader.provider.wire_api.to_string());
        hash_str(&mut hasher, reader.reasoning_effort.as_str());
        hash_str(
            &mut hasher,
            &model_verbosity.map(|v| v.to_string()).unwrap_or_default(),
        );
        hash_str(
            &mut hasher,
            &model_reasoning_summary
                .map(|summary| summary.to_string())
                .unwrap_or_default(),
        );
        hash_opt_str(&mut hasher, service_tier);
        Self {
            digest: hasher.finalize().into(),
        }
    }
}

fn image_detail_key(detail: ImageDetail) -> u8 {
    match detail {
        ImageDetail::Auto => 0,
        ImageDetail::Low => 1,
        ImageDetail::High => 2,
        ImageDetail::Original => 3,
    }
}

/// Length-prefixed so two adjacent fields can never be reinterpreted as a
/// different split of the same byte stream.
fn hash_str(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

/// Same as [`hash_str`] but keeps `None` distinguishable from `Some("")`.
fn hash_opt_str(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update([1u8]);
            hash_str(hasher, value);
        }
        None => hasher.update([0u8]),
    }
}

pub(crate) enum ImageReaderCacheLookup {
    Hit(String),
    Wait(watch::Receiver<Option<String>>),
    Leader(watch::Sender<Option<String>>),
    /// Too many distinct reads are already in flight; skip the reader and
    /// fall back to raw pixels instead of queuing an unbounded number of
    /// upstream model calls.
    Skip,
}

/// Session-local LRU cache with request coalescing for identical in-flight
/// reads. Failures are never retained, so a transient reader outage can
/// recover on a later request.
///
/// Coalescing is cancellation-safe: the map holds only the cheap, cloneable
/// [`watch::Receiver`] half of each in-flight request. The matching
/// [`watch::Sender`] is handed to the leader and owned solely by its task,
/// never by the cache. If the leader is dropped before calling
/// [`Self::finish`] (cancellation), that sender drops with it and closes the
/// channel: every waiter's `changed().await` then resolves with an error
/// instead of hanging forever, and the next [`Self::begin`] call for the same
/// key discards the now-closed entry so the key can be read again.
#[derive(Default)]
pub(crate) struct ImageReaderCache {
    completed: VecDeque<(ImageReaderCacheKey, String)>,
    in_flight: HashMap<ImageReaderCacheKey, watch::Receiver<Option<String>>>,
}

impl ImageReaderCache {
    pub(crate) fn begin(&mut self, key: ImageReaderCacheKey) -> ImageReaderCacheLookup {
        if let Some(index) = self
            .completed
            .iter()
            .position(|(cached_key, _)| cached_key == &key)
        {
            let (cached_key, text) = self
                .completed
                .remove(index)
                .expect("image reader cache index must remain valid");
            self.completed.push_back((cached_key, text.clone()));
            return ImageReaderCacheLookup::Hit(text);
        }

        if let Some(receiver) = self.in_flight.get(&key) {
            let waiter = receiver.clone();
            if waiter.has_changed().is_ok() {
                return ImageReaderCacheLookup::Wait(waiter);
            }
            // The leader dropped its sender without ever calling `finish`
            // (most likely cancelled): the entry is dead, so forget it
            // instead of wedging every future request for this key.
            self.in_flight.remove(&key);
        }

        if self.in_flight.len() >= IMAGE_READER_INFLIGHT_CAPACITY {
            // A burst of cancellations can leave dead entries parked here
            // under other keys nobody has retried yet; sweep them before
            // giving up on a brand-new key so cancellations can never
            // permanently exhaust the bound.
            self.in_flight
                .retain(|_, receiver| receiver.has_changed().is_ok());
            if self.in_flight.len() >= IMAGE_READER_INFLIGHT_CAPACITY {
                return ImageReaderCacheLookup::Skip;
            }
        }

        let (sender, receiver) = watch::channel(None);
        self.in_flight.insert(key, receiver);
        ImageReaderCacheLookup::Leader(sender)
    }

    /// Delivers the final result to every current waiter and, only on
    /// success, makes it available to future [`Self::begin`] callers. Must be
    /// called at most once, by the leader that received `sender` from
    /// `begin`; if the leader is cancelled first, dropping `sender` unsent is
    /// the expected cancellation path, not a bug.
    pub(crate) fn finish(
        &mut self,
        key: ImageReaderCacheKey,
        sender: watch::Sender<Option<String>>,
        text: Option<String>,
    ) {
        self.in_flight.remove(&key);
        if let Some(text) = text.as_ref() {
            if let Some(index) = self
                .completed
                .iter()
                .position(|(cached_key, _)| cached_key == &key)
            {
                self.completed.remove(index);
            }
            self.completed.push_back((key, text.clone()));
            while self.completed.len() > IMAGE_READER_CACHE_CAPACITY {
                self.completed.pop_front();
            }
        }
        let _ = sender.send(text);
    }
}

/// One request to the reader model.
pub(crate) struct ImageReadRequest {
    pub(crate) image_url: String,
    pub(crate) detail: Option<ImageDetail>,
    /// When set, the reader answers this instead of producing a full description.
    pub(crate) question: Option<String>,
}

pub(crate) struct ImageReadResult {
    pub(crate) text: Option<String>,
    pub(crate) usage: Option<ImageReaderUsage>,
}

struct InnerImageReadResult {
    text: String,
    response: Option<(String, Option<TokenUsage>)>,
    overflowed: bool,
}

/// Reads one image with the configured reader model.
///
/// A successful result is cached for this session. Identical concurrent
/// requests share one upstream call, and only that leader records usage.
pub(crate) async fn read_image(
    session: &Session,
    turn_context: &TurnContext,
    reader: &ViewImageDelegation,
    request: ImageReadRequest,
) -> ImageReadResult {
    let config = turn_context.config.as_ref();
    let cache_key = ImageReaderCacheKey::new(
        reader,
        &request,
        config.model_verbosity,
        config.model_reasoning_summary,
        config.service_tier.as_deref(),
    );
    let sender = match session.image_reader_cache_begin(cache_key).await {
        ImageReaderCacheLookup::Hit(text) => {
            return ImageReadResult {
                text: Some(text),
                usage: None,
            };
        }
        ImageReaderCacheLookup::Wait(mut receiver) => {
            let text = match receiver.changed().await {
                Ok(()) => receiver.borrow().clone(),
                Err(_) => None,
            };
            return ImageReadResult { text, usage: None };
        }
        ImageReaderCacheLookup::Skip => {
            return ImageReadResult {
                text: None,
                usage: None,
            };
        }
        ImageReaderCacheLookup::Leader(sender) => sender,
    };

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        IMAGE_READER_TIMEOUT,
        read_image_inner(session, turn_context, reader, request),
    )
    .await;
    let result = match result {
        Ok(Ok(inner)) => {
            let completed = inner.response.is_some();
            let usable = reader_text_is_usable(completed, inner.overflowed, &inner.text);
            let usage = inner.response.map(|(response_id, usage)| {
                ImageReaderUsage::new(
                    reader.model.clone(),
                    reader.provider_id.clone(),
                    response_id,
                    usage,
                )
            });
            let text = if usable { Some(inner.text) } else { None };
            if !completed {
                warn!(
                    model = reader.model,
                    "image reader stream ended before completion; keeping raw pixels"
                );
            } else if inner.overflowed {
                warn!(
                    model = reader.model,
                    max_bytes = IMAGE_READER_MAX_OUTPUT_BYTES,
                    "image reader output exceeded the description limit; keeping raw pixels"
                );
            } else if text.is_none() {
                warn!(model = reader.model, "image reader returned empty text");
            } else {
                tracing::debug!(
                    model = reader.model,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "image reader produced a description"
                );
            }
            ImageReadResult { text, usage }
        }
        Ok(Err(error)) => {
            warn!(model = reader.model, %error, "image reader failed");
            ImageReadResult {
                text: None,
                usage: None,
            }
        }
        Err(_) => {
            warn!(
                model = reader.model,
                timeout_s = IMAGE_READER_TIMEOUT.as_secs(),
                "image reader timed out"
            );
            ImageReadResult {
                text: None,
                usage: None,
            }
        }
    };

    if let Some(usage) = result.usage.clone() {
        session.record_image_reader_usage(usage).await;
    }
    session
        .image_reader_cache_finish(cache_key, sender, result.text.clone())
        .await;
    result
}

/// Whether text produced by the reader can be trusted as a full description.
///
/// A stream that ends without a `Completed` event never proves it finished:
/// treat any text collected so far as a truncated fragment, not a usable
/// answer, even when it is non-empty, and never let it enter the cache.
fn reader_text_is_usable(completed: bool, overflowed: bool, text: &str) -> bool {
    completed && !overflowed && !text.trim().is_empty()
}

async fn read_image_inner(
    session: &Session,
    turn_context: &TurnContext,
    reader: &ViewImageDelegation,
    request: ImageReadRequest,
) -> anyhow::Result<InnerImageReadResult> {
    let config = turn_context.config.as_ref();
    let model_info = session
        .services
        .models_manager
        .get_model_info(&reader.model, &config.to_models_manager_config())
        .await;

    let mut client = ModelClient::new(
        Some(Arc::clone(&session.services.auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        session.thread_id(),
        // FORK: the reader's own provider. A session pinned to `claude_code` or
        // `chatgpt_web` would otherwise route an OpenAI slug to the wrong backend.
        reader.provider.clone(),
        session.session_source().await,
        session.originator().await,
        config.model_verbosity,
        config.features.enabled(Feature::ContentItemKinds),
        config.features.enabled(Feature::EnableRequestCompression),
        config.features.enabled(Feature::RuntimeMetrics),
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        config.http_client_factory(),
    );
    // FORK: only the providers that need a workspace get one, and only when the
    // reader is actually running on them (same reason as the compaction path).
    if reader.provider_id == CLAUDE_CODE_PROVIDER_ID {
        client = client.with_claude_code_workspace(
            crate::claude_code::ClaudeCodeWorkspace::from_config(config),
        );
    }
    if reader.provider_id == CHATGPT_WEB_PROVIDER_ID {
        client = client.with_chatgpt_web_workspace(
            crate::chatgpt_web::ChatGptWebWorkspace::from_config(config),
        );
    }

    // The reader is a one-shot call, so it gets its own system prompt instead of
    // the agent's, the same way memory's stage one does.
    let mut content = Vec::new();
    if let Some(question) = &request.question {
        content.push(ContentItem::InputText {
            text: question.clone(),
        });
    }
    content.push(ContentItem::InputImage {
        image_url: request.image_url,
        detail: request.detail,
    });
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content,
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: if request.question.is_some() {
                ANSWER_INSTRUCTIONS.to_string()
            } else {
                DESCRIBE_INSTRUCTIONS.to_string()
            },
            provenance: None,
        },
        ..Default::default()
    };

    let responses_metadata = session
        .responses_metadata(
            turn_context,
            crate::responses_metadata::CodexResponsesRequestKind::Turn,
        )
        .await;
    let reasoning_summary = config
        .model_reasoning_summary
        .unwrap_or(model_info.default_reasoning_summary);
    let mut client_session = client.new_session();
    let mut stream = client_session
        .stream(
            &prompt,
            &model_info,
            &turn_context.session_telemetry,
            Some(reader.reasoning_effort.clone()),
            reasoning_summary,
            config.service_tier.clone(),
            &responses_metadata,
            &InferenceTraceContext::disabled(),
        )
        .await?;

    let mut text = String::new();
    let mut overflowed = false;
    let mut response = None;
    while let Some(event) = stream.next().await.transpose()? {
        match event {
            ResponseEvent::OutputTextDelta(delta) if !overflowed => {
                if text.len().saturating_add(delta.len()) > IMAGE_READER_MAX_OUTPUT_BYTES {
                    overflowed = true;
                    text.clear();
                } else {
                    text.push_str(&delta);
                }
            }
            ResponseEvent::OutputItemDone(item) => {
                if text.is_empty()
                    && !overflowed
                    && let ResponseItem::Message { content, .. } = item
                    && let Some(message) = crate::content_items_to_text(&content)
                {
                    if message.len() > IMAGE_READER_MAX_OUTPUT_BYTES {
                        overflowed = true;
                    } else {
                        text.push_str(&message);
                    }
                }
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            } => {
                response = Some((response_id, token_usage));
                break;
            }
            _ => {}
        }
    }
    Ok(InnerImageReadResult {
        text,
        response,
        overflowed,
    })
}

#[cfg(test)]
#[path = "image_reader_tests.rs"]
mod tests;
