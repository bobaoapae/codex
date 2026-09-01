//! Rendering and bounded validation for plan text injected into a model context.

use codex_utils_string::approx_token_count;
use std::fmt;

/// Hard context budget for an approved plan fragment.
pub const MAX_APPROVED_PLAN_TOKENS: usize = 10_000;
const MAX_FRAGMENT_ID_BYTES: usize = 240;
const MAX_FRAGMENT_TITLE_BYTES: usize = 4_096;

/// Failure returned when an approved plan cannot be represented safely in context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanFragmentError {
    EmptyId,
    EmptyTitle,
    IdTooLong,
    TitleTooLong,
    TooManyTokens { actual: usize, maximum: usize },
}

impl fmt::Display for PlanFragmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("plan fragment id must not be empty"),
            Self::EmptyTitle => formatter.write_str("plan fragment title must not be empty"),
            Self::IdTooLong => formatter.write_str("plan fragment id exceeds 240 bytes"),
            Self::TitleTooLong => formatter.write_str("plan fragment title exceeds 4096 bytes"),
            Self::TooManyTokens { actual, maximum } => write!(
                formatter,
                "plan fragment has {actual} tokens; maximum is {maximum}"
            ),
        }
    }
}

impl std::error::Error for PlanFragmentError {}

/// Render the exact bounded fragment used when an approved plan is injected.
///
/// The header intentionally carries the opaque plan ID and the draft revision so a model-facing
/// context inspection can identify the immutable contract without consulting a second store.
pub fn render_plan_fragment(
    id: &str,
    revision: u32,
    title: &str,
    markdown: &str,
) -> Result<String, PlanFragmentError> {
    validate_header(id, title)?;
    let body = markdown
        .replace("\r\n", "\n")
        .trim_end_matches('\n')
        .to_string();
    let fragment = format!("# {title}\n\nPlan ID: {id}\nRevision: {revision}\n\n{body}\n");
    validate_plan_fragment(&fragment)?;
    Ok(fragment)
}

/// Validate a pre-rendered plan fragment against the same model-context budget.
pub fn validate_plan_fragment(fragment: &str) -> Result<(), PlanFragmentError> {
    let actual = approx_token_count(fragment);
    if actual > MAX_APPROVED_PLAN_TOKENS {
        return Err(PlanFragmentError::TooManyTokens {
            actual,
            maximum: MAX_APPROVED_PLAN_TOKENS,
        });
    }
    Ok(())
}

fn validate_header(id: &str, title: &str) -> Result<(), PlanFragmentError> {
    if id.is_empty() {
        return Err(PlanFragmentError::EmptyId);
    }
    if id.len() > MAX_FRAGMENT_ID_BYTES {
        return Err(PlanFragmentError::IdTooLong);
    }
    if title.trim().is_empty() {
        return Err(PlanFragmentError::EmptyTitle);
    }
    if title.len() > MAX_FRAGMENT_TITLE_BYTES {
        return Err(PlanFragmentError::TitleTooLong);
    }
    Ok(())
}
