//! Explicit reader capability declarations for shared rollout compression.

/// Capability declaration for every reader that may open a Codex home.
///
/// `desktop` is optional because the installed Desktop reader cannot be inferred from this
/// library. A missing or unknown capability keeps shared rollouts in plain JSONL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RolloutCompressionCapabilities {
    /// Cargo-distributed readers can open compressed shared rollouts.
    pub cargo: bool,
    /// Bazel-distributed readers can open compressed shared rollouts.
    pub bazel: bool,
    /// TUI readers can open compressed shared rollouts.
    pub tui: bool,
    /// App-server readers can open compressed shared rollouts.
    pub app_server: bool,
    /// Desktop support is explicitly known only when set to `Some(true)`.
    pub desktop: Option<bool>,
}

impl RolloutCompressionCapabilities {
    /// A convenient explicit declaration for tests and controlled deployments.
    pub const fn all_readers() -> Self {
        Self {
            cargo: true,
            bazel: true,
            tui: true,
            app_server: true,
            desktop: Some(true),
        }
    }

    /// Returns whether `IncludeShared` is safe for this reader fleet.
    pub fn all_readers_support_shared(&self) -> bool {
        self.cargo && self.bazel && self.tui && self.app_server && self.desktop == Some(true)
    }

    /// Returns the explicit capability names that still block shared compression.
    pub fn missing_shared_readers(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.cargo {
            missing.push("Cargo");
        }
        if !self.bazel {
            missing.push("Bazel");
        }
        if !self.tui {
            missing.push("TUI");
        }
        if !self.app_server {
            missing.push("app-server");
        }
        if self.desktop != Some(true) {
            missing.push("Desktop");
        }
        missing
    }

    /// A local, redacted diagnostic suitable for logs or a CLI preview.
    pub fn shared_compression_diagnostic(&self) -> String {
        if self.all_readers_support_shared() {
            return "all configured readers support shared rollout compression".to_string();
        }
        format!(
            "shared rollout compression disabled; missing or unknown readers: {}",
            self.missing_shared_readers().join(", ")
        )
    }
}

#[cfg(test)]
#[path = "compression_capabilities_tests.rs"]
mod tests;
