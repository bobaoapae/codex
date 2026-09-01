//! App-server orchestration for the read-only `/context` command.
//!
//! There is one request per explicit command. The request token and displayed thread check in the
//! ChatWidget decide whether a late response may update the transcript.

use super::*;
use crate::app_event::ContextInspectionLoadResult;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ContextInspectParams;
use codex_app_server_protocol::ContextInspectResponse;
use codex_app_server_protocol::RequestId;

impl App {
    pub(super) fn open_context_inspection(
        &mut self,
        app_server: &AppServerSession,
        include_preview: bool,
    ) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            self.chat_widget.show_context_not_found();
            return;
        };

        let request_id = self.chat_widget.begin_context_inspection_request(thread_id);
        self.chat_widget.show_context_loading(include_preview);

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ContextInspectResponse>(ClientRequest::ContextInspect {
                    request_id: RequestId::String(format!(
                        "context-inspect-{}",
                        uuid::Uuid::new_v4()
                    )),
                    params: ContextInspectParams {
                        thread_id: thread_id.to_string(),
                        include_preview,
                    },
                })
                .await
                .map(|response| ContextInspectionLoadResult::Success(Box::new(response)))
                .unwrap_or_else(map_context_inspection_error);
            app_event_tx.send(crate::app_event::AppEvent::ContextInspectionLoaded {
                request_id,
                thread_id,
                include_preview,
                result,
            });
        });
    }

    pub(super) fn apply_context_inspection_result(
        &mut self,
        request_id: uuid::Uuid,
        thread_id: codex_protocol::ThreadId,
        include_preview: bool,
        result: ContextInspectionLoadResult,
    ) {
        if !self
            .chat_widget
            .finish_context_inspection_request(request_id, thread_id)
            || self.current_displayed_thread_id() != Some(thread_id)
        {
            return;
        }
        self.chat_widget
            .show_context_result(result, include_preview);
    }
}

fn map_context_inspection_error(
    error: codex_app_server_client::TypedRequestError,
) -> ContextInspectionLoadResult {
    let not_found = matches!(
        &error,
        codex_app_server_client::TypedRequestError::Server { source, .. }
            if source
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(serde_json::Value::as_str)
                == Some("notFound")
    );
    if not_found {
        ContextInspectionLoadResult::NotFound
    } else {
        ContextInspectionLoadResult::Error(error.to_string())
    }
}
