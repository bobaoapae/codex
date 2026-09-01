use std::path::PathBuf;

use super::MutationIntent;
use super::classify_command_at_depth;
use super::executable_key;
use super::mutation_intent_git::classify_git;

pub(super) fn classify_direct_command(command: &[String], depth: usize) -> MutationIntent {
    let Some(name) = executable_key(&command[0]) else {
        return MutationIntent::RequiresCheckoutLease;
    };
    match name.as_str() {
        "env" => classify_env(command, depth),
        "sudo" | "doas" => classify_privilege_wrapper(command, depth),
        "git" => classify_git(&command[1..]),
        "bash" | "sh" | "zsh" | "cmd" | "pwsh" | "powershell" => {
            MutationIntent::RequiresCheckoutLease
        }
        name if is_powershell_command_name(name) => classify_powershell_words(command),
        name if is_read_only_command(name) => {
            if has_shell_control_token(&command[1..]) {
                MutationIntent::RequiresCheckoutLease
            } else if name == "find" {
                if find_can_mutate(&command[1..]) {
                    MutationIntent::RequiresCheckoutLease
                } else {
                    classify_find_output(&command[1..])
                }
            } else if name == "sort" {
                classify_sort_output(&command[1..])
            } else if name == "sed" && sed_can_mutate(&command[1..]) {
                classify_sed_mutation(&command[1..])
            } else {
                MutationIntent::ReadOnly
            }
        }
        name if is_filesystem_mutator(name) => classify_filesystem_mutator(name, &command[1..]),
        _ => MutationIntent::RequiresCheckoutLease,
    }
}

fn classify_env(command: &[String], depth: usize) -> MutationIntent {
    let mut index = 1;
    while let Some(argument) = command.get(index) {
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(argument.as_str(), "-i" | "--ignore-environment")
            || argument.split_once('=').is_some_and(|(name, value)| {
                !name.is_empty() && !name.starts_with('-') && !value.is_empty()
            })
        {
            index += 1;
            continue;
        }
        break;
    }
    if index == command.len() {
        MutationIntent::ReadOnly
    } else {
        classify_command_at_depth(&command[index..], depth + 1)
    }
}

fn classify_privilege_wrapper(command: &[String], depth: usize) -> MutationIntent {
    let mut index = 1;
    while let Some(argument) = command.get(index) {
        if argument == "--" {
            index += 1;
            break;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    if index == command.len() {
        MutationIntent::RequiresCheckoutLease
    } else {
        classify_command_at_depth(&command[index..], depth + 1)
    }
}

fn classify_powershell_words(command: &[String]) -> MutationIntent {
    let Some(name) = command.first().and_then(|arg| executable_key(arg)) else {
        return MutationIntent::RequiresCheckoutLease;
    };
    if name.starts_with("get-")
        || name.starts_with("select-")
        || name.starts_with("measure-")
        || name.starts_with("where-")
        || name.starts_with("format-")
        || name.starts_with("resolve-")
        || name.starts_with("test-")
        || matches!(
            name.as_str(),
            "write-output" | "write-host" | "convertto-json"
        )
    {
        MutationIntent::ReadOnly
    } else if powershell_mutator_name(&name) {
        classify_filesystem_mutator(&name, &command[1..])
    } else {
        MutationIntent::RequiresCheckoutLease
    }
}

fn classify_filesystem_mutator(name: &str, args: &[String]) -> MutationIntent {
    if args.iter().any(|arg| has_dynamic_path_syntax(arg)) {
        return MutationIntent::RequiresCheckoutLease;
    }
    let paths = if powershell_mutator_name(name) {
        powershell_paths(args)
    } else {
        unix_mutator_paths(name, args)
    };
    let Some(paths) = paths else {
        return MutationIntent::RequiresCheckoutLease;
    };
    if paths.is_empty() {
        MutationIntent::RequiresCheckoutLease
    } else {
        MutationIntent::WritesKnownPaths(dedup_paths(paths))
    }
}

fn unix_mutator_paths(name: &str, args: &[String]) -> Option<Vec<PathBuf>> {
    let mut operands = Vec::new();
    let mut target_directory = None;
    let mut index = 0;
    let mut after_separator = false;
    while let Some(argument) = args.get(index) {
        if after_separator || !argument.starts_with('-') || argument == "-" {
            operands.push(PathBuf::from(argument));
            index += 1;
            continue;
        }
        if argument == "--" {
            after_separator = true;
            index += 1;
            continue;
        }
        let (option, attached) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(option, value)| {
                (option, Some(value))
            });
        let takes_value = match name {
            "touch" => matches!(option, "-d" | "--date" | "-t"),
            "mkdir" => matches!(option, "-m" | "--mode" | "--context"),
            "cp" | "mv" | "install" | "ln" => {
                matches!(option, "-t" | "--target-directory" | "-S" | "--suffix")
            }
            "truncate" => matches!(option, "-s" | "--size"),
            _ => false,
        };
        if takes_value {
            if attached.is_none() && args.get(index + 1).is_none() {
                return None;
            }
            if matches!(name, "cp" | "mv" | "install" | "ln")
                && matches!(option, "-t" | "--target-directory")
            {
                target_directory = Some(PathBuf::from(
                    attached
                        .map(str::to_owned)
                        .or_else(|| args.get(index + 1).cloned())?,
                ));
            }
            index += if attached.is_some() { 1 } else { 2 };
            continue;
        }

        // GNU coreutils also accepts the short target-directory option with an attached value,
        // e.g. `cp -tdest source`. Keep the value out of the source operands.
        if matches!(name, "cp" | "mv" | "install" | "ln")
            && argument.starts_with('-')
            && !argument.starts_with("--")
        {
            let flags = &argument[1..];
            if let Some(target_flag) = flags.find('t') {
                let attached = &flags[target_flag + 1..];
                target_directory = if attached.is_empty() {
                    Some(PathBuf::from(args.get(index + 1)?))
                } else {
                    Some(PathBuf::from(
                        attached.strip_prefix('=').unwrap_or(attached),
                    ))
                };
                index += if attached.is_empty() { 2 } else { 1 };
                continue;
            }
        }
        let known = match name {
            "touch" => matches!(option, "-a" | "-m" | "-c" | "--no-create"),
            "mkdir" => matches!(option, "-p" | "--parents" | "-v" | "--verbose"),
            "cp" | "mv" | "install" | "ln" => matches!(
                option,
                "-a" | "-b"
                    | "-d"
                    | "-f"
                    | "-i"
                    | "-L"
                    | "-n"
                    | "-P"
                    | "-p"
                    | "-r"
                    | "-R"
                    | "-s"
                    | "-T"
                    | "-u"
                    | "-v"
                    | "--backup"
                    | "--directory"
                    | "--no-clobber"
                    | "--no-target-directory"
                    | "--preserve"
                    | "--symbolic"
                    | "--verbose"
            ),
            "rm" | "rmdir" | "del" | "erase" | "rd" => matches!(
                option,
                "-d" | "-f"
                    | "-i"
                    | "-I"
                    | "-r"
                    | "-R"
                    | "-v"
                    | "--dir"
                    | "--force"
                    | "--interactive"
                    | "--recursive"
                    | "--verbose"
            ),
            "chmod" | "chown" | "chgrp" => matches!(
                option,
                "-f" | "-h"
                    | "-R"
                    | "--changes"
                    | "--dereference"
                    | "--no-dereference"
                    | "--recursive"
                    | "--silent"
                    | "--verbose"
            ),
            "tee" => matches!(option, "-a" | "--append" | "-i" | "--ignore-interrupts"),
            "truncate" => matches!(option, "-c" | "--no-create"),
            _ => false,
        };
        if !known {
            return None;
        }
        index += 1;
    }
    let paths = match name {
        "cp" | "install" | "ln" => {
            if let Some(target_directory) = target_directory {
                if operands.is_empty() {
                    return None;
                }
                vec![target_directory]
            } else {
                vec![operands.last()?.clone()]
            }
        }
        "mv" => {
            let target_directory_specified = target_directory.is_some();
            let destination = target_directory.or_else(|| operands.last().cloned());
            if operands.is_empty() || (!target_directory_specified && operands.len() < 2) {
                return None;
            }
            let mut paths = vec![destination?];
            if target_directory_specified {
                paths.extend(operands);
            } else {
                let source_count = operands.len().saturating_sub(1);
                paths.extend(operands.into_iter().take(source_count));
            }
            paths
        }
        "chmod" | "chown" | "chgrp" => {
            if operands.len() < 2 {
                return None;
            }
            operands.into_iter().skip(1).collect()
        }
        _ => operands,
    };
    Some(paths)
}

fn powershell_paths(args: &[String]) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut positional = Vec::new();
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if !argument.starts_with('-') {
            positional.push(argument.clone());
            index += 1;
            continue;
        }
        let (parameter, attached) = argument
            .split_once(':')
            .map_or((argument.as_str(), None), |(key, value)| (key, Some(value)));
        let key = parameter.to_ascii_lowercase();
        let is_path = matches!(
            key.as_str(),
            "-path" | "-literalpath" | "-destination" | "-target" | "-filepath" | "-name"
        );
        let takes_value = is_path
            || matches!(
                key.as_str(),
                "-value"
                    | "-itemtype"
                    | "-filter"
                    | "-include"
                    | "-exclude"
                    | "-force"
                    | "-recurse"
                    | "-depth"
                    | "-encoding"
                    | "-property"
                    | "-whatif"
                    | "-confirm"
            );
        if !takes_value {
            return None;
        }
        if is_path {
            paths.push(PathBuf::from(
                attached
                    .map(str::to_owned)
                    .or_else(|| args.get(index + 1).cloned())?,
            ));
        }
        index += if attached.is_some() { 1 } else { 2 };
    }
    if paths.is_empty() {
        paths.extend(positional.into_iter().map(PathBuf::from));
    }
    Some(paths)
}

fn classify_sort_output(args: &[String]) -> MutationIntent {
    let mut output_paths = Vec::new();
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            break;
        }
        if argument == "-o" || argument == "--output" {
            let Some(path) = args.get(index + 1) else {
                return MutationIntent::RequiresCheckoutLease;
            };
            output_paths.push(PathBuf::from(path));
            index += 2;
            continue;
        }
        if let Some(path) = argument.strip_prefix("--output=") {
            if path.is_empty() {
                return MutationIntent::RequiresCheckoutLease;
            }
            output_paths.push(PathBuf::from(path));
            index += 1;
            continue;
        }
        if let Some(path) = argument.strip_prefix("-o")
            && !path.is_empty()
        {
            output_paths.push(PathBuf::from(path.strip_prefix('=').unwrap_or(path)));
            index += 1;
            continue;
        }
        index += 1;
    }
    if output_paths
        .iter()
        .any(|path| has_dynamic_path_syntax(&path.to_string_lossy()))
    {
        MutationIntent::RequiresCheckoutLease
    } else if output_paths.is_empty() {
        MutationIntent::ReadOnly
    } else {
        MutationIntent::WritesKnownPaths(dedup_paths(output_paths))
    }
}

fn classify_find_output(args: &[String]) -> MutationIntent {
    let mut output_paths = Vec::new();
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        let (predicate, attached) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(predicate, path)| {
                (predicate, Some(path))
            });
        if matches!(predicate, "-fprint" | "-fprint0" | "-fls" | "-fprintf") {
            let Some(path) = attached.or_else(|| args.get(index + 1).map(String::as_str)) else {
                return MutationIntent::RequiresCheckoutLease;
            };
            if path != "-" {
                output_paths.push(PathBuf::from(path));
            }
            // `-fprintf` consumes a format operand after its output path. It is not a path and
            // must not be mistaken for another find expression.
            index += if attached.is_some() { 1 } else { 2 };
            if predicate == "-fprintf" {
                if args.get(index).is_none() {
                    return MutationIntent::RequiresCheckoutLease;
                }
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    if output_paths
        .iter()
        .any(|path| has_dynamic_path_syntax(&path.to_string_lossy()))
    {
        MutationIntent::RequiresCheckoutLease
    } else if output_paths.is_empty() {
        MutationIntent::ReadOnly
    } else {
        MutationIntent::WritesKnownPaths(dedup_paths(output_paths))
    }
}

fn sed_can_mutate(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "-i"
            || arg == "--in-place"
            || arg.starts_with("-i")
            || arg.starts_with("--in-place=")
    })
}

fn classify_sed_mutation(args: &[String]) -> MutationIntent {
    let paths = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty()
        || paths
            .iter()
            .any(|path| has_dynamic_path_syntax(&path.to_string_lossy()))
    {
        MutationIntent::RequiresCheckoutLease
    } else {
        MutationIntent::WritesKnownPaths(dedup_paths(paths))
    }
}

fn is_powershell_command_name(name: &str) -> bool {
    name.starts_with("get-")
        || name.starts_with("set-")
        || name.starts_with("new-")
        || name.starts_with("remove-")
        || name.starts_with("move-")
        || name.starts_with("copy-")
        || name.starts_with("rename-")
        || name.starts_with("clear-")
        || name.starts_with("add-")
        || name.starts_with("update-")
        || name.starts_with("write-")
        || name.starts_with("select-")
        || name.starts_with("measure-")
        || name.starts_with("where-")
        || name.starts_with("format-")
        || name.starts_with("resolve-")
        || name.starts_with("test-")
        || matches!(name, "convertto-json" | "out-file")
}

fn powershell_mutator_name(name: &str) -> bool {
    name.starts_with("set-")
        || name.starts_with("new-")
        || name.starts_with("remove-")
        || name.starts_with("move-")
        || name.starts_with("copy-")
        || name.starts_with("rename-")
        || name.starts_with("clear-")
        || name.starts_with("add-")
        || name.starts_with("update-")
        || name == "out-file"
}

fn is_read_only_command(name: &str) -> bool {
    matches!(
        name,
        "ls" | "dir"
            | "pwd"
            | "cd"
            | "cat"
            | "type"
            | "head"
            | "tail"
            | "rg"
            | "grep"
            | "egrep"
            | "fgrep"
            | "ag"
            | "ack"
            | "pt"
            | "rga"
            | "find"
            | "fd"
            | "eza"
            | "exa"
            | "tree"
            | "du"
            | "bat"
            | "batcat"
            | "less"
            | "more"
            | "echo"
            | "printf"
            | "true"
            | "false"
            | "whoami"
            | "which"
            | "where"
            | "whereis"
            | "stat"
            | "file"
            | "wc"
            | "sort"
            | "uniq"
            | "diff"
            | "cmp"
            | "uname"
            | "date"
            | "hostname"
            | "id"
            | "basename"
            | "dirname"
            | "readlink"
            | "realpath"
            | "sed"
    )
}

fn is_filesystem_mutator(name: &str) -> bool {
    matches!(
        name,
        "touch"
            | "mkdir"
            | "mktemp"
            | "cp"
            | "mv"
            | "install"
            | "ln"
            | "rm"
            | "rmdir"
            | "del"
            | "erase"
            | "rd"
            | "chmod"
            | "chown"
            | "chgrp"
            | "tee"
            | "truncate"
    )
}

fn find_can_mutate(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
        )
    })
}

fn has_shell_control_token(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "|" | "||" | "&" | "&&" | ";" | ">" | ">>" | "<" | "2>" | "2>>" | "2>&1"
        ) || arg.starts_with('>')
            || arg.starts_with('<')
    })
}

fn has_dynamic_path_syntax(path: &str) -> bool {
    path.is_empty()
        || path == "-"
        || path.starts_with('~')
        || path.contains(['$', '%', '`', '*', '?', '[', ']', '(', ')'])
}

pub(super) fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduplicated = Vec::with_capacity(paths.len());
    for path in paths {
        if !deduplicated.contains(&path) {
            deduplicated.push(path);
        }
    }
    deduplicated
}
