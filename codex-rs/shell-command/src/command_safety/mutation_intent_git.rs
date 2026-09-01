use super::MutationIntent;

pub(super) fn classify_git(args: &[String]) -> MutationIntent {
    let config_risk = git_config_override_risk(args);
    if config_risk == ConfigRisk::None
        && args.len() == 1
        && matches!(args[0].as_str(), "--version" | "-v" | "--help" | "-h")
    {
        return MutationIntent::ReadOnly;
    }
    let Some((verb, rest)) = git_subcommand(args) else {
        return match config_risk {
            ConfigRisk::Destructive => MutationIntent::DestructiveGit {
                verb: "config".to_string(),
            },
            ConfigRisk::None | ConfigRisk::Requires => MutationIntent::RequiresCheckoutLease,
        };
    };

    let command_intent = if is_destructive_git_verb(&verb) {
        MutationIntent::DestructiveGit { verb }
    } else {
        match verb.as_str() {
            "status" | "diff" | "log" | "show" | "grep" | "ls-files" | "rev-parse" | "cat-file"
            | "describe" | "shortlog" | "whatchanged" | "blame" | "show-ref" | "verify-commit"
            | "verify-tag" | "version" | "help" | "count-objects" => {
                if git_output_can_write(rest) {
                    MutationIntent::RequiresCheckoutLease
                } else {
                    MutationIntent::ReadOnly
                }
            }
            "branch" => {
                if has_long_option(rest, "--delete")
                    || has_short_option(rest, 'd')
                    || has_short_option(rest, 'D')
                {
                    MutationIntent::DestructiveGit { verb: verb.clone() }
                } else if is_branch_read_only(rest) {
                    MutationIntent::ReadOnly
                } else {
                    MutationIntent::RequiresCheckoutLease
                }
            }
            "tag" => {
                if rest.iter().any(|arg| {
                    matches!(
                        arg.as_str(),
                        "-d" | "-a"
                            | "-s"
                            | "-u"
                            | "-f"
                            | "--delete"
                            | "--annotate"
                            | "--sign"
                            | "--force"
                    )
                }) {
                    MutationIntent::RequiresCheckoutLease
                } else if is_tag_read_only(rest) {
                    MutationIntent::ReadOnly
                } else {
                    MutationIntent::RequiresCheckoutLease
                }
            }
            "remote" => {
                if rest.is_empty() || rest.iter().all(|arg| arg == "-v" || arg == "--verbose") {
                    MutationIntent::ReadOnly
                } else {
                    MutationIntent::RequiresCheckoutLease
                }
            }
            "config" => classify_git_config(rest),
            "worktree" => {
                if matches!(git_nested_subcommand(rest), Some("remove" | "prune")) {
                    MutationIntent::DestructiveGit { verb: verb.clone() }
                } else if git_nested_subcommand(rest) == Some("list") {
                    MutationIntent::ReadOnly
                } else {
                    MutationIntent::RequiresCheckoutLease
                }
            }
            "switch"
                if has_long_option(rest, "--discard-changes") || has_short_option(rest, 'f') =>
            {
                MutationIntent::DestructiveGit { verb: verb.clone() }
            }
            "rm" => MutationIntent::DestructiveGit { verb: verb.clone() },
            "update-ref" if update_ref_deletes(rest) => {
                MutationIntent::DestructiveGit { verb: verb.clone() }
            }
            "reflog"
                if matches!(
                    git_nested_subcommand(rest),
                    Some("expire" | "delete" | "drop")
                ) =>
            {
                MutationIntent::DestructiveGit { verb: verb.clone() }
            }
            "gc" if has_long_option(rest, "--prune") => {
                MutationIntent::DestructiveGit { verb: verb.clone() }
            }
            _ => MutationIntent::RequiresCheckoutLease,
        }
    };

    match config_risk {
        ConfigRisk::Destructive
            if !matches!(command_intent, MutationIntent::DestructiveGit { .. }) =>
        {
            MutationIntent::DestructiveGit {
                verb: "config".to_string(),
            }
        }
        ConfigRisk::Requires if matches!(command_intent, MutationIntent::ReadOnly) => {
            MutationIntent::RequiresCheckoutLease
        }
        _ => command_intent,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigRisk {
    None,
    Requires,
    Destructive,
}

impl ConfigRisk {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Destructive, _) | (_, Self::Destructive) => Self::Destructive,
            (Self::Requires, _) | (_, Self::Requires) => Self::Requires,
            _ => Self::None,
        }
    }
}

fn has_long_option(arguments: &[String], option: &str) -> bool {
    arguments
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| argument == option || argument.starts_with(&format!("{option}=")))
}

fn has_short_option(arguments: &[String], option: char) -> bool {
    arguments
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .filter_map(|argument| argument.strip_prefix('-'))
        .filter(|flags| !flags.starts_with('-'))
        .any(|flags| flags.chars().any(|flag| flag == option))
}

fn git_nested_subcommand(arguments: &[String]) -> Option<&str> {
    arguments
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .find(|argument| !argument.starts_with('-'))
        .map(String::as_str)
}

fn update_ref_deletes(arguments: &[String]) -> bool {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" {
            return false;
        }
        if argument == "-d" || argument == "--delete" || argument.starts_with("--delete=") {
            return true;
        }
        if argument == "-m" || argument == "--message" {
            index += 2;
            continue;
        }
        if argument.starts_with("--message=") || argument.starts_with("-m") {
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        return argument == "delete";
    }
    false
}

fn git_config_override_risk(arguments: &[String]) -> ConfigRisk {
    let mut risk = ConfigRisk::None;
    let mut subcommand_seen = false;
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        match argument.as_str() {
            "-c" | "--config-env" => {
                let Some(specification) = arguments.get(index + 1) else {
                    if argument == "-c" && subcommand_seen {
                        index += 1;
                        continue;
                    }
                    return ConfigRisk::Requires;
                };
                if argument == "--config-env" || specification.contains('=') || !subcommand_seen {
                    risk = risk.combine(config_specification_risk(specification));
                }
                index += 2;
            }
            _ if argument.starts_with("--config-env=") => {
                let Some(specification) = argument.strip_prefix("--config-env=") else {
                    return ConfigRisk::Requires;
                };
                risk = risk.combine(config_specification_risk(specification));
                index += 1;
            }
            _ if argument.starts_with("-c") && argument.len() > 2 => {
                let specification = &argument[2..];
                risk = risk.combine(config_specification_risk(specification));
                index += 1;
            }
            "-C" | "--work-tree" | "--git-dir" | "--namespace" | "--exec-path"
            | "--super-prefix" => {
                if arguments.get(index + 1).is_none() {
                    return ConfigRisk::Requires;
                }
                index += 2;
            }
            _ if argument.starts_with("-C") && argument.len() > 2 => index += 1,
            _ if argument.starts_with("--work-tree=")
                || argument.starts_with("--git-dir=")
                || argument.starts_with("--namespace=")
                || argument.starts_with("--exec-path=")
                || argument.starts_with("--super-prefix=") =>
            {
                index += 1
            }
            _ if argument.starts_with('-') => index += 1,
            _ => {
                subcommand_seen = true;
                index += 1;
            }
        }
    }
    risk
}

fn config_specification_risk(specification: &str) -> ConfigRisk {
    let Some((key, value)) = specification.split_once('=') else {
        return ConfigRisk::Requires;
    };
    if key.is_empty() || value.is_empty() {
        return ConfigRisk::Requires;
    }
    let key = key.to_ascii_lowercase();
    if value.trim_start().starts_with('!') || is_executable_config_key(&key) {
        ConfigRisk::Destructive
    } else {
        // A config override can redirect Git's files, hooks, pager, transport, or filters. Even
        // an apparently harmless key therefore cannot be allowed to collapse a command to
        // ReadOnly without consulting checkout ownership.
        ConfigRisk::Requires
    }
}

fn is_executable_config_key(key: &str) -> bool {
    key == "core.editor"
        || key == "core.fsmonitor"
        || key == "core.fsmonitorhook"
        || key == "core.gitproxy"
        || key == "core.hookspath"
        || key == "core.askpass"
        || key == "core.pager"
        || key == "core.sshcommand"
        || key == "credential.helper"
        || key == "diff.external"
        || key == "interactive.difffilter"
        || key == "gpg.program"
        || key == "gpg.ssh.program"
        || key == "sequence.editor"
        || key == "pager"
        || key.starts_with("pager.")
        || key.starts_with("filter.")
        || (key.starts_with("credential.") && key.ends_with(".helper"))
        || (key.starts_with("diff.")
            && (key.ends_with(".external")
                || key.ends_with(".command")
                || key.ends_with(".textconv")))
        || (key.starts_with("merge.") && key.ends_with(".driver"))
        || (key.starts_with("mergetool.") && (key.ends_with(".cmd") || key.ends_with(".path")))
        || (key.starts_with("difftool.") && (key.ends_with(".cmd") || key.ends_with(".path")))
        || (key.starts_with("remote.")
            && (key.ends_with(".uploadpack") || key.ends_with(".receivepack")))
        || (key.starts_with("submodule.") && key.ends_with(".update"))
}

fn is_branch_read_option(argument: &str) -> bool {
    matches!(
        argument,
        "-a" | "--all"
            | "-r"
            | "--remotes"
            | "-v"
            | "-vv"
            | "--verbose"
            | "--no-abbrev"
            | "--contains"
            | "--no-contains"
            | "--merged"
            | "--no-merged"
            | "--points-at"
            | "--show-current"
            | "--list"
            | "-l"
    ) || argument.starts_with("--format=")
        || argument.starts_with("--sort=")
        || argument.starts_with("--column=")
        || argument.starts_with("--color=")
}

fn is_branch_read_only(arguments: &[String]) -> bool {
    let mut list_mode = false;
    for argument in arguments {
        if is_branch_read_option(argument) {
            list_mode = true;
        } else if argument.starts_with('-') || !list_mode {
            return false;
        }
    }
    true
}

fn is_tag_read_option(argument: &str) -> bool {
    matches!(
        argument,
        "-l" | "--list" | "--contains" | "--no-contains" | "--merged" | "--no-merged"
    ) || argument.starts_with("--sort=")
        || argument.starts_with("--format=")
        || argument.starts_with("--column=")
        || argument.starts_with("--color=")
}

fn is_tag_list_selector(argument: &str) -> bool {
    matches!(
        argument,
        "-l" | "--list"
            | "--contains"
            | "--no-contains"
            | "--merged"
            | "--no-merged"
            | "--points-at"
    ) || argument.starts_with("--sort=")
        || argument.starts_with("--format=")
        || argument.starts_with("--column=")
        || argument.starts_with("--color=")
}

fn is_tag_read_only(arguments: &[String]) -> bool {
    let mut list_mode = false;
    for argument in arguments {
        if is_tag_list_selector(argument) || is_tag_read_option(argument) {
            list_mode = true;
        } else if argument.starts_with('-') || !list_mode {
            return false;
        }
    }
    true
}

fn git_subcommand(args: &[String]) -> Option<(String, &[String])> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if matches!(
            argument.as_str(),
            "-C" | "-c"
                | "--work-tree"
                | "--git-dir"
                | "--config-env"
                | "--namespace"
                | "--exec-path"
                | "--super-prefix"
        ) {
            args.get(index + 1)?;
            index += 2;
            continue;
        }
        if argument.starts_with("-C")
            || argument.starts_with("--work-tree=")
            || argument.starts_with("--git-dir=")
            || argument.starts_with("--config-env=")
            || argument.starts_with("--namespace=")
            || argument.starts_with("--exec-path=")
            || argument.starts_with("--super-prefix=")
            || (argument.starts_with("-c") && argument.len() > 2)
        {
            index += 1;
            continue;
        }
        if matches!(
            argument.as_str(),
            "--paginate"
                | "--no-pager"
                | "--no-replace-objects"
                | "--bare"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
        ) {
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            return None;
        }
        return Some((argument.to_ascii_lowercase(), &args[index + 1..]));
    }
    None
}

fn classify_git_config(args: &[String]) -> MutationIntent {
    let mut read_mode = false;
    let mut positional_count = 0;
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            positional_count += args.len().saturating_sub(index + 1);
            break;
        }
        if !argument.starts_with('-') {
            positional_count += 1;
            index += 1;
            continue;
        }
        match argument.as_str() {
            "--unset" | "--unset-all" | "--rename-section" | "--remove-section" | "--add"
            | "--replace-all" | "--edit" | "-e" | "--stdin" => {
                return MutationIntent::RequiresCheckoutLease;
            }
            "--list" | "-l" | "--get" | "--get-all" | "--get-regexp" | "--get-urlmatch"
            | "--get-color" | "--get-colorbool" | "--name-only" | "--show-origin"
            | "--show-scope" | "--null" | "-z" | "--includes" | "--no-includes" | "--global"
            | "--local" | "--system" | "--worktree" | "--fixed-value" | "--default" => {
                read_mode = true
            }
            "--file" | "-f" | "--blob" | "--type" => {
                if args.get(index + 1).is_none() {
                    return MutationIntent::RequiresCheckoutLease;
                }
                index += 1;
            }
            _ if argument.starts_with("--file=")
                || argument.starts_with("--blob=")
                || argument.starts_with("--type=")
                || argument.starts_with("--default=") => {}
            _ => return MutationIntent::RequiresCheckoutLease,
        }
        index += 1;
    }
    if read_mode || positional_count <= 1 {
        MutationIntent::ReadOnly
    } else {
        MutationIntent::RequiresCheckoutLease
    }
}

fn git_output_can_write(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-o" | "--output"
                | "--output-directory"
                | "--format=%(raw)"
                | "--exec"
                | "--ext-diff"
                | "--textconv"
        ) || arg.starts_with("--output=")
    })
}

fn is_destructive_git_verb(verb: &str) -> bool {
    matches!(
        verb,
        "reset" | "checkout" | "clean" | "restore" | "revert" | "stash"
    )
}
