use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::MutationIntent;
use super::classify_command;

fn command(words: &[&str]) -> Vec<String> {
    words.iter().map(std::string::ToString::to_string).collect()
}

fn paths(paths: &[&str]) -> MutationIntent {
    MutationIntent::WritesKnownPaths(paths.iter().map(PathBuf::from).collect())
}

#[test]
fn preserves_proven_read_only_commands() {
    for words in [
        command(&["git", "status"]),
        command(&["git", "-C", "repo", "diff"]),
        command(&["rg", "-n", "pattern", "src"]),
        command(&["cat", "README.md"]),
        command(&["find", ".", "-name", "*.rs"]),
        command(&["git", "branch", "--list", "feature*"]),
        command(&["git", "tag", "--list", "v*"]),
        command(&["git", "config", "--global", "--list"]),
        command(&["git", "config", "--local", "--get", "user.name"]),
        command(&["git", "log", "-c"]),
        command(&["Get-Content", "README.md"]),
        command(&["Write-Output", "ok"]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::ReadOnly,
            "{words:?}"
        );
    }
}

#[test]
fn classifies_destructive_git_with_global_options_and_wrappers() {
    for (words, verb) in [
        (command(&["git", "reset", "--hard"]), "reset"),
        (
            command(&["git.exe", "-C", "repo", "reset", "--hard"]),
            "reset",
        ),
        (
            command(&[
                "git",
                "-c",
                "core.hooksPath=/tmp",
                "--work-tree=repo",
                "stash",
            ]),
            "stash",
        ),
        (
            command(&[
                r"C:\Program Files\Git\bin\git.exe",
                "--git-dir",
                "repo/.git",
                "checkout",
                "main",
            ]),
            "checkout",
        ),
        (command(&["bash", "-lc", "git reset --hard"]), "reset"),
        (command(&["sh", "-c", "git clean -fd"]), "clean"),
        (command(&["zsh", "-lc", "git stash"]), "stash"),
        (
            command(&["pwsh", "-NoProfile", "-Command", "git restore file"]),
            "restore",
        ),
        (command(&["cmd.exe", "/c", "git revert HEAD"]), "revert"),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::DestructiveGit {
                verb: verb.to_string()
            },
            "{words:?}"
        );
    }
}

#[test]
fn git_mutators_never_look_read_only() {
    for words in [
        command(&["git", "commit", "-m", "message"]),
        command(&["git", "add", "src/lib.rs"]),
        command(&["git", "mv", "old", "new"]),
        command(&["git", "merge", "feature"]),
        command(&["git", "pull"]),
        command(&["git", "branch", "feature"]),
        command(&["git", "tag", "release"]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::RequiresCheckoutLease,
            "{words:?}"
        );
    }
}

#[test]
fn returns_known_paths_for_simple_filesystem_mutations() {
    assert_eq!(
        classify_command(&command(&["touch", "one"])),
        paths(&["one"])
    );
    assert_eq!(
        classify_command(&command(&["mkdir", "-p", "build"])),
        paths(&["build"])
    );
    assert_eq!(classify_command(&command(&["cp", "a", "b"])), paths(&["b"]));
    assert_eq!(
        classify_command(&command(&["rm", "-f", "old"])),
        paths(&["old"])
    );
    assert_eq!(
        classify_command(&command(&["New-Item", "-Path", "output.txt"])),
        paths(&["output.txt"])
    );
    assert_eq!(
        classify_command(&command(&["Copy-Item", "-Path", "a", "-Destination", "b"])),
        paths(&["a", "b"])
    );
}

#[test]
fn dynamic_paths_and_unknown_commands_fail_closed() {
    for words in [
        command(&["touch", "$TARGET"]),
        command(&["rm", "%TARGET%"]),
        command(&["cp", "a", "$(target)"]),
        command(&["unknown-tool", "--version"]),
        command(&["git", "config", "user.name", "Joao"]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::RequiresCheckoutLease,
            "{words:?}"
        );
    }
}

#[test]
fn complex_shell_syntax_does_not_prove_write_scope() {
    for script in [
        "echo ok > output.txt",
        "touch output.txt | cat",
        "touch output.txt && cat README.md",
        "echo $(cat README.md)",
        "echo $(git reset --hard)",
        "eval 'cat README.md'",
    ] {
        assert_eq!(
            classify_command(&command(&["bash", "-lc", script])),
            MutationIntent::RequiresCheckoutLease,
            "{script}"
        );
    }
    assert_eq!(
        classify_command(&command(&["bash", "-i", "-c", "cat README.md"])),
        MutationIntent::RequiresCheckoutLease
    );
    assert_eq!(
        classify_command(&command(&["bash", "-i", "-c", "git reset --hard"])),
        MutationIntent::RequiresCheckoutLease
    );
}

#[test]
fn read_only_shell_pipelines_remain_read_only() {
    assert_eq!(
        classify_command(&command(&["bash", "-lc", "cat README.md | wc -l"])),
        MutationIntent::ReadOnly
    );
    assert_eq!(
        classify_command(&command(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "Get-Content README.md"
        ])),
        MutationIntent::ReadOnly
    );
    assert_eq!(
        classify_command(&command(&["cmd", "/c", "type README.md"])),
        MutationIntent::ReadOnly
    );
    assert_eq!(
        classify_command(&command(&["cmd", "/unknown", "/c", "type README.md"])),
        MutationIntent::RequiresCheckoutLease
    );
}

#[test]
fn wrapper_sequences_preserve_the_strongest_result() {
    assert_eq!(
        classify_command(&command(&["env", "GIT_DIR=repo/.git", "git", "reset"])),
        MutationIntent::DestructiveGit {
            verb: "reset".to_string()
        }
    );
    assert_eq!(
        classify_command(&command(&["sudo", "git", "checkout", "main"])),
        MutationIntent::DestructiveGit {
            verb: "checkout".to_string()
        }
    );
}

#[test]
fn classifies_destructive_git_subcommands() {
    for (words, verb) in [
        (
            command(&["git", "switch", "--discard-changes", "main"]),
            "switch",
        ),
        (command(&["git", "switch", "-f", "main"]), "switch"),
        (command(&["git", "branch", "-d", "old"]), "branch"),
        (command(&["git", "branch", "-D", "old"]), "branch"),
        (command(&["git", "branch", "--delete", "old"]), "branch"),
        (
            command(&["git", "worktree", "remove", "../old"]),
            "worktree",
        ),
        (
            command(&["git", "worktree", "remove", "--force", "../old"]),
            "worktree",
        ),
        (
            command(&["git", "worktree", "prune", "--expire", "now"]),
            "worktree",
        ),
        (command(&["git", "rm", "relative/file"]), "rm"),
        (
            command(&["git", "update-ref", "-d", "refs/heads/old"]),
            "update-ref",
        ),
        (
            command(&["git", "update-ref", "delete", "refs/heads/old"]),
            "update-ref",
        ),
        (command(&["git", "reflog", "expire", "--all"]), "reflog"),
        (command(&["git", "gc", "--prune=now"]), "gc"),
        (command(&["git", "gc", "--prune", "now"]), "gc"),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::DestructiveGit {
                verb: verb.to_string()
            },
            "{words:?}"
        );
    }

    for words in [
        command(&["git", "switch", "main"]),
        command(&["git", "update-ref", "refs/heads/new", "HEAD"]),
        command(&["git", "gc"]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::RequiresCheckoutLease,
            "{words:?}"
        );
    }
}

#[test]
fn config_overrides_cannot_be_read_only() {
    for words in [
        command(&["git", "-c", "core.hooksPath=hooks", "status"]),
        command(&["git", "-c", "core.sshCommand=ssh", "status"]),
        command(&["git", "-c", "core.pager=cat", "status"]),
        command(&["git", "-c", "pager.status=cat", "status"]),
        command(&["git", "-c", "diff.external=tool", "status"]),
        command(&["git", "-c", "alias.review=!git status", "status"]),
        command(&["git", "--config-env", "core.sshCommand=GIT_SSH", "status"]),
        command(&["git", "-ccore.hooksPath=hooks", "status"]),
        command(&["git", "--config-env=core.hooksPath=GIT_HOOKS", "status"]),
        command(&["git", "status", "-c", "core.hooksPath=hooks"]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::DestructiveGit {
                verb: "config".to_string()
            },
            "{words:?}"
        );
    }

    for words in [
        command(&["git", "-c", "user.name=Joao", "status"]),
        command(&["git", "-c", "color.ui=false", "status"]),
        command(&["git", "-c", "alias.review=log", "status"]),
        command(&["git", "--config-env", "user.name=GIT_USER", "status"]),
        command(&["git", "status", "-c", "color.ui=false"]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::RequiresCheckoutLease,
            "{words:?}"
        );
    }

    for words in [
        command(&["bash", "-lc", "git -c core.hooksPath=hooks status"]),
        command(&["sudo", "git", "-c", "core.sshCommand=ssh", "status"]),
        command(&[
            "env",
            "GIT_OPTIONAL_LOCKS=0",
            "git",
            "-c",
            "core.pager=cat",
            "status",
        ]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::DestructiveGit {
                verb: "config".to_string()
            },
            "{words:?}"
        );
    }
}

#[test]
fn records_target_directories_and_move_source_deletions() {
    assert_eq!(
        classify_command(&command(&["cp", "-t", "relative/dest", "a", "b"])),
        paths(&["relative/dest"])
    );
    assert_eq!(
        classify_command(&command(&["cp", "--target-directory=relative/dest", "a"])),
        paths(&["relative/dest"])
    );
    assert_eq!(
        classify_command(&command(&["cp", "-trelative/dest", "a"])),
        paths(&["relative/dest"])
    );
    assert_eq!(
        classify_command(&command(&[
            "install",
            "--target-directory",
            "relative/dest",
            "a",
        ])),
        paths(&["relative/dest"])
    );
    assert_eq!(
        classify_command(&command(&["ln", "-t", "relative/dest", "a"])),
        paths(&["relative/dest"])
    );
    assert_eq!(
        classify_command(&command(&["mv", "-t", "relative/dest", "a", "b"])),
        paths(&["relative/dest", "a", "b"])
    );
    assert_eq!(
        classify_command(&command(&["mv", "--target-directory=relative/dest", "a"])),
        paths(&["relative/dest", "a"])
    );
    assert_eq!(
        classify_command(&command(&["mv", "a", "b", "relative/dest"])),
        paths(&["relative/dest", "a", "b"])
    );
    assert_eq!(
        classify_command(&command(&[
            "mv.exe",
            "--target-directory",
            r"relative\dest",
            r"relative\a",
        ])),
        paths(&[r"relative\dest", r"relative\a"])
    );
}

#[test]
fn records_sort_and_find_declared_output_paths() {
    for (words, expected) in [
        (
            command(&["sort", "-o", "relative/sorted", "input"]),
            &["relative/sorted"][..],
        ),
        (
            command(&["sort", "--output=relative/sorted", "input"]),
            &["relative/sorted"][..],
        ),
        (
            command(&["sort.exe", "-n", "-o", r"relative\sorted", "input"]),
            &[r"relative\sorted"][..],
        ),
        (
            command(&["find.exe", ".", "-fprint", r"relative\report"]),
            &[r"relative\report"][..],
        ),
        (
            command(&["find", ".", "-fprint0", "relative/report0"]),
            &["relative/report0"][..],
        ),
        (
            command(&["find", ".", "-fprintf", "relative/report", "%p\\n"]),
            &["relative/report"][..],
        ),
        (
            command(&["find", ".", "-fls", "relative/listing"]),
            &["relative/listing"][..],
        ),
        (
            command(&[
                "find",
                ".",
                "-fprint",
                "relative/one",
                "-fprint0",
                "relative/two",
            ]),
            &["relative/one", "relative/two"][..],
        ),
    ] {
        assert_eq!(classify_command(&words), paths(expected), "{words:?}");
    }

    assert_eq!(
        classify_command(&command(&["find", ".", "-fprint", "-"])),
        MutationIntent::ReadOnly
    );
    for words in [
        command(&["sort", "-o", "$OUTPUT", "input"]),
        command(&["find", ".", "-fprint", "$OUTPUT"]),
        command(&["find", ".", "-fprintf", "relative/report"]),
    ] {
        assert_eq!(
            classify_command(&words),
            MutationIntent::RequiresCheckoutLease,
            "{words:?}"
        );
    }
}

#[test]
fn shell_wrappers_preserve_declared_relative_paths() {
    assert_eq!(
        classify_command(&command(&[
            "bash",
            "-lc",
            "cp.exe --target-directory=relative/dest relative/source",
        ])),
        paths(&["relative/dest"])
    );
    assert_eq!(
        classify_command(&command(&[
            "cmd.exe",
            "/c",
            "sort -o relative/sorted relative/input",
        ])),
        paths(&["relative/sorted"])
    );
    assert_eq!(
        classify_command(&command(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "find.exe . -fprint relative/report",
        ])),
        paths(&["relative/report"])
    );
}
