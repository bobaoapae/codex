//! Titles, slugs and file names for saved plans.

use chrono::DateTime;
use chrono::Local;

const MAX_TITLE_CHARS: usize = 80;
const MAX_SLUG_CHARS: usize = 48;

/// Derive a human title from the plan markdown.
///
/// Prefers the first ATX heading, then the first non-empty line, and finally a dated fallback.
pub fn extract_title(markdown: &str, now: DateTime<Local>) -> String {
    let heading = markdown.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix('#')?;
        let rest = rest.trim_start_matches('#').trim();
        (!rest.is_empty()).then(|| rest.to_string())
    });
    let title = heading.or_else(|| {
        markdown
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    });
    match title {
        Some(title) => truncate_chars(&title, MAX_TITLE_CHARS),
        None => format!("Plan {}", now.format("%Y-%m-%d")),
    }
}

/// Lowercase `[a-z0-9]` runs joined by `-`, bounded so file names stay short.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut pending_separator = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            slug.push(ch.to_ascii_lowercase());
            if slug.chars().count() >= MAX_SLUG_CHARS {
                break;
            }
        } else {
            pending_separator = true;
        }
    }
    if slug.is_empty() {
        "plan".to_string()
    } else {
        slug
    }
}

/// File stem for a new plan: a sortable local timestamp plus the slug.
///
/// `:` is not usable in Windows file names, so the time uses `-` separators.
pub fn file_stem_for(now: DateTime<Local>, slug: &str) -> String {
    format!("{}-{slug}", now.format("%Y-%m-%dT%H-%M-%S"))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
#[path = "naming_tests.rs"]
mod tests;
