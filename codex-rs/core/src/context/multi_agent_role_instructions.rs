use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiAgentRoleInstructions {
    text: String,
    marked: bool,
}

impl MultiAgentRoleInstructions {
    pub(crate) fn unmarked(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            marked: false,
        }
    }

    pub(crate) fn catalog(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            marked: true,
        }
    }

    /// FORK: the resolved text, so fork-owned sections can be appended to it.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// FORK: the same instructions with different text, keeping the marking.
    pub(crate) fn with_text(self, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            marked: self.marked,
        }
    }
}

impl ContextualUserFragment for MultiAgentRoleInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.role_instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        if self.marked {
            Self::type_markers()
        } else {
            ("", "")
        }
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<multi_agent_role>", "</multi_agent_role>")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}
