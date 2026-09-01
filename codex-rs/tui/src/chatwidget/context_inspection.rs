//! Read-only `/context` request state and presentation entrypoints.

use codex_protocol::ThreadId;
use uuid::Uuid;

use super::ChatWidget;
use crate::app_event::ContextInspectionLoadResult;

#[path = "context_inspection_render.rs"]
mod render;
use render::context_error_lines;
use render::context_loading_lines;
use render::context_not_found_lines;
use render::context_summary_lines;

/// Tracks the request currently represented by the widget's loading card.
#[derive(Default)]
pub(crate) struct ContextInspectionState {
    request_id: Option<Uuid>,
    thread_id: Option<ThreadId>,
}

impl ContextInspectionState {
    fn begin(&mut self, thread_id: ThreadId) -> Uuid {
        let request_id = Uuid::new_v4();
        self.request_id = Some(request_id);
        self.thread_id = Some(thread_id);
        request_id
    }

    fn finish(&mut self, request_id: Uuid, thread_id: ThreadId) -> bool {
        if self.request_id != Some(request_id) {
            return false;
        }
        let matches_thread = self.thread_id == Some(thread_id);
        self.request_id = None;
        self.thread_id = None;
        matches_thread
    }
}

impl ChatWidget {
    /// Begin one context request and return the token carried by its completion event.
    pub(crate) fn begin_context_inspection_request(&mut self, thread_id: ThreadId) -> Uuid {
        self.context_inspection.begin(thread_id)
    }

    /// Consume the current request only when both its token and target thread still match.
    pub(crate) fn finish_context_inspection_request(
        &mut self,
        request_id: Uuid,
        thread_id: ThreadId,
    ) -> bool {
        self.context_inspection.finish(request_id, thread_id)
    }

    pub(crate) fn show_context_loading(&mut self, include_preview: bool) {
        self.add_plain_history_lines(context_loading_lines(include_preview));
    }

    pub(crate) fn show_context_not_found(&mut self) {
        self.add_plain_history_lines(context_not_found_lines());
    }

    pub(crate) fn show_context_result(
        &mut self,
        result: ContextInspectionLoadResult,
        include_preview: bool,
    ) {
        let lines = match result {
            ContextInspectionLoadResult::Success(response) => {
                context_summary_lines(&response.context, include_preview)
            }
            ContextInspectionLoadResult::NotFound => context_not_found_lines(),
            ContextInspectionLoadResult::Error(error) => context_error_lines(&error),
        };
        self.add_plain_history_lines(lines);
    }
}

#[cfg(test)]
#[path = "context_inspection_tests.rs"]
mod tests;
