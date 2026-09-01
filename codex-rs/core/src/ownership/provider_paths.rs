use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn extract_mcp_paths(value: &serde_json::Value) -> Vec<PathBuf> {
    const PATH_KEYS: &[&str] = &[
        "path",
        "paths",
        "file",
        "files",
        "filePath",
        "file_path",
        "directory",
        "directories",
        "target",
        "destination",
    ];
    let mut paths = Vec::new();
    collect_mcp_paths(value, PATH_KEYS, &mut paths);
    paths
}

fn collect_mcp_paths(value: &serde_json::Value, keys: &[&str], paths: &mut Vec<PathBuf>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if keys
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(key))
                {
                    collect_string_paths(value, paths);
                } else {
                    collect_mcp_paths(value, keys, paths);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_mcp_paths(value, keys, paths);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn collect_string_paths(value: &serde_json::Value, paths: &mut Vec<PathBuf>) {
    match value {
        serde_json::Value::String(value)
            if !value.is_empty()
                && !value.contains(['$', '%', '*', '?', '~', '`'])
                && value != "-" =>
        {
            paths.push(PathBuf::from(value));
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_string_paths(value, paths);
            }
        }
        serde_json::Value::Object(_)
        | serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

pub(super) fn claude_input_paths(input: &serde_json::Value, cwd: &AbsolutePathBuf) -> Vec<PathBuf> {
    extract_mcp_paths(input)
        .into_iter()
        .map(|path| resolve_path(cwd, &path))
        .collect()
}

pub(super) fn resolve_path(cwd: &AbsolutePathBuf, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path).into_path_buf()
    }
}
