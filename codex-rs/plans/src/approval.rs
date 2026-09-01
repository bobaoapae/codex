//! Immutable approved-plan snapshots and their derived supersession state.

use crate::fragment::PlanFragmentError;
use crate::fragment::render_plan_fragment;
use crate::front_matter::PlanFrontMatter;
use crate::front_matter::PlanOrigin;
use crate::front_matter::format_timestamp;
use crate::front_matter::parse_document;
use crate::front_matter::render_document;
use crate::locking::blocking_result;
use crate::locking::check_safe_directory;
use crate::locking::check_safe_file_destination;
use crate::locking::ensure_safe_directory;
use crate::locking::open_regular_file;
use crate::locking::with_write_lock;
use crate::plans_dir;
use crate::store::is_valid_plan_id;
use crate::store::normalize_body;
use chrono::DateTime;
use chrono::Utc;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path::write_atomically;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

const APPROVED_DIRECTORY: &str = "approved";

/// Input for approving the current draft revision of one plan ID.
#[derive(Clone, Debug)]
pub struct ApprovePlanRequest {
    pub codex_home: AbsolutePathBuf,
    pub id: String,
    pub expected_revision: u32,
    pub origin: PlanOrigin,
    pub approved_at: Option<DateTime<Utc>>,
}

/// Error returned when an approval cannot be safely applied.
#[derive(Debug)]
pub enum PlanApprovalError {
    Io(io::Error),
    InvalidId(String),
    DraftNotFound(String),
    StaleDraft {
        id: String,
        expected: u32,
        actual: u32,
    },
    Conflict(String),
    TooLarge {
        actual: usize,
        maximum: usize,
    },
}

/// Backwards-compatible short name for approval failures.
pub type ApprovePlanError = PlanApprovalError;

impl std::fmt::Display for PlanApprovalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::InvalidId(id) => write!(formatter, "invalid plan id: {id}"),
            Self::DraftNotFound(id) => write!(formatter, "draft plan not found: {id}"),
            Self::StaleDraft {
                id,
                expected,
                actual,
            } => write!(
                formatter,
                "draft plan {id} is stale: expected revision {expected}, current revision {actual}"
            ),
            Self::Conflict(message) => formatter.write_str(message),
            Self::TooLarge { actual, maximum } => write!(
                formatter,
                "approved plan fragment has {actual} tokens; maximum is {maximum}"
            ),
        }
    }
}

impl std::error::Error for PlanApprovalError {}

impl From<io::Error> for PlanApprovalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Immutable approved snapshot plus its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedPlan {
    pub summary: ApprovedPlanSummary,
    pub markdown: String,
    /// `false` when an equivalent snapshot already existed.
    pub written: bool,
}

/// Metadata for one approved snapshot. Supersession is derived from the files present on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedPlanSummary {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub rollout_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub approved_at: DateTime<Utc>,
    pub revision: u32,
    pub build_revision: Option<String>,
    pub config_revision: Option<String>,
    pub superseded_by: Option<u32>,
}

/// Approve one draft revision without rewriting the draft or any prior snapshot.
pub async fn approve_plan(request: ApprovePlanRequest) -> Result<ApprovedPlan, PlanApprovalError> {
    if !is_valid_plan_id(&request.id) {
        return Err(PlanApprovalError::InvalidId(request.id));
    }
    if request.expected_revision == 0 {
        return Err(PlanApprovalError::Conflict(
            "expected draft revision must be positive".to_string(),
        ));
    }
    let dir = plans_dir(request.codex_home.as_path());
    with_write_lock(dir, move |dir| approve_plan_locked(request, dir)).await
}

fn approve_plan_locked(
    request: ApprovePlanRequest,
    dir: &Path,
) -> Result<ApprovedPlan, PlanApprovalError> {
    ensure_safe_directory(dir)?;
    let draft_path = dir.join(&request.id);
    let Some(mut draft_file) = open_regular_file(&draft_path)? else {
        return Err(PlanApprovalError::DraftNotFound(request.id));
    };
    let mut draft_document = String::new();
    draft_file.read_to_string(&mut draft_document)?;
    let (draft_front_matter, draft_body) = parse_document(&draft_document).ok_or_else(|| {
        PlanApprovalError::Conflict(format!(
            "draft plan {} has invalid front matter",
            request.id
        ))
    })?;
    if draft_front_matter.revision != request.expected_revision {
        return Err(PlanApprovalError::StaleDraft {
            id: request.id,
            expected: request.expected_revision,
            actual: draft_front_matter.revision,
        });
    }
    let draft_body = normalize_body(&draft_body);
    let title = draft_front_matter.title.clone();
    let approved_dir = dir.join(APPROVED_DIRECTORY);
    ensure_safe_directory(&approved_dir)?;
    let id_dir = approved_dir.join(&request.id);
    ensure_safe_directory(&id_dir)?;
    let approved_path = id_dir.join(format!("{}.md", request.expected_revision));
    check_safe_file_destination(&approved_path)?;

    let existing_front_matter = read_existing_front_matter(&approved_path)?;
    let approved_at = request
        .approved_at
        .or_else(|| {
            existing_front_matter
                .as_ref()
                .and_then(PlanFrontMatter::approved_at_utc)
        })
        .unwrap_or_else(Utc::now);
    let origin = merge_origin(&draft_front_matter, &request.origin);
    let fragment =
        render_plan_fragment(&request.id, request.expected_revision, &title, &draft_body)
            .map_err(plan_fragment_error)?;
    let _ = fragment;
    let front_matter = PlanFrontMatter {
        title,
        thread_id: origin.thread_id,
        turn_id: origin.turn_id,
        item_id: origin.item_id,
        rollout_id: origin.rollout_id,
        cwd: draft_front_matter.cwd,
        model: draft_front_matter.model,
        created_at: draft_front_matter.created_at,
        updated_at: format_timestamp(approved_at),
        revision: request.expected_revision,
        approved_at: Some(format_timestamp(approved_at)),
        build_revision: origin.build_revision,
        config_revision: origin.config_revision,
    };
    if !front_matter.is_bounded() {
        return Err(PlanApprovalError::Conflict(
            "approved plan metadata is out of bounds".to_string(),
        ));
    }
    let document = render_document(&front_matter, &draft_body);

    if let Some(existing_document) = read_existing_document(&approved_path)? {
        if existing_document == document {
            return Ok(ApprovedPlan {
                summary: approved_summary(&request.id, &approved_path, &front_matter)?,
                markdown: draft_body,
                written: false,
            });
        }
        return Err(PlanApprovalError::Conflict(format!(
            "approved snapshot already exists with different bytes or metadata: {}",
            approved_path.display()
        )));
    }

    write_atomically(&approved_path, &document)?;
    Ok(ApprovedPlan {
        summary: approved_summary(&request.id, &approved_path, &front_matter)?,
        markdown: draft_body,
        written: true,
    })
}

/// List every approved revision, newest first, with supersession derived from the directory.
pub async fn list_approved_plans(
    codex_home: &AbsolutePathBuf,
) -> Result<Vec<ApprovedPlanSummary>, PlanApprovalError> {
    let dir = plans_dir(codex_home.as_path()).join(APPROVED_DIRECTORY);
    blocking_result(move || list_approved_plans_sync(&dir)).await
}

/// Read one approved revision, or the newest revision when `revision` is omitted.
pub async fn read_approved_plan(
    codex_home: &AbsolutePathBuf,
    id: &str,
    revision: Option<u32>,
) -> Result<Option<ApprovedPlan>, PlanApprovalError> {
    if !is_valid_plan_id(id) {
        return Ok(None);
    }
    if revision == Some(0) {
        return Err(PlanApprovalError::Conflict(
            "approved revision must be positive".to_string(),
        ));
    }
    let dir = plans_dir(codex_home.as_path())
        .join(APPROVED_DIRECTORY)
        .join(id);
    let id = id.to_string();
    blocking_result(move || {
        if !check_safe_directory(&dir)? {
            return Ok(None);
        }
        let mut candidates = read_approved_documents_sync(&dir, &id)?;
        candidates.sort_by_key(|plan| plan.summary.revision);
        let Some(plan) = (match revision {
            Some(revision) => candidates
                .into_iter()
                .find(|plan| plan.summary.revision == revision),
            None => candidates.pop(),
        }) else {
            return Ok(None);
        };
        Ok(Some(plan))
    })
    .await
}

fn list_approved_plans_sync(dir: &Path) -> Result<Vec<ApprovedPlanSummary>, PlanApprovalError> {
    let mut summaries = Vec::new();
    if !check_safe_directory(dir)? {
        return Ok(summaries);
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(summaries),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type()?;
        if !is_valid_plan_id(&id) || !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let mut documents = read_approved_documents_sync(&entry.path(), &id)?;
        summaries.extend(documents.drain(..).map(|plan| plan.summary));
    }
    let mut latest_by_id: HashMap<String, u32> = HashMap::new();
    for summary in &summaries {
        latest_by_id
            .entry(summary.id.clone())
            .and_modify(|revision| *revision = (*revision).max(summary.revision))
            .or_insert(summary.revision);
    }
    for summary in &mut summaries {
        summary.superseded_by = latest_by_id
            .get(&summary.id)
            .copied()
            .filter(|revision| *revision != summary.revision);
    }
    summaries.sort_by(|left, right| {
        right
            .approved_at
            .cmp(&left.approved_at)
            .then_with(|| right.id.cmp(&left.id))
            .then_with(|| right.revision.cmp(&left.revision))
    });
    Ok(summaries)
}

fn read_approved_documents_sync(
    dir: &Path,
    id: &str,
) -> Result<Vec<ApprovedPlan>, PlanApprovalError> {
    if !check_safe_directory(dir)? {
        return Ok(Vec::new());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut plans = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let Some(mut file) = open_regular_file(&path)? else {
            continue;
        };
        let mut document = String::new();
        file.read_to_string(&mut document)?;
        let Some((front_matter, body)) = parse_document(&document) else {
            continue;
        };
        let Some(path_revision) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if path_revision != front_matter.revision {
            continue;
        }
        if front_matter.approved_at.is_none() || front_matter.revision == 0 {
            continue;
        }
        plans.push(ApprovedPlan {
            summary: approved_summary(id, &path, &front_matter)?,
            markdown: body,
            written: false,
        });
    }
    Ok(plans)
}

fn approved_summary(
    id: &str,
    path: &Path,
    front_matter: &PlanFrontMatter,
) -> Result<ApprovedPlanSummary, PlanApprovalError> {
    let approved_at = front_matter.approved_at_utc().ok_or_else(|| {
        PlanApprovalError::Conflict("approvedAt is missing or invalid".to_string())
    })?;
    let created_at = front_matter.created_at_utc().ok_or_else(|| {
        PlanApprovalError::Conflict("createdAt is missing or invalid".to_string())
    })?;
    let updated_at = front_matter.updated_at_utc().ok_or_else(|| {
        PlanApprovalError::Conflict("updatedAt is missing or invalid".to_string())
    })?;
    Ok(ApprovedPlanSummary {
        id: id.to_string(),
        path: path.to_path_buf(),
        title: front_matter.title.clone(),
        thread_id: front_matter.thread_id.clone(),
        turn_id: front_matter.turn_id.clone(),
        item_id: front_matter.item_id.clone(),
        rollout_id: front_matter.rollout_id.clone(),
        cwd: front_matter.cwd.clone(),
        model: front_matter.model.clone(),
        created_at,
        updated_at,
        approved_at,
        revision: front_matter.revision,
        build_revision: front_matter.build_revision.clone(),
        config_revision: front_matter.config_revision.clone(),
        superseded_by: None,
    })
}

fn read_existing_document(path: &Path) -> io::Result<Option<String>> {
    if !check_safe_file_destination(path)? {
        return Ok(None);
    }
    let Some(mut file) = open_regular_file(path)? else {
        return Ok(None);
    };
    let mut document = String::new();
    file.read_to_string(&mut document)?;
    Ok(Some(document))
}

fn read_existing_front_matter(path: &Path) -> io::Result<Option<PlanFrontMatter>> {
    let Some(document) = read_existing_document(path)? else {
        return Ok(None);
    };
    Ok(parse_document(&document).map(|(front_matter, _)| front_matter))
}

fn merge_origin(draft: &PlanFrontMatter, requested: &PlanOrigin) -> PlanOrigin {
    PlanOrigin {
        thread_id: requested
            .thread_id
            .clone()
            .or_else(|| draft.thread_id.clone()),
        turn_id: requested.turn_id.clone().or_else(|| draft.turn_id.clone()),
        item_id: requested.item_id.clone().or_else(|| draft.item_id.clone()),
        rollout_id: requested
            .rollout_id
            .clone()
            .or_else(|| draft.rollout_id.clone()),
        build_revision: requested
            .build_revision
            .clone()
            .or_else(|| draft.build_revision.clone()),
        config_revision: requested
            .config_revision
            .clone()
            .or_else(|| draft.config_revision.clone()),
    }
}

fn plan_fragment_error(error: PlanFragmentError) -> PlanApprovalError {
    match error {
        PlanFragmentError::TooManyTokens { actual, maximum } => {
            PlanApprovalError::TooLarge { actual, maximum }
        }
        error => PlanApprovalError::Conflict(error.to_string()),
    }
}

#[cfg(test)]
#[path = "approval_tests.rs"]
mod tests;
