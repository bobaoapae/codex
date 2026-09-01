use crate::bash::parse_shell_lc_plain_commands;
use crate::powershell::parse_powershell_script_into_plain_commands;

use super::MutationIntent;
use super::classify_command_at_depth;
use super::executable_key;
use super::mutation_intent_paths::dedup_paths;

pub(super) fn classify_shell(command: &[String], depth: usize) -> Option<MutationIntent> {
    if let Some((script, has_operator, requires_lease)) = bash_script(command) {
        return Some(classify_bash_script(
            &script,
            has_operator,
            requires_lease,
            depth,
        ));
    }
    if let Some(script) = powershell_script(command) {
        return Some(classify_script_commands(
            parse_powershell_script_into_plain_commands(&script),
            script_has_shell_operator(&script),
            depth,
        ));
    }
    cmd_script(command).map(|(script, requires_lease)| {
        let intent = classify_cmd_script(&script, depth);
        if requires_lease {
            MutationIntent::RequiresCheckoutLease
        } else {
            intent
        }
    })
}

fn classify_bash_script(
    script: &str,
    has_operator: bool,
    requires_lease: bool,
    depth: usize,
) -> MutationIntent {
    let normalized = vec!["bash".to_string(), "-c".to_string(), script.to_string()];
    if let Some(commands) = parse_shell_lc_plain_commands(&normalized) {
        let intent = classify_script_commands(Some(commands), has_operator, depth);
        return if requires_lease {
            MutationIntent::RequiresCheckoutLease
        } else {
            intent
        };
    }

    MutationIntent::RequiresCheckoutLease
}

fn classify_script_commands(
    commands: Option<Vec<Vec<String>>>,
    has_operator: bool,
    depth: usize,
) -> MutationIntent {
    let Some(commands) = commands else {
        return MutationIntent::RequiresCheckoutLease;
    };
    if commands.is_empty() {
        return MutationIntent::RequiresCheckoutLease;
    }
    let intents = commands
        .iter()
        .map(|command| classify_command_at_depth(command, depth + 1))
        .collect::<Vec<_>>();
    merge_intents(&intents, has_operator)
}

fn classify_cmd_script(script: &str, depth: usize) -> MutationIntent {
    let Some(words) = shlex::split(script) else {
        return MutationIntent::RequiresCheckoutLease;
    };
    if words.is_empty() {
        return MutationIntent::RequiresCheckoutLease;
    }
    let mut commands = Vec::new();
    let mut current = Vec::new();
    let mut has_operator = false;
    for word in words {
        if is_shell_operator(&word) {
            has_operator = true;
            if current.is_empty() {
                return MutationIntent::RequiresCheckoutLease;
            }
            commands.push(std::mem::take(&mut current));
        } else {
            current.push(word);
        }
    }
    if current.is_empty() {
        return MutationIntent::RequiresCheckoutLease;
    }
    commands.push(current);
    classify_script_commands(Some(commands), has_operator, depth)
}

fn merge_intents(intents: &[MutationIntent], has_operator: bool) -> MutationIntent {
    let mut paths = Vec::new();
    for intent in intents {
        match intent {
            MutationIntent::DestructiveGit { verb } => {
                return MutationIntent::DestructiveGit { verb: verb.clone() };
            }
            MutationIntent::RequiresCheckoutLease => {
                return MutationIntent::RequiresCheckoutLease;
            }
            MutationIntent::WritesKnownPaths(found) => paths.extend(found.iter().cloned()),
            MutationIntent::ReadOnly => {}
        }
    }
    if has_operator && !paths.is_empty() {
        MutationIntent::RequiresCheckoutLease
    } else if paths.is_empty() {
        MutationIntent::ReadOnly
    } else {
        MutationIntent::WritesKnownPaths(dedup_paths(paths))
    }
}

fn bash_script(command: &[String]) -> Option<(String, bool, bool)> {
    let name = executable_key(command.first()?)?;
    if !matches!(name.as_str(), "bash" | "sh" | "zsh") {
        return None;
    }
    let flag_index = command
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, argument)| matches!(argument.as_str(), "-c" | "-lc").then_some(index))?;
    let script = command.get(flag_index + 1)?.clone();
    let requires_lease = command.len() != flag_index + 2
        || command[..flag_index]
            .iter()
            .skip(1)
            .any(|argument| !matches!(argument.as_str(), "--noprofile" | "--norc" | "--login"));
    Some((
        script.clone(),
        script_has_shell_operator(&script),
        requires_lease,
    ))
}

fn powershell_script(command: &[String]) -> Option<String> {
    let name = executable_key(command.first()?)?;
    if !matches!(name.as_str(), "pwsh" | "powershell") {
        return None;
    }
    let mut index = 1;
    while index < command.len() {
        let argument = &command[index];
        if matches!(
            argument.to_ascii_lowercase().as_str(),
            "-nologo" | "-noprofile"
        ) {
            index += 1;
            continue;
        }
        if matches!(argument.to_ascii_lowercase().as_str(), "-command" | "-c") {
            return (index + 2 == command.len()).then(|| command[index + 1].clone());
        }
        return None;
    }
    None
}

fn cmd_script(command: &[String]) -> Option<(String, bool)> {
    let name = executable_key(command.first()?)?;
    if name != "cmd" {
        return None;
    }
    let switch_index = command
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, argument)| {
            matches!(argument.to_ascii_lowercase().as_str(), "/c" | "/r" | "/k").then_some(index)
        })?;
    let requires_lease = command[1..switch_index].iter().any(|argument| {
        !matches!(
            argument.to_ascii_lowercase().as_str(),
            "/d" | "/s"
                | "/q"
                | "/a"
                | "/u"
                | "/v:on"
                | "/v:off"
                | "/f:on"
                | "/f:off"
                | "/e:on"
                | "/e:off"
        )
    });
    if command[switch_index].eq_ignore_ascii_case("/k") {
        return Some((String::new(), true));
    }
    if switch_index + 1 >= command.len() {
        return Some((String::new(), true));
    }
    Some((command[switch_index + 1..].join(" "), requires_lease))
}

fn is_shell_operator(word: &str) -> bool {
    matches!(word, "|" | "||" | "&" | "&&" | ";")
}

fn script_has_shell_operator(script: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for character in script.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(current_quote) = quote {
            if character == current_quote {
                quote = None;
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if matches!(character, '|' | ';' | '&') {
            return true;
        }
    }
    false
}
