use std::path::PathBuf;

#[path = "mutation_intent_git.rs"]
mod mutation_intent_git;
#[path = "mutation_intent_paths.rs"]
mod mutation_intent_paths;
#[path = "mutation_intent_shell.rs"]
mod mutation_intent_shell;

const MAX_WRAPPER_DEPTH: usize = 8;

/// Describes the checkout capability required by a tokenized command.
///
/// The classifier is deliberately conservative: an unrecognized command or a
/// shell construct whose runtime argv cannot be proven is a checkout lease
/// requirement. Paths are kept as host-local `PathBuf`s because this type is
/// consumed by local command enforcement, not exposed on an app-server wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationIntent {
    /// The command is proven not to mutate the checkout.
    ReadOnly,
    /// The command writes only the listed, statically known paths.
    WritesKnownPaths(Vec<PathBuf>),
    /// The command may write paths that cannot be proven from argv.
    RequiresCheckoutLease,
    /// Git changes checkout state in a destructive way.
    DestructiveGit { verb: String },
}

/// Classifies an already-tokenized command without executing it.
pub fn classify_command(command: &[String]) -> MutationIntent {
    classify_command_at_depth(command, 0)
}

/// Alias with a name that makes the policy purpose explicit at call sites.
pub fn classify_mutation_intent(command: &[String]) -> MutationIntent {
    classify_command(command)
}

pub(super) fn classify_command_at_depth(command: &[String], depth: usize) -> MutationIntent {
    if command.is_empty() || depth > MAX_WRAPPER_DEPTH {
        return MutationIntent::RequiresCheckoutLease;
    }
    if let Some(intent) = mutation_intent_shell::classify_shell(command, depth) {
        return intent;
    }
    mutation_intent_paths::classify_direct_command(command, depth)
}

pub(super) fn executable_key(raw: &str) -> Option<String> {
    let component = raw.rsplit(['/', '\\']).next()?;
    let mut key = component.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stripped) = key.strip_suffix(suffix) {
            key = stripped.to_string();
            break;
        }
    }
    (!key.is_empty()).then_some(key)
}

#[cfg(test)]
#[path = "mutation_intent_tests.rs"]
mod tests;
