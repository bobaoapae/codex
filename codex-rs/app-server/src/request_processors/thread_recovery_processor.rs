//! Experimental thread recovery request handling.
//!
//! The recovery algorithm belongs to the thread-store implementation. This
//! module owns only the app-server boundary: it derives the provider-attested
//! candidate policy from persisted events, delegates validation and sanitizing
//! to `codex-thread-store`, and starts the returned replacement lineage.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::ThreadRecoveryCreateParams;
use codex_app_server_protocol::ThreadRecoveryCreateResponse;
use codex_app_server_protocol::ThreadRecoveryExcludedItem;
use codex_app_server_protocol::ThreadRecoveryPreviewParams;
use codex_app_server_protocol::ThreadRecoveryPreviewResponse;
use codex_app_server_protocol::ThreadRecoveryWatermark;
use codex_core::ForkSnapshot;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::W3cTraceContext;
use codex_rollout::InitialHistory;
use codex_rollout::RolloutItem;
use codex_thread_store::ExistingRecovery;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::PreparedRecovery;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::RecoveryBlockReason;
use codex_thread_store::RecoveryCreateParams;
use codex_thread_store::RecoveryCreateResult;
use codex_thread_store::RecoveryEncryptedAgentMessageCandidate;
use codex_thread_store::RecoveryExclusionReason;
use codex_thread_store::RecoveryLimits;
use codex_thread_store::RecoveryPolicy;
use codex_thread_store::RecoveryPreview;
use codex_thread_store::RecoveryPreviewParams;
use codex_thread_store::RecoveryQuiescenceAttestation;
use codex_thread_store::RecoveryQuiescenceParams;
use codex_thread_store::RecoveryRetryTurnCandidate;
use codex_thread_store::RecoveryRolloutScan;
use codex_thread_store::RecoveryToken;
use codex_thread_store::RecoveryTurnCompleteCandidate;
use codex_thread_store::RecoveryTurnState;
use codex_thread_store::RecoveryWatermark;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;
use crate::error_code::method_not_found;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::skills_watcher::SkillsWatcher;
use crate::thread_state::ThreadStateManager;
use crate::thread_status::ThreadWatchManager;

use super::thread_from_stored_thread;
use super::thread_input::can_accept_direct_input;
use super::thread_lifecycle::ListenerTaskContext;
use super::thread_lifecycle::ensure_conversation_listener;
use super::thread_lifecycle::log_listener_attach_result;
use super::thread_summary::thread_started_notification;

const HISTORY_DECRYPTION_ERROR_FRAGMENT: &str =
    "Encrypted function output content could not be decrypted or decoded.";
const RECOVERY_MAX_ITEMS: usize = 100_000;
const RECOVERY_MAX_SERIALIZED_BYTES: u64 = 40 * 1024 * 1024;

/// The future returned by a thread recovery engine operation.
pub(super) type ThreadRecoveryFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ThreadRecoveryError>> + Send + 'a>>;

/// Store-neutral preview request passed through the app-server adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ThreadRecoveryPreviewRequest {
    pub thread_id: ThreadId,
    pub quiescence: Option<RecoveryQuiescenceAttestation>,
    pub has_live_descendants: bool,
}

/// Store-neutral create request passed through the app-server adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ThreadRecoveryCreateRequest {
    pub token: RecoveryToken,
    pub quiescence: Option<RecoveryQuiescenceAttestation>,
    pub has_live_descendants: bool,
}

/// Failure categories exposed by the recovery boundary.
#[derive(Debug, thiserror::Error)]
pub(super) enum ThreadRecoveryError {
    #[error("thread recovery is not supported by this thread store")]
    Unsupported,
    #[error("invalid recovery request: {0}")]
    InvalidRequest(String),
    #[error("recovery token is stale")]
    StaleToken,
    #[error("thread cannot be recovered: {0}")]
    NotRecoverable(String),
    #[error("thread not found: {0}")]
    ThreadNotFound(ThreadId),
    #[error("thread recovery failed: {0}")]
    Internal(String),
}

/// Recovery engine contract implemented by `codex-thread-store` adapters.
///
/// Implementations must make `preview` read-only and bind the opaque token to
/// the exact source rollout watermark. `create` must reject a stale token before
/// writing anything and must leave the source rollout immutable.
pub(super) trait ThreadRecoveryEngine: Send + Sync {
    fn preview(
        &self,
        request: ThreadRecoveryPreviewRequest,
    ) -> ThreadRecoveryFuture<'_, RecoveryPreview>;

    fn create(
        &self,
        request: ThreadRecoveryCreateRequest,
    ) -> ThreadRecoveryFuture<'_, RecoveryCreateResult>;
}

/// App-server adapter for the public recovery API of `codex-thread-store`.
///
/// The adapter deliberately does not copy the store's rollout scanner or
/// sanitizer. It only translates the app-server's provider-boundary evidence
/// into the public store request and serializes the returned token.
pub(super) struct ThreadStoreRecoveryAdapter {
    thread_store: Arc<dyn ThreadStore>,
}

impl ThreadStoreRecoveryAdapter {
    pub(super) fn new(thread_store: Arc<dyn ThreadStore>) -> Self {
        Self { thread_store }
    }
}

impl ThreadRecoveryEngine for ThreadStoreRecoveryAdapter {
    fn preview(
        &self,
        request: ThreadRecoveryPreviewRequest,
    ) -> ThreadRecoveryFuture<'_, RecoveryPreview> {
        let thread_store = Arc::clone(&self.thread_store);
        Box::pin(async move {
            let source = thread_store
                .read_thread(ReadThreadParams {
                    thread_id: request.thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
                .map_err(ThreadRecoveryError::from_store)?;
            let rollout_path = source.rollout_path.ok_or_else(|| {
                ThreadRecoveryError::InvalidRequest(format!(
                    "thread {} has no rollout path for recovery evidence",
                    request.thread_id
                ))
            })?;
            let provider_id = source.model_provider;
            let scan = tokio::task::spawn_blocking(move || {
                codex_thread_store::scan_recovery_rollout(
                    rollout_path.as_path(),
                    request.thread_id,
                    RecoveryLimits::default(),
                )
            })
            .await
            .map_err(|error| {
                ThreadRecoveryError::Internal(format!(
                    "failed to join recovery rollout scan: {error}"
                ))
            })?
            .map_err(|error| {
                ThreadRecoveryError::Internal(format!("failed to scan recovery rollout: {error}"))
            })?;
            let policy = recovery_policy_from_rollout(&scan, provider_id.as_str());
            thread_store
                .preview_recovery(RecoveryPreviewParams {
                    thread_id: request.thread_id,
                    include_archived: true,
                    policy,
                    quiescence: request.quiescence,
                    has_live_descendants: request.has_live_descendants,
                })
                .await
                .map_err(ThreadRecoveryError::from_store)
        })
    }

    fn create(
        &self,
        request: ThreadRecoveryCreateRequest,
    ) -> ThreadRecoveryFuture<'_, RecoveryCreateResult> {
        let thread_store = Arc::clone(&self.thread_store);
        Box::pin(async move {
            thread_store
                .create_recovery(RecoveryCreateParams {
                    token: request.token,
                    quiescence: request.quiescence,
                    has_live_descendants: request.has_live_descendants,
                })
                .await
                .map_err(ThreadRecoveryError::from_store)
        })
    }
}

impl ThreadRecoveryError {
    fn from_store(error: ThreadStoreError) -> Self {
        match error {
            ThreadStoreError::Unsupported { .. } => Self::Unsupported,
            ThreadStoreError::InvalidRequest { message } => Self::InvalidRequest(message),
            ThreadStoreError::ThreadNotFound { thread_id } => Self::ThreadNotFound(thread_id),
            ThreadStoreError::Conflict { message } => {
                if message.contains("changed after preview")
                    || message.contains("token")
                    || message.contains("source rollout")
                    || (message.contains("attestation") && message.contains("stale"))
                {
                    Self::StaleToken
                } else {
                    Self::NotRecoverable(message)
                }
            }
            ThreadStoreError::Internal { message } => Self::Internal(message),
        }
    }
}

/// Construction arguments for the recovery request processor.
pub(crate) struct ThreadRecoveryRequestProcessorArgs {
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) config: Arc<Config>,
    pub(crate) config_manager: crate::config_manager::ConfigManager,
    pub(crate) thread_manager: Arc<ThreadManager>,
    pub(crate) outgoing: Arc<OutgoingMessageSender>,
    pub(crate) pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    pub(crate) thread_state_manager: ThreadStateManager,
    pub(crate) thread_watch_manager: ThreadWatchManager,
    pub(crate) thread_list_state_permit: Arc<Semaphore>,
    pub(crate) skills_watcher: Arc<SkillsWatcher>,
    pub(crate) turn_cost_worker: Option<crate::turn_cost_worker::TurnCostWorkerHandle>,
}

/// Request processor for the two experimental recovery methods.
pub(crate) struct ThreadRecoveryRequestProcessor {
    engine: Arc<dyn ThreadRecoveryEngine>,
    config: Arc<Config>,
    config_manager: crate::config_manager::ConfigManager,
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_list_state_permit: Arc<Semaphore>,
    skills_watcher: Arc<SkillsWatcher>,
    turn_cost_worker: Option<crate::turn_cost_worker::TurnCostWorkerHandle>,
}

impl ThreadRecoveryRequestProcessor {
    pub(crate) fn new(args: ThreadRecoveryRequestProcessorArgs) -> Self {
        let ThreadRecoveryRequestProcessorArgs {
            thread_store,
            config,
            config_manager,
            thread_manager,
            outgoing,
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            skills_watcher,
            turn_cost_worker,
        } = args;
        let engine = Arc::new(ThreadStoreRecoveryAdapter::new(Arc::clone(&thread_store)));
        Self {
            engine,
            config,
            config_manager,
            thread_manager,
            thread_store,
            outgoing,
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            skills_watcher,
            turn_cost_worker,
        }
    }

    pub(crate) async fn preview(
        &self,
        params: ThreadRecoveryPreviewParams,
    ) -> Result<ThreadRecoveryPreviewResponse, codex_app_server_protocol::JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        let has_live_descendants = self.has_live_descendants(thread_id).await?;
        let quiescence = if has_live_descendants {
            None
        } else {
            self.prepare_source_quiescence(thread_id).await?
        };
        let preview = self
            .engine
            .preview(ThreadRecoveryPreviewRequest {
                thread_id,
                quiescence,
                has_live_descendants,
            })
            .await
            .map_err(recovery_error)?;
        preview_response(preview)
    }

    pub(crate) async fn create(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRecoveryCreateParams,
        client_mcp_extensions: codex_protocol::mcp::ClientMcpExtensions,
        parent_trace: Option<W3cTraceContext>,
    ) -> Result<ThreadRecoveryCreateResponse, codex_app_server_protocol::JSONRPCErrorError> {
        let token = decode_recovery_token(&params.token)?;
        let source_thread_id = token.source_thread_id;
        let has_live_descendants = self.has_live_descendants(source_thread_id).await?;
        if has_live_descendants {
            return Err(invalid_request(format!(
                "cannot recover thread {source_thread_id} while live descendants exist"
            )));
        }
        let source_config = self.source_config(source_thread_id).await?;
        let quiescence = self.prepare_source_quiescence(source_thread_id).await?;
        let create_result = self
            .engine
            .create(ThreadRecoveryCreateRequest {
                token,
                quiescence,
                has_live_descendants,
            })
            .await
            .map_err(recovery_error)?;
        match create_result {
            RecoveryCreateResult::Prepared(prepared) => {
                self.start_recovered_thread(
                    request_id,
                    prepared,
                    source_config,
                    client_mcp_extensions,
                    parent_trace,
                )
                .await
            }
            RecoveryCreateResult::Existing(existing) => {
                self.existing_recovery_response(existing).await
            }
        }
    }

    async fn has_live_descendants(
        &self,
        thread_id: ThreadId,
    ) -> Result<bool, codex_app_server_protocol::JSONRPCErrorError> {
        let subtree = self
            .thread_manager
            .list_agent_subtree_thread_ids(thread_id)
            .await
            .map_err(|error| {
                internal_error(format!("failed to inspect recovery descendants: {error}"))
            })?;
        for descendant_id in subtree.into_iter().skip(1) {
            if self.thread_manager.get_thread(descendant_id).await.is_ok() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn prepare_source_quiescence(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<RecoveryQuiescenceAttestation>, codex_app_server_protocol::JSONRPCErrorError>
    {
        let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
            return Ok(None);
        };
        let loaded_status = self
            .thread_watch_manager
            .loaded_status_for_thread(&thread_id.to_string())
            .await;
        if matches!(
            loaded_status,
            codex_app_server_protocol::ThreadStatus::Active { .. }
        ) || matches!(
            thread.agent_status().await,
            AgentStatus::Running | AgentStatus::PendingInit
        ) {
            return Err(invalid_request(format!(
                "thread {thread_id} must be idle before recovery"
            )));
        }
        thread.flush_rollout().await.map_err(|error| {
            internal_error(format!("failed to flush thread {thread_id}: {error}"))
        })?;
        self.thread_store
            .attest_recovery_quiescence(RecoveryQuiescenceParams {
                thread_id,
                turn_state: RecoveryTurnState::Idle,
            })
            .await
            .map(Some)
            .map_err(ThreadRecoveryError::from_store)
            .map_err(recovery_error)
    }

    async fn source_config(
        &self,
        thread_id: ThreadId,
    ) -> Result<Arc<Config>, codex_app_server_protocol::JSONRPCErrorError> {
        if let Ok(thread) = self.thread_manager.get_thread(thread_id).await {
            return Ok(thread.config().await);
        }

        let stored_thread = self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(|error| recovery_read_error(thread_id, error))?;
        let model_context = self
            .thread_store
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id,
                include_archived: true,
            })
            .await
            .map_err(|error| recovery_read_error(thread_id, error))?;
        let persisted_settings = super::persisted_resume_settings::latest_persisted_resume_settings(
            &model_context.items,
        );
        let config = self
            .config_manager
            .load_for_cwd(
                None,
                ConfigOverrides {
                    model: stored_thread.model,
                    cwd: Some(stored_thread.cwd.clone()),
                    approval_policy: Some(
                        persisted_settings
                            .as_ref()
                            .map_or(stored_thread.approval_mode, |settings| {
                                settings.approval_policy
                            }),
                    ),
                    approvals_reviewer: persisted_settings
                        .as_ref()
                        .and_then(|settings| settings.approvals_reviewer),
                    permission_profile: Some(stored_thread.permission_profile),
                    persisted_permission_profile_id: persisted_settings.and_then(|settings| {
                        settings.active_permission_profile.map(|profile| profile.id)
                    }),
                    model_provider: Some(stored_thread.model_provider),
                    ..Default::default()
                },
                Some(stored_thread.cwd),
            )
            .await
            .map_err(|error| {
                internal_error(format!(
                    "failed to restore configuration for thread {thread_id}: {error}"
                ))
            })?;
        Ok(Arc::new(config))
    }

    async fn start_recovered_thread(
        &self,
        request_id: &ConnectionRequestId,
        prepared: PreparedRecovery,
        source_config: Arc<Config>,
        client_mcp_extensions: codex_protocol::mcp::ClientMcpExtensions,
        parent_trace: Option<W3cTraceContext>,
    ) -> Result<ThreadRecoveryCreateResponse, codex_app_server_protocol::JSONRPCErrorError> {
        let source_thread_id = prepared.source_thread_id;
        let recovered_thread_id = prepared.recovered_thread_id;
        let history = Arc::unwrap_or_clone(Arc::clone(&prepared.model_context));
        let new_thread = self
            .thread_manager
            .fork_thread_from_history(
                ForkSnapshot::Interrupted,
                source_config.as_ref().clone(),
                InitialHistory::Forked(history),
                Some(ThreadSource::Feature("recovery".to_string())),
                parent_trace,
                client_mcp_extensions,
                Some(recovered_thread_id),
            )
            .await
            .map_err(|error| recovery_start_error(source_thread_id, error))?;
        // PreparedRecovery holds the source lifecycle/writer reservation. It
        // must be released after the child has either started or failed.
        drop(prepared);
        if new_thread.thread_id != recovered_thread_id {
            return Err(internal_error(format!(
                "recovery engine reserved thread {recovered_thread_id}, but thread manager created {}",
                new_thread.thread_id
            )));
        }

        let listener_context = ListenerTaskContext {
            thread_manager: Arc::clone(&self.thread_manager),
            thread_state_manager: self.thread_state_manager.clone(),
            outgoing: Arc::clone(&self.outgoing),
            pending_thread_unloads: Arc::clone(&self.pending_thread_unloads),
            thread_watch_manager: self.thread_watch_manager.clone(),
            thread_list_state_permit: Arc::clone(&self.thread_list_state_permit),
            fallback_model_provider: source_config.model_provider_id.clone(),
            codex_home: source_config.codex_home.to_path_buf(),
            skills_watcher: Arc::clone(&self.skills_watcher),
            turn_cost_worker: self.turn_cost_worker.clone(),
            thread_unload_delay: source_config.thread_unload_delay,
        };
        let listener_result = ensure_conversation_listener(
            listener_context,
            recovered_thread_id,
            request_id.connection_id,
            /*raw_events_enabled*/ false,
        )
        .await;
        log_listener_attach_result(
            listener_result,
            recovered_thread_id,
            request_id.connection_id,
            "recovered thread",
        );

        let stored_thread = self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id: recovered_thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(|error| recovery_read_error(recovered_thread_id, error))?;
        let (mut thread, _) = thread_from_stored_thread(
            stored_thread,
            &source_config.model_provider_id,
            &source_config.cwd,
        );
        let config_snapshot = new_thread.thread.config_snapshot().await;
        thread.session_id = new_thread.session_configured.session_id.to_string();
        thread.forked_from_id = Some(source_thread_id.to_string());
        thread.can_accept_direct_input = Some(can_accept_direct_input(
            new_thread.thread.multi_agent_version(),
            &config_snapshot.session_source,
        ));
        thread.thread_source = config_snapshot.thread_source.clone().map(Into::into);
        thread.status = super::resolve_thread_status(
            self.thread_watch_manager
                .loaded_status_for_thread(&thread.id)
                .await,
            /*has_in_progress_turn*/ false,
        );
        self.thread_watch_manager
            .upsert_thread_silently(&thread.id)
            .await;

        self.outgoing
            .send_server_notification(
                codex_app_server_protocol::ServerNotification::ThreadStarted(
                    thread_started_notification(thread.clone()),
                ),
            )
            .await;
        Ok(ThreadRecoveryCreateResponse {
            thread,
            recovered_from_thread_id: source_thread_id.to_string(),
        })
    }

    async fn existing_recovery_response(
        &self,
        existing: ExistingRecovery,
    ) -> Result<ThreadRecoveryCreateResponse, codex_app_server_protocol::JSONRPCErrorError> {
        let stored_thread = self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id: existing.recovered_thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(|error| recovery_read_error(existing.recovered_thread_id, error))?;
        let (mut thread, _) = thread_from_stored_thread(
            stored_thread,
            &self.config.model_provider_id,
            &self.config.cwd,
        );
        thread.forked_from_id = Some(existing.source_thread_id.to_string());
        Ok(ThreadRecoveryCreateResponse {
            thread,
            recovered_from_thread_id: existing.source_thread_id.to_string(),
        })
    }
}

fn parse_thread_id(
    thread_id: &str,
) -> Result<ThreadId, codex_app_server_protocol::JSONRPCErrorError> {
    ThreadId::from_string(thread_id)
        .map_err(|error| invalid_request(format!("invalid thread id: {error}")))
}

fn recovery_policy_from_rollout(scan: &RecoveryRolloutScan, provider_id: &str) -> RecoveryPolicy {
    let records = &scan.records;
    let first_failure_index = records.iter().position(|record| {
        matches!(
            &record.item,
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.error.as_ref().is_some_and(|error| {
                    error.message.contains(HISTORY_DECRYPTION_ERROR_FRAGMENT)
                })
        )
    });
    let encrypted_agent_messages: Vec<RecoveryEncryptedAgentMessageCandidate> = first_failure_index
        .and_then(|failure_index| {
            records[..failure_index]
                .iter()
                .rev()
                .find_map(|record| {
                    let RolloutItem::ResponseItem(envelope) = &record.item else {
                        return None;
                    };
                    let ResponseItem::AgentMessage { id, content, .. } = &envelope.item else {
                        return None;
                    };
                    content
                        .iter()
                        .any(|part| {
                            matches!(
                                part,
                                codex_protocol::models::AgentMessageInputContent::EncryptedContent { .. }
                            )
                        })
                        .then(|| RecoveryEncryptedAgentMessageCandidate {
                            rollout_ordinal: record.ordinal,
                            item_id: id.as_ref().map(ToString::to_string),
                            provider_id: provider_id.to_string(),
                        })
                })
        })
        .into_iter()
        .collect();
    let contaminated_turn_completions = first_failure_index
        .and_then(|failure_index| {
            let record = &records[failure_index];
            let ordinal = record.ordinal;
            let item = &record.item;
            let RolloutItem::EventMsg(EventMsg::TurnComplete(event)) = item else {
                return None;
            };
            let error = event.error.as_ref()?;
            Some(RecoveryTurnCompleteCandidate {
                rollout_ordinal: ordinal,
                turn_id: event.turn_id.clone(),
                error_message: error.message.clone(),
            })
        })
        .into_iter()
        .collect::<Vec<_>>();
    let candidate_ordinal = encrypted_agent_messages
        .first()
        .map(|candidate| candidate.rollout_ordinal);
    let retry_turns = records
        .iter()
        .enumerate()
        .filter_map(|(completion_index, record)| {
            let RolloutItem::EventMsg(EventMsg::TurnComplete(event)) = &record.item else {
                return None;
            };
            let error = event.error.as_ref()?;
            if !error.message.contains(HISTORY_DECRYPTION_ERROR_FRAGMENT) {
                return None;
            }
            let start_ordinal = records[..completion_index]
                .iter()
                .filter_map(|record| match &record.item {
                    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id,
                        ..
                    })) if turn_id == &event.turn_id => Some(record.ordinal),
                    _ => None,
                })
                .next_back();
            if candidate_ordinal
                .is_none_or(|candidate| start_ordinal.is_none_or(|start| start <= candidate))
            {
                return None;
            }
            Some(RecoveryRetryTurnCandidate {
                turn_id: event.turn_id.clone(),
                error_message: error.message.clone(),
            })
        })
        .collect::<Vec<_>>();
    RecoveryPolicy {
        encrypted_agent_messages,
        contaminated_turn_completions,
        retry_turns,
        limits: RecoveryLimits {
            max_items: RECOVERY_MAX_ITEMS,
            max_serialized_bytes: RECOVERY_MAX_SERIALIZED_BYTES,
        },
    }
}

fn preview_response(
    preview: RecoveryPreview,
) -> Result<ThreadRecoveryPreviewResponse, codex_app_server_protocol::JSONRPCErrorError> {
    let excluded_items = preview
        .excluded_items
        .iter()
        .map(excluded_item_response)
        .collect::<Vec<_>>();
    let failed_turns = failed_retry_turn_count(&preview.excluded_items);
    let token = preview
        .token
        .as_ref()
        .map(encode_recovery_token)
        .transpose()?;
    Ok(ThreadRecoveryPreviewResponse {
        token,
        thread_id: preview.source_thread_id.to_string(),
        source_rollout_id: preview.source_rollout_id.to_string(),
        source_model_provider: preview.source_model_provider,
        watermark: recovery_watermark_response(preview.watermark),
        source_item_count: preview.source_item_count as u64,
        source_serialized_bytes: preview.source_serialized_bytes,
        retained_item_count: preview.retained_item_count as u64,
        retained_serialized_bytes: preview.retained_serialized_bytes,
        excluded_items,
        counts: codex_app_server_protocol::ThreadRecoveryCounts {
            total_items: preview.source_item_count as u64,
            retained_items: preview.retained_item_count as u64,
            excluded_items: preview.excluded_items.len() as u64,
            failed_turns,
        },
        can_recover: preview.can_recover,
        reason: preview.blocked_reason.map(recovery_block_reason_message),
        blocked_reason: preview.blocked_reason.map(recovery_block_reason_wire),
    })
}

fn failed_retry_turn_count(excluded_items: &[codex_thread_store::RecoveryExcludedItem]) -> u64 {
    let mut turn_ids = HashSet::new();
    for item in excluded_items {
        if let RecoveryExclusionReason::RetryTurn { turn_id } = &item.reason {
            turn_ids.insert(turn_id);
        }
    }
    turn_ids.len() as u64
}

fn excluded_item_response(
    item: &codex_thread_store::RecoveryExcludedItem,
) -> ThreadRecoveryExcludedItem {
    let (turn_id, reason) = match &item.reason {
        RecoveryExclusionReason::InvalidEncryptedAgentMessage { provider_id } => (
            None,
            format!("invalid encrypted agent message from provider {provider_id}"),
        ),
        RecoveryExclusionReason::ContaminatedTurnComplete { turn_id } => (
            Some(turn_id.clone()),
            "terminal error for the contaminated turn".to_string(),
        ),
        RecoveryExclusionReason::RetryTurn { turn_id } => (
            Some(turn_id.clone()),
            "retry turn after poisoned history".to_string(),
        ),
    };
    ThreadRecoveryExcludedItem {
        rollout_ordinal: item.rollout_ordinal,
        item_id: item.item_id.clone(),
        turn_id,
        reason,
    }
}

fn recovery_watermark_response(watermark: RecoveryWatermark) -> ThreadRecoveryWatermark {
    ThreadRecoveryWatermark {
        rollout_id: watermark.rollout_id.to_string(),
        end_ordinal_exclusive: watermark.end_ordinal_exclusive,
        end_byte_offset: watermark.end_byte_offset,
    }
}

fn recovery_block_reason_wire(reason: RecoveryBlockReason) -> String {
    match reason {
        RecoveryBlockReason::LiveDescendants => "liveDescendants",
        RecoveryBlockReason::AmbiguousCandidates => "ambiguousCandidates",
        RecoveryBlockReason::ContextTooLarge => "contextTooLarge",
    }
    .to_string()
}

fn recovery_block_reason_message(reason: RecoveryBlockReason) -> String {
    match reason {
        RecoveryBlockReason::LiveDescendants => {
            "recovery is blocked while live descendants exist".to_string()
        }
        RecoveryBlockReason::AmbiguousCandidates => {
            "recovery candidates are ambiguous or incomplete".to_string()
        }
        RecoveryBlockReason::ContextTooLarge => {
            "retained recovery history exceeds the configured safety limits".to_string()
        }
    }
}

fn encode_recovery_token(
    token: &RecoveryToken,
) -> Result<String, codex_app_server_protocol::JSONRPCErrorError> {
    let bytes = serde_json::to_vec(token)
        .map_err(|error| internal_error(format!("failed to serialize recovery token: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_recovery_token(
    token: &str,
) -> Result<RecoveryToken, codex_app_server_protocol::JSONRPCErrorError> {
    if token.is_empty() {
        return Err(invalid_params("recovery token must not be empty"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| invalid_params("recovery token is malformed"))?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_params("recovery token is malformed"))
}

fn recovery_error(error: ThreadRecoveryError) -> codex_app_server_protocol::JSONRPCErrorError {
    match error {
        ThreadRecoveryError::Unsupported => {
            method_not_found("thread recovery is unavailable in this thread store")
        }
        ThreadRecoveryError::InvalidRequest(message) => invalid_request(message),
        ThreadRecoveryError::StaleToken => invalid_params("recovery token is stale"),
        ThreadRecoveryError::NotRecoverable(message) => invalid_request(message),
        ThreadRecoveryError::ThreadNotFound(thread_id) => {
            invalid_request(format!("thread not found: {thread_id}"))
        }
        ThreadRecoveryError::Internal(message) => internal_error(message),
    }
}

fn recovery_start_error(
    source_thread_id: ThreadId,
    error: codex_protocol::error::CodexErr,
) -> codex_app_server_protocol::JSONRPCErrorError {
    match error.details() {
        codex_protocol::error::CodexErrorDetails::InvalidRequest(message) => {
            invalid_request(message.clone())
        }
        _ => internal_error(format!(
            "failed to start recovered thread from {source_thread_id}: {error}"
        )),
    }
}

fn recovery_read_error(
    thread_id: ThreadId,
    error: ThreadStoreError,
) -> codex_app_server_protocol::JSONRPCErrorError {
    match error {
        ThreadStoreError::ThreadNotFound { .. } => {
            invalid_request(format!("recovered thread {thread_id} was not persisted"))
        }
        ThreadStoreError::InvalidRequest { message } => invalid_request(message),
        ThreadStoreError::Unsupported { operation } => method_not_found(operation),
        error => internal_error(format!(
            "failed to read recovered thread {thread_id}: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_retry_turn_count_deduplicates_items_and_excludes_contaminated_turn() {
        let excluded_items = vec![
            codex_thread_store::RecoveryExcludedItem {
                rollout_ordinal: 1,
                item_id: None,
                reason: RecoveryExclusionReason::ContaminatedTurnComplete {
                    turn_id: "contaminated".to_string(),
                },
            },
            codex_thread_store::RecoveryExcludedItem {
                rollout_ordinal: 2,
                item_id: None,
                reason: RecoveryExclusionReason::RetryTurn {
                    turn_id: "retry-1".to_string(),
                },
            },
            codex_thread_store::RecoveryExcludedItem {
                rollout_ordinal: 3,
                item_id: None,
                reason: RecoveryExclusionReason::RetryTurn {
                    turn_id: "retry-1".to_string(),
                },
            },
            codex_thread_store::RecoveryExcludedItem {
                rollout_ordinal: 4,
                item_id: None,
                reason: RecoveryExclusionReason::RetryTurn {
                    turn_id: "retry-2".to_string(),
                },
            },
        ];

        assert_eq!(failed_retry_turn_count(&excluded_items), 2);
    }
}
