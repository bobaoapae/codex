//! Best-effort redaction for bounded terminal observability text.

/// Redact common credential forms before retaining or exposing text.
///
/// This intentionally errs toward redacting values after any key containing a
/// credential marker.  The retained preview is a diagnostic hint, never an
/// authority or a source for reconstructing command arguments.
pub(crate) fn redact_and_truncate(input: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(input.len().min(max_bytes));
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        while index < chars.len()
            && (chars[index].is_ascii_alphanumeric() || matches!(chars[index], '_' | '-'))
        {
            index += 1;
        }
        if index > start {
            let key = chars[start..index]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase();
            let mut delimiter = index;
            while delimiter < chars.len() && chars[delimiter].is_ascii_whitespace() {
                delimiter += 1;
            }
            let has_assignment = delimiter < chars.len() && matches!(chars[delimiter], '=' | ':');
            let sensitive_key = key.contains("token")
                || key.contains("secret")
                || key.contains("password")
                || key.contains("passwd")
                || key.contains("api_key")
                || key.contains("apikey")
                || key.contains("authorization")
                || key.contains("cookie")
                || key.contains("private_key")
                || key == "key";
            let is_bearer = key == "bearer";
            let has_separated_value = sensitive_key && delimiter > index && delimiter < chars.len();
            if (has_assignment && sensitive_key)
                || has_separated_value
                || is_bearer && delimiter > index
            {
                output.extend(chars[start..delimiter].iter());
                if has_assignment {
                    output.push(chars[delimiter]);
                    delimiter += 1;
                    while delimiter < chars.len() && chars[delimiter].is_ascii_whitespace() {
                        output.push(chars[delimiter]);
                        delimiter += 1;
                    }
                }
                let quoted = chars
                    .get(delimiter)
                    .copied()
                    .filter(|character| matches!(character, '\'' | '"'));
                if let Some(quote) = quoted {
                    delimiter += 1;
                    while delimiter < chars.len() && chars[delimiter] != quote {
                        delimiter += 1;
                    }
                    if delimiter < chars.len() {
                        delimiter += 1;
                    }
                } else {
                    let value_start = delimiter;
                    while delimiter < chars.len() && !chars[delimiter].is_ascii_whitespace() {
                        delimiter += 1;
                    }
                    // `Authorization: Bearer <token>` has two words in its
                    // value; consume both while retaining no credential text.
                    let value = chars[value_start..delimiter].iter().collect::<String>();
                    if sensitive_key && value.eq_ignore_ascii_case("bearer") {
                        while delimiter < chars.len() && chars[delimiter].is_ascii_whitespace() {
                            delimiter += 1;
                        }
                        while delimiter < chars.len() && !chars[delimiter].is_ascii_whitespace() {
                            delimiter += 1;
                        }
                    }
                }
                output.push_str("[REDACTED_SECRET]");
                index = delimiter;
                continue;
            }
            output.extend(chars[start..index].iter());
            continue;
        }
        output.push(chars[index]);
        index += 1;
    }

    // Cover common token prefixes that are not written as assignments.  This
    // pass only sees the already bounded string, so it cannot retain the raw
    // command/output in another buffer.
    let mut redacted = output;
    for prefix in ["sk-", "ghp_", "github_pat_", "AKIA"] {
        let mut search_from = 0;
        while let Some(offset) = redacted[search_from..].find(prefix) {
            let start = search_from + offset;
            let mut end = start + prefix.len();
            while end < redacted.len()
                && !redacted.as_bytes()[end].is_ascii_whitespace()
                && !matches!(redacted.as_bytes()[end], b'"' | b'\'')
            {
                end += 1;
            }
            if end.saturating_sub(start) >= prefix.len() + 8 {
                redacted.replace_range(start..end, "[REDACTED_SECRET]");
                search_from = start + "[REDACTED_SECRET]".len();
            } else {
                search_from = end;
            }
            if search_from >= redacted.len() {
                break;
            }
        }
    }
    truncate_utf8(&redacted, max_bytes)
}

fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }
    let suffix = if max_bytes >= "…".len() { "…" } else { "" };
    if suffix.is_empty() {
        return input.chars().take(max_bytes).collect();
    }
    let budget = max_bytes.saturating_sub(suffix.len());
    let mut output = input.to_string();
    while output.len() > budget {
        output.pop();
    }
    output.push_str(suffix);
    output
}

#[cfg(test)]
#[path = "terminal_redaction_tests.rs"]
mod tests;
