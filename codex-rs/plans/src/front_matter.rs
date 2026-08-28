//! YAML front matter carried by every saved plan file.

use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

/// Metadata stored above the plan body.
///
/// Timestamps are serialized as RFC3339 UTC strings so the file stays readable and diffable; the
/// public API converts them to unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFrontMatter {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default = "default_revision")]
    pub revision: u32,
}

fn default_revision() -> u32 {
    1
}

impl PlanFrontMatter {
    pub fn created_at_utc(&self) -> Option<DateTime<Utc>> {
        parse_timestamp(&self.created_at)
    }

    pub fn updated_at_utc(&self) -> Option<DateTime<Utc>> {
        parse_timestamp(&self.updated_at)
    }
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

/// Format a timestamp the way it is written into the front matter.
pub fn format_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, /*use_z*/ true)
}

/// Render `front_matter` + `body` as the on-disk plan document.
pub fn render_document(front_matter: &PlanFrontMatter, body: &str) -> String {
    let yaml = serde_yaml::to_string(front_matter)
        .unwrap_or_else(|err| panic!("plan front matter must serialize: {err}"));
    let body = body.trim_end_matches('\n');
    format!("---\n{yaml}---\n\n{body}\n")
}

/// Split a plan document into its front matter and body.
///
/// Returns `None` when the document does not start with a `---` fence or the YAML block does not
/// deserialize; callers treat that as "not a plan file" and skip it.
pub fn parse_document(contents: &str) -> Option<(PlanFrontMatter, String)> {
    let without_bom = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let rest = without_bom
        .strip_prefix("---\r\n")
        .or_else(|| without_bom.strip_prefix("---\n"))?;
    let (yaml, body) = split_closing_fence(rest)?;
    let front_matter = serde_yaml::from_str::<PlanFrontMatter>(yaml).ok()?;
    Some((front_matter, strip_leading_blank_lines(body).to_string()))
}

fn strip_leading_blank_lines(body: &str) -> &str {
    let mut rest = body;
    loop {
        rest = match rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
        {
            Some(next) => next,
            None => return rest,
        };
    }
}

fn split_closing_fence(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
#[path = "front_matter_tests.rs"]
mod tests;
