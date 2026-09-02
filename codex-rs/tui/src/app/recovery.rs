//! FORK: asynchronous TUI clients for the experimental thread recovery methods.
//!
//! Recovery is deliberately a bare current-thread action. The chat widget checks the local
//! surface before this module starts a request, and the completion path checks it again before
//! replacing the active widget. The app event request ID is the only value crossing the event
//! bus; the opaque recovery token remains in the widget state until explicit confirmation.

use super::session_lifecycle::ThreadAttachPresentation;
use crate::app_event::AppEvent;
use crate::app_event::ThreadRecoveryPreviewResult;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ResumeModelSettings;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadRecoveryCreateParams;
use codex_app_server_protocol::ThreadRecoveryCreateResponse;
use codex_app_server_protocol::ThreadRecoveryPreviewParams;
use codex_app_server_protocol::ThreadRecoveryPreviewResponse;
use codex_protocol::ThreadId;
use uuid::Uuid;

use super::App;

const RECOVERY_PREVIEW_ERROR: &str = "Failed to inspect thread recovery";
const RECOVERY_CREATE_ERROR: &str = "Failed to create recovered thread";

impl App {
    pub(super) fn open_recovery(&mut self, app_server: &AppServerSession) {
        let Some(thread_id) = self.chat_widget.thread_id() else {
            self.chat_widget
                .add_error_message("Recovery is unavailable until the session starts.".to_string());
            return;
        };
        if let Some(reason) = self.chat_widget.recovery_block_reason() {
            self.chat_widget.add_error_message(reason.to_string());
            return;
        }

        let request_id = self.chat_widget.begin_recovery_preview(thread_id);
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ThreadRecoveryPreviewResponse>(
                    ClientRequest::ThreadRecoveryPreview {
                        request_id: RequestId::String(format!("recovery-preview-{request_id}")),
                        params: ThreadRecoveryPreviewParams {
                            thread_id: thread_id.to_string(),
                        },
                    },
                )
                .await
                .map(ThreadRecoveryPreviewResult::from_response)
                .map_err(|error| format!("{RECOVERY_PREVIEW_ERROR}: {error}"));
            app_event_tx.send(AppEvent::RecoveryPreviewLoaded { request_id, result });
        });
    }

    pub(super) fn apply_recovery_preview(
        &mut self,
        request_id: Uuid,
        result: Result<ThreadRecoveryPreviewResult, String>,
    ) {
        self.chat_widget.apply_recovery_preview(request_id, result);
    }

    pub(super) async fn create_recovery(
        &mut self,
        app_server: &AppServerSession,
        preview_request_id: Uuid,
    ) {
        if let Some(reason) = self.chat_widget.recovery_block_reason() {
            self.chat_widget.add_error_message(reason.to_string());
            return;
        }
        let Some((request_id, source_thread_id, token)) =
            self.chat_widget.begin_recovery_create(preview_request_id)
        else {
            return;
        };
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ThreadRecoveryCreateResponse>(
                    ClientRequest::ThreadRecoveryCreate {
                        request_id: RequestId::String(format!("recovery-create-{request_id}")),
                        params: ThreadRecoveryCreateParams { token },
                    },
                )
                .await
                .map_err(|error| format!("{RECOVERY_CREATE_ERROR}: {error}"));
            app_event_tx.send(AppEvent::RecoveryCreated {
                request_id,
                source_thread_id,
                result,
            });
        });
    }

    pub(super) async fn apply_recovery_created(
        &mut self,
        tui: &mut crate::tui::Tui,
        app_server: &mut AppServerSession,
        request_id: Uuid,
        source_thread_id: ThreadId,
        result: Result<ThreadRecoveryCreateResponse, String>,
    ) {
        if !self.chat_widget.finish_recovery_create_request(request_id) {
            return;
        }
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                self.chat_widget.add_error_message(error);
                return;
            }
        };
        let recovered_thread_id = match ThreadId::from_string(&response.thread.id) {
            Ok(thread_id) => thread_id,
            Err(error) => {
                self.chat_widget.add_error_message(format!(
                    "{RECOVERY_CREATE_ERROR}: server returned an invalid thread ID: {error}"
                ));
                return;
            }
        };
        if recovered_thread_id == source_thread_id
            || response.recovered_from_thread_id != source_thread_id.to_string()
        {
            self.chat_widget.add_error_message(format!(
                "{RECOVERY_CREATE_ERROR}: server returned an invalid source lineage."
            ));
            return;
        }

        let source_is_current = self.chat_widget.thread_id() == Some(source_thread_id)
            && self.primary_thread_id == Some(source_thread_id)
            && self.chat_widget.recovery_block_reason().is_none();
        if !source_is_current {
            self.announce_recovered_thread(recovered_thread_id);
            return;
        }

        let config = self.config.clone();
        let resumed = app_server
            .resume_thread(
                &self.local_settings,
                config.clone(),
                recovered_thread_id,
                ResumeModelSettings::RestoreFromThread,
            )
            .await;
        let started = match resumed {
            Ok(started) => started,
            Err(error) => {
                self.chat_widget.add_error_message(format!(
                    "{RECOVERY_CREATE_ERROR}: recovery created thread {recovered_thread_id}, but it could not be attached: {error}"
                ));
                return;
            }
        };

        self.shutdown_current_thread(app_server).await;
        self.config = config;
        if let Err(error) = self
            .replace_chat_widget_with_app_server_thread(
                tui,
                started,
                ThreadAttachPresentation::SessionLineage,
                /*initial_user_message*/ None,
            )
            .await
        {
            self.chat_widget.add_error_message(format!(
                "{RECOVERY_CREATE_ERROR}: recovery created thread {recovered_thread_id}, but it could not be attached: {error}"
            ));
            return;
        }
        self.chat_widget.add_info_message(
            format!("Recovered thread {recovered_thread_id} from {source_thread_id}."),
            None,
        );
    }

    fn announce_recovered_thread(&mut self, recovered_thread_id: ThreadId) {
        self.chat_widget.add_info_message(
            format!(
                "Recovery created thread {recovered_thread_id}. Use /resume {recovered_thread_id} to open it."
            ),
            None,
        );
    }
}
