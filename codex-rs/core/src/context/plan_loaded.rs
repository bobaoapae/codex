//! Approved Plan-mode plans injected into the model context.

use super::ContextualUserFragment;
use codex_plans::PlanFragmentError;
use codex_plans::validate_plan_fragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;

const OPEN_MARKER: &str = "<approved_plan>";
const CLOSE_MARKER: &str = "</approved_plan>";
const BODY_OPEN_MARKER: &str = "<approved_plan_body>";
const BODY_CLOSE_MARKER: &str = "</approved_plan_body>";
pub(crate) use codex_plans::MAX_APPROVED_PLAN_TOKENS;
const MAX_APPROVED_PLAN_ID_BYTES: usize = 256;

/// Stable identity of an approved plan snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApprovedPlanRef {
    pub(crate) id: String,
    pub(crate) revision: u32,
}

impl ApprovedPlanRef {
    pub(crate) fn new(id: impl Into<String>, revision: u32) -> Self {
        Self {
            id: id.into(),
            revision,
        }
    }
}

/// A model-visible, immutable approved-plan snapshot.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PlanLoaded {
    approved_plan: ApprovedPlanRef,
    body: String,
}

impl std::fmt::Debug for PlanLoaded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlanLoaded")
            .field("approved_plan", &self.approved_plan)
            .field("body", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlanLoadedError {
    InvalidId,
    TooLarge { approx_tokens: usize },
}

impl std::fmt::Display for PlanLoadedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId => formatter.write_str("approved plan id must be a single line"),
            Self::TooLarge { approx_tokens } => write!(
                formatter,
                "approved plan context is too large ({approx_tokens} approximate tokens; maximum is {MAX_APPROVED_PLAN_TOKENS})"
            ),
        }
    }
}

impl PlanLoaded {
    /// Build a plan fragment without truncating its approved body.
    pub(crate) fn new(
        approved_plan: ApprovedPlanRef,
        body: impl Into<String>,
    ) -> Result<Self, PlanLoadedError> {
        if approved_plan.id.trim().is_empty()
            || approved_plan.id.contains('\n')
            || approved_plan.id.contains('\r')
            || approved_plan.id.len() > MAX_APPROVED_PLAN_ID_BYTES
        {
            return Err(PlanLoadedError::InvalidId);
        }
        let plan = Self {
            approved_plan,
            body: body.into(),
        };
        let rendered = plan.render();
        if let Err(error) = validate_plan_fragment(&rendered) {
            let PlanFragmentError::TooManyTokens { actual, .. } = error else {
                return Err(PlanLoadedError::TooLarge {
                    approx_tokens: MAX_APPROVED_PLAN_TOKENS.saturating_add(1),
                });
            };
            return Err(PlanLoadedError::TooLarge {
                approx_tokens: actual,
            });
        }
        Ok(plan)
    }

    pub(crate) fn approved_plan(&self) -> &ApprovedPlanRef {
        &self.approved_plan
    }

    /// Recover a validated fragment from persisted model-visible text.
    pub(crate) fn from_text(text: &str) -> Option<Self> {
        let inner = text
            .trim()
            .strip_prefix(OPEN_MARKER)?
            .strip_suffix(CLOSE_MARKER)?
            .trim();
        let inner = inner.strip_prefix("Approved plan reference:\nplan_id: ")?;
        let (id, inner) = inner.split_once("\nplan_revision: ")?;
        let (revision, body) = inner.split_once('\n')?;
        let revision = revision.parse().ok()?;
        let body = body
            .strip_prefix(BODY_OPEN_MARKER)?
            .strip_prefix('\n')?
            .strip_suffix(BODY_CLOSE_MARKER)?
            .strip_suffix('\n')?;
        Self::new(
            ApprovedPlanRef::new(unescape_markup(id), revision),
            unescape_markup(body),
        )
        .ok()
    }

    pub(crate) fn from_response_item(item: &ResponseItem) -> Option<Self> {
        let ResponseItem::Message { content, .. } = item else {
            return None;
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return None;
        };
        Self::from_text(text)
    }

    pub(crate) fn is_response_item(item: &ResponseItem) -> bool {
        let ResponseItem::Message { content, .. } = item else {
            return false;
        };
        content.iter().any(|content| {
            matches!(content, ContentItem::InputText { text } if Self::matches_text(text))
        })
    }

    /// Find the latest valid approved-plan fragment in surviving history.
    ///
    /// The caller supplies already-reconstructed history, so rollback and
    /// compaction exclusions have been applied before this scan.
    pub(crate) fn from_history(history: &[codex_history::ResponseItemEnvelope]) -> Option<Self> {
        history.iter().fold(None, |latest, envelope| {
            Self::from_response_item(&envelope.item).or(latest)
        })
    }

    fn render(&self) -> String {
        format!("{OPEN_MARKER}{}{CLOSE_MARKER}", self.rendered_body())
    }

    fn rendered_body(&self) -> String {
        format!(
            "Approved plan reference:\nplan_id: {}\nplan_revision: {}\n{BODY_OPEN_MARKER}\n{}\n{BODY_CLOSE_MARKER}",
            escape_markup(&self.approved_plan.id),
            self.approved_plan.revision,
            escape_markup(&self.body),
        )
    }
}

impl ContextualUserFragment for PlanLoaded {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("plan.loaded".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (OPEN_MARKER, CLOSE_MARKER)
    }

    fn body(&self) -> String {
        self.rendered_body()
    }
}

fn escape_markup(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unescape_markup(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
#[path = "plan_loaded_tests.rs"]
mod tests;
