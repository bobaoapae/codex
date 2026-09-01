//! FORK: current-thread recovery preview and explicit confirmation.
//!
//! The app layer owns the asynchronous app-server calls. This module owns only the small state
//! machine needed to keep an in-flight preview stale-safe and to keep the opaque recovery token
//! out of rendered rows and app events emitted by the confirmation popup.

use codex_app_server_protocol::ThreadRecoveryPreviewResponse;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use uuid::Uuid;

use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::app_event::ThreadRecoveryPreviewResult;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;

const PREVIEW_ERROR: &str = "Recovery preview failed";
const CREATE_DISABLED_REASON: &str = "The server did not return a recovery token.";
const MAX_REASON_CHARS: usize = 240;
pub(crate) const RECOVERY_PREVIEW_VIEW_ID: &str = "recovery-preview";

/// State for one user-visible recovery flow.
///
/// Only the app layer can consume `token`, and no rendered popup or `AppEvent` contains it. A new
/// preview replaces the old request ID so late responses cannot replace a newer confirmation.
#[derive(Default)]
pub(crate) struct RecoveryState {
    pub(crate) preview_request_id: Option<Uuid>,
    pub(crate) confirmation_request_id: Option<Uuid>,
    pub(crate) source_thread_id: Option<ThreadId>,
    pub(crate) token: Option<String>,
    pub(crate) create_request_id: Option<Uuid>,
}

impl ChatWidget {
    /// Return the user-facing reason `/recover` cannot run in the current surface.
    pub(crate) fn recovery_block_reason(&self) -> Option<&'static str> {
        if self.thread_id.is_none() {
            return Some("Recovery is unavailable until the session starts.");
        }
        if self.active_side_conversation {
            return Some("Recovery is unavailable in a side conversation.");
        }
        if self.blocks_direct_input {
            return Some("Recovery is unavailable for a parent-owned thread.");
        }
        if self.is_user_turn_pending_or_running() || self.has_queued_follow_up_messages() {
            return Some("Recovery is unavailable while a turn or queued follow-up is active.");
        }
        None
    }

    pub(crate) fn begin_recovery_preview(&mut self, source_thread_id: ThreadId) -> Uuid {
        let request_id = Uuid::new_v4();
        self.recovery.preview_request_id = Some(request_id);
        self.recovery.confirmation_request_id = None;
        self.recovery.source_thread_id = Some(source_thread_id);
        self.recovery.token = None;
        self.recovery.create_request_id = None;
        request_id
    }

    /// Apply a preview only if it belongs to the currently visible request.
    pub(crate) fn apply_recovery_preview(
        &mut self,
        request_id: Uuid,
        result: Result<ThreadRecoveryPreviewResult, String>,
    ) {
        if self.recovery.preview_request_id != Some(request_id) {
            return;
        }
        self.recovery.preview_request_id = None;

        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.recovery.source_thread_id = None;
                self.add_error_message(format!("{PREVIEW_ERROR}: {error}"));
                return;
            }
        };
        let Some(source_thread_id) = self.recovery.source_thread_id else {
            self.add_error_message(format!("{PREVIEW_ERROR}: source thread is unavailable."));
            return;
        };
        if result.response.thread_id != source_thread_id.to_string() {
            self.recovery.source_thread_id = None;
            self.add_error_message(format!(
                "{PREVIEW_ERROR}: the response did not describe the current thread."
            ));
            return;
        }

        let token_available = result.response.can_recover && result.token.is_some();
        self.recovery.token = token_available.then_some(result.token).flatten();
        self.recovery.confirmation_request_id = Some(request_id);
        self.show_recovery_preview(request_id, result.response, token_available);
    }

    /// Move the opaque token into the app-layer async request after explicit popup acceptance.
    pub(crate) fn begin_recovery_create(
        &mut self,
        preview_request_id: Uuid,
    ) -> Option<(Uuid, ThreadId, String)> {
        if self.recovery.preview_request_id.is_some()
            || self.recovery.create_request_id.is_some()
            || self.recovery.confirmation_request_id != Some(preview_request_id)
            || self.recovery.source_thread_id.is_none()
        {
            return None;
        }
        let source_thread_id = self.recovery.source_thread_id?;
        let token = self.recovery.token.take()?;
        let create_request_id = Uuid::new_v4();
        self.recovery.create_request_id = Some(create_request_id);
        Some((create_request_id, source_thread_id, token))
    }

    pub(crate) fn finish_recovery_create_request(&mut self, request_id: Uuid) -> bool {
        if self.recovery.create_request_id != Some(request_id) {
            return false;
        }
        self.recovery.create_request_id = None;
        self.recovery.confirmation_request_id = None;
        self.recovery.source_thread_id = None;
        self.recovery.token = None;
        true
    }

    pub(crate) fn cancel_recovery(&mut self, request_id: Uuid) -> bool {
        if self.recovery.confirmation_request_id != Some(request_id)
            || self.recovery.create_request_id.is_some()
        {
            return false;
        }
        self.recovery.confirmation_request_id = None;
        self.recovery.source_thread_id = None;
        self.recovery.token = None;
        true
    }

    pub(crate) fn recovery_popup_active(&self) -> bool {
        self.bottom_pane
            .selected_index_for_present_view(RECOVERY_PREVIEW_VIEW_ID)
            .is_some()
    }

    /// Drop a token when a recovery popup was dismissed by a cancellation key.
    ///
    /// The popup's cancellation callback still emits an app event so the app-level state machine
    /// records the dismissal. Clearing here as well keeps the widget state safe for direct input
    /// and tests that route keys without running the outer app event loop.
    pub(crate) fn clear_recovery_after_popup_cancel(
        &mut self,
        was_active: bool,
        key_event: KeyEvent,
    ) {
        let is_cancel_key = matches!(key_event.code, KeyCode::Esc)
            || key_event.code == KeyCode::Char('c')
                && key_event.modifiers.contains(KeyModifiers::CONTROL);
        if !was_active || self.recovery_popup_active() || !is_cancel_key {
            return;
        }
        if let Some(request_id) = self.recovery.confirmation_request_id {
            self.cancel_recovery(request_id);
        }
    }

    fn show_recovery_preview(
        &mut self,
        request_id: Uuid,
        response: ThreadRecoveryPreviewResponse,
        token_available: bool,
    ) {
        let mut description = format!(
            "Retain {} of {} items; exclude {} items across {} failed turns.",
            response.retained_item_count,
            response.source_item_count,
            response.counts.excluded_items,
            response.counts.failed_turns,
        );
        if let Some(reason) = response
            .reason
            .as_deref()
            .or(response.blocked_reason.as_deref())
        {
            description.push(' ');
            description.push_str(&bounded_text(reason));
        }

        let disabled_reason = if !response.can_recover {
            response
                .reason
                .as_deref()
                .or(response.blocked_reason.as_deref())
                .map(bounded_text)
                .or_else(|| Some("The server marked this thread as not recoverable.".to_string()))
        } else if !token_available {
            Some(CREATE_DISABLED_REASON.to_string())
        } else {
            None
        };
        let actions = disabled_reason.is_none().then(|| {
            vec![
                Box::new(move |tx: &crate::app_event_sender::AppEventSender| {
                    tx.send(AppEvent::CreateThreadRecovery { request_id });
                }) as crate::bottom_pane::SelectionAction,
            ]
        });
        self.show_selection_view(SelectionViewParams {
            view_id: Some(RECOVERY_PREVIEW_VIEW_ID),
            title: Some("Recovery preview".to_string()),
            subtitle: Some(format!("Immutable preview for {}", response.thread_id)),
            footer_hint: Some(standard_popup_hint_line()),
            on_cancel: Some(Box::new(move |tx| {
                tx.send(AppEvent::CancelThreadRecovery { request_id });
            })),
            items: vec![SelectionItem {
                name: "Create replacement lineage".to_string(),
                description: Some(description),
                actions: actions.unwrap_or_default(),
                dismiss_on_select: true,
                require_explicit_confirmation: true,
                disabled_reason,
                ..Default::default()
            }],
            ..Default::default()
        });
    }
}

fn bounded_text(text: &str) -> String {
    let mut bounded = text.chars().take(MAX_REASON_CHARS).collect::<String>();
    if text.chars().count() > MAX_REASON_CHARS {
        bounded.push('…');
    }
    bounded
}
