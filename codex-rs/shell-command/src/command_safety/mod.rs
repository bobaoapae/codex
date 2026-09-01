// Keep the PowerShell subprocess parser available as a test oracle, but do not
// compile it into production command classification.
pub(crate) mod mutation_intent;
#[cfg(test)]
#[allow(dead_code)]
mod powershell_parser;
mod powershell_tree_sitter;

pub mod is_dangerous_command;
pub(crate) use powershell_tree_sitter::try_parse_powershell_commands;
