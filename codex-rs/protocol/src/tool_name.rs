use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::fmt;

/// Namespace used for top-level function and custom tools.
pub const DEFAULT_FUNCTION_NAMESPACE: &str = "functions";

/// Tool names whose arguments may contain inter-agent message contents.
///
/// The list lives at the protocol boundary so core dispatch, telemetry, and
/// rollout tracing apply the same redaction policy without depending on one
/// another.
pub const SENSITIVE_MULTI_AGENT_TOOL_NAMES: &[&str] = &[
    "spawn_agent",
    "send_message",
    "followup_task",
    "send_input",
    "assign_task",
];

/// Returns whether a tool's arguments may contain inter-agent message text.
pub fn is_sensitive_multi_agent_tool(name: &str) -> bool {
    SENSITIVE_MULTI_AGENT_TOOL_NAMES.contains(&name)
}

/// Identifies a callable tool, preserving the namespace split when the model
/// provides one.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ToolName {
    pub name: String,
    pub namespace: Option<String>,
}

impl ToolName {
    pub fn new(namespace: Option<String>, name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace,
        }
    }

    pub fn plain(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: None,
        }
    }

    pub fn namespaced(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: Some(namespace.into()),
        }
    }

    pub fn with_default_namespace(mut self) -> Self {
        if self.namespace.as_deref().is_none_or(str::is_empty) {
            self.namespace = Some(DEFAULT_FUNCTION_NAMESPACE.to_string());
        }
        self
    }

    pub fn is_default_namespace(&self) -> bool {
        matches!(
            self.namespace.as_deref(),
            None | Some("") | Some(DEFAULT_FUNCTION_NAMESPACE)
        )
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            Some(_) if self.is_default_namespace() => f.write_str(&self.name),
            Some(namespace) => write!(f, "{namespace}{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

impl Ord for ToolName {
    fn cmp(&self, other: &Self) -> Ordering {
        let lhs = match &self.namespace {
            Some(namespace) => (namespace.as_str(), Some(self.name.as_str())),
            None => (self.name.as_str(), None),
        };
        let rhs = match &other.namespace {
            Some(namespace) => (namespace.as_str(), Some(other.name.as_str())),
            None => (other.name.as_str(), None),
        };
        lhs.cmp(&rhs)
    }
}

impl PartialOrd for ToolName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<String> for ToolName {
    fn from(name: String) -> Self {
        Self::plain(name)
    }
}

impl From<&str> for ToolName {
    fn from(name: &str) -> Self {
        Self::plain(name)
    }
}
