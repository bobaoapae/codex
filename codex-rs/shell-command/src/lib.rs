//! Command parsing and safety utilities shared across Codex crates.

pub mod shell_detect;
pub mod shell_snapshot;

pub mod bash;
pub(crate) mod command_safety;
pub mod parse_command;
pub mod powershell;

pub use command_safety::is_dangerous_command;
pub use command_safety::mutation_intent::MutationIntent;
pub use command_safety::mutation_intent::classify_command;
pub use command_safety::mutation_intent::classify_mutation_intent;
