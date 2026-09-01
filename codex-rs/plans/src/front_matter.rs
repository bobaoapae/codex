//! YAML front matter carried by every saved plan file.

use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

pub(crate) const MAX_ID_BYTES: usize = 240;
pub(crate) const MAX_TITLE_BYTES: usize = 4_096;
pub(crate) const MAX_CWD_BYTES: usize = 4_096;
pub(crate) const MAX_MODEL_BYTES: usize = 256;
pub(crate) const MAX_REVISION_BYTES: usize = 256;

/// Provenance attached to an approved plan snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanOrigin {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub rollout_id: Option<String>,
    pub build_revision: Option<String>,
    pub config_revision: Option<String>,
}

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
    /// Optional item that produced the plan in the source rollout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Optional source rollout identity for provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default = "default_revision")]
    pub revision: u32,
    /// Set only on an approved immutable snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
    /// Build revision that produced the approved snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_revision: Option<String>,
    /// Effective configuration revision used for the approved snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_revision: Option<String>,
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

    pub fn approved_at_utc(&self) -> Option<DateTime<Utc>> {
        self.approved_at.as_deref().and_then(parse_timestamp)
    }

    pub(crate) fn is_bounded(&self) -> bool {
        valid_required(&self.title, MAX_TITLE_BYTES)
            && valid_optional(&self.thread_id, MAX_ID_BYTES)
            && valid_optional(&self.turn_id, MAX_ID_BYTES)
            && valid_optional(&self.item_id, MAX_ID_BYTES)
            && valid_optional(&self.rollout_id, MAX_ID_BYTES)
            && valid_optional(&self.cwd, MAX_CWD_BYTES)
            && valid_optional(&self.model, MAX_MODEL_BYTES)
            && valid_required(&self.created_at, MAX_REVISION_BYTES)
            && valid_required(&self.updated_at, MAX_REVISION_BYTES)
            && self.revision > 0
            && valid_optional(&self.approved_at, MAX_REVISION_BYTES)
            && valid_optional(&self.build_revision, MAX_REVISION_BYTES)
            && valid_optional(&self.config_revision, MAX_REVISION_BYTES)
    }
}

fn valid_required(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn valid_optional(value: &Option<String>, max_bytes: usize) -> bool {
    value
        .as_deref()
        .is_none_or(|value| valid_required(value, max_bytes))
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
    debug_assert!(front_matter.is_bounded());
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
    if yaml.len() > 64 * 1024 {
        return None;
    }
    let front_matter = serde_yaml::from_str::<PlanFrontMatter>(yaml).ok()?;
    if !front_matter.is_bounded() {
        return None;
    }
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
