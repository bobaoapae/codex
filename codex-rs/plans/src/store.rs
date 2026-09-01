//! Draft plan storage under `$CODEX_HOME/plans/`.

use crate::front_matter::PlanFrontMatter;
use crate::front_matter::PlanOrigin;
use crate::front_matter::format_timestamp;
use crate::front_matter::parse_document;
use crate::front_matter::render_document;
use crate::locking::blocking;
use crate::locking::check_safe_directory;
use crate::locking::ensure_safe_directory;
use crate::locking::open_regular_file;
use crate::locking::with_write_lock;
use crate::naming::extract_title;
use crate::naming::file_stem_for;
use crate::naming::slugify;
use crate::plans_dir;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path::write_atomically;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;

/// Everything needed to persist one `<proposed_plan>` draft.
#[derive(Clone, Debug)]
pub struct SavePlanRequest {
    pub codex_home: AbsolutePathBuf,
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub cwd: Option<AbsolutePathBuf>,
    pub model: Option<String>,
    pub markdown: String,
}

/// Optional provenance supplied by a caller that owns the source rollout.
pub type SavePlanMetadata = PlanOrigin;

/// Where a plan ended up, and whether anything was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPlanPath {
    pub id: String,
    pub path: PathBuf,
    pub revision: u32,
    /// `false` when the document and metadata were byte-identical to what was already stored.
    pub written: bool,
}

/// Listing entry for a saved draft plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPlanSummary {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub rollout_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub build_revision: Option<String>,
    pub config_revision: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revision: u32,
}

/// A saved draft plus its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPlan {
    pub summary: SavedPlanSummary,
    pub markdown: String,
}

/// Persist one plan draft, keyed by thread.
pub async fn save_plan(request: SavePlanRequest) -> io::Result<SavedPlanPath> {
    save_plan_at(request, Local::now()).await
}

/// Persist one draft with caller-supplied provenance metadata.
pub async fn save_plan_with_metadata(
    request: SavePlanRequest,
    metadata: SavePlanMetadata,
) -> io::Result<SavedPlanPath> {
    save_plan_with_metadata_at(request, metadata, Local::now()).await
}

/// [`save_plan`] with an injectable clock.
pub async fn save_plan_at(
    request: SavePlanRequest,
    now: DateTime<Local>,
) -> io::Result<SavedPlanPath> {
    save_plan_at_with_origin(request, None, now).await
}

/// Metadata-aware variant of [`save_plan_at`].
pub async fn save_plan_with_metadata_at(
    request: SavePlanRequest,
    metadata: SavePlanMetadata,
    now: DateTime<Local>,
) -> io::Result<SavedPlanPath> {
    save_plan_at_with_origin(request, Some(metadata), now).await
}

async fn save_plan_at_with_origin(
    request: SavePlanRequest,
    origin: Option<PlanOrigin>,
    now: DateTime<Local>,
) -> io::Result<SavedPlanPath> {
    let dir = plans_dir(request.codex_home.as_path());
    with_write_lock(dir, move |dir| save_plan_locked(request, origin, now, dir)).await
}

fn save_plan_locked(
    request: SavePlanRequest,
    origin: Option<PlanOrigin>,
    now: DateTime<Local>,
    dir: &Path,
) -> io::Result<SavedPlanPath> {
    ensure_safe_directory(dir)?;
    let existing = read_all_plans_sync(dir)?;
    let body = normalize_body(&request.markdown);
    let title = extract_title(&body, now);
    let thread_id = request.thread_id.to_string();
    let origin = origin.unwrap_or_default();
    let effective_turn_id = origin
        .turn_id
        .as_deref()
        .unwrap_or(request.turn_id.as_str());
    validate_origin(&origin)?;

    let previous = existing
        .into_iter()
        .filter(|plan| plan.summary.thread_id.as_deref() == Some(thread_id.as_str()))
        .max_by(|left, right| {
            left.summary
                .revision
                .cmp(&right.summary.revision)
                .then_with(|| left.summary.updated_at.cmp(&right.summary.updated_at))
                .then_with(|| left.summary.id.cmp(&right.summary.id))
        });

    let (path, created_at, revision) = match previous {
        Some(previous) => {
            let same_metadata = previous.summary.title == title
                && previous.summary.turn_id.as_deref() == Some(effective_turn_id)
                && previous.summary.item_id == origin.item_id
                && previous.summary.rollout_id == origin.rollout_id
                && previous.summary.build_revision == origin.build_revision
                && previous.summary.config_revision == origin.config_revision
                && previous.summary.cwd
                    == request
                        .cwd
                        .as_ref()
                        .map(|cwd| cwd.as_path().to_string_lossy().to_string())
                && previous.summary.model == request.model;
            if normalize_body(&previous.markdown) == body && same_metadata {
                return Ok(SavedPlanPath {
                    id: previous.summary.id,
                    path: previous.summary.path,
                    revision: previous.summary.revision,
                    written: false,
                });
            }
            let revision = previous
                .summary
                .revision
                .checked_add(1)
                .ok_or_else(|| invalid_input("plan revision overflow"))?;
            (previous.summary.path, previous.summary.created_at, revision)
        }
        None => {
            let path = allocate_path_sync(dir, &file_stem_for(now, &slugify(&title)))?;
            (path, now.with_timezone(&Utc), 1)
        }
    };

    let front_matter = PlanFrontMatter {
        title,
        thread_id: Some(thread_id),
        turn_id: origin.turn_id.or(Some(request.turn_id)),
        item_id: origin.item_id,
        rollout_id: origin.rollout_id,
        cwd: request
            .cwd
            .as_ref()
            .map(|cwd| cwd.as_path().to_string_lossy().to_string()),
        model: request.model,
        created_at: format_timestamp(created_at),
        updated_at: format_timestamp(now.with_timezone(&Utc)),
        revision,
        approved_at: None,
        build_revision: origin.build_revision,
        config_revision: origin.config_revision,
    };
    validate_front_matter(&front_matter)?;
    let document = render_document(&front_matter, &body);
    let id = plan_id_for(&path)
        .ok_or_else(|| invalid_input(format!("plan path {} has no file name", path.display())))?;
    write_atomically(&path, &document)?;

    Ok(SavedPlanPath {
        id,
        path,
        revision,
        written: true,
    })
}

/// Every saved draft, newest first.
pub async fn list_plans(codex_home: &AbsolutePathBuf) -> io::Result<Vec<SavedPlanSummary>> {
    let dir = plans_dir(codex_home.as_path());
    blocking(move || {
        if !check_safe_directory(&dir)? {
            return Ok(Vec::new());
        }
        read_all_plans_sync(&dir).map(|plans| {
            let mut plans: Vec<_> = plans.into_iter().map(|plan| plan.summary).collect();
            plans.sort_by(|left, right| {
                right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then_with(|| right.id.cmp(&left.id))
            });
            plans
        })
    })
    .await
}

/// Read one draft by its opaque, safe component ID.
pub async fn read_plan(codex_home: &AbsolutePathBuf, id: &str) -> io::Result<Option<SavedPlan>> {
    if !is_valid_plan_id(id) {
        return Ok(None);
    }
    let path = plans_dir(codex_home.as_path()).join(id);
    blocking(move || {
        let Some(dir) = path.parent() else {
            return Ok(None);
        };
        if !check_safe_directory(dir)? {
            return Ok(None);
        }
        let Some(mut file) = open_regular_file(&path)? else {
            return Ok(None);
        };
        let mut contents = String::new();
        use std::io::Read;
        file.read_to_string(&mut contents)?;
        Ok(plan_from_document(&path, &contents))
    })
    .await
}

/// Validate an opaque plan ID before using it as a path component.
pub fn is_valid_plan_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= crate::front_matter::MAX_ID_BYTES
        && id != "."
        && id != ".."
        && !id.ends_with('.')
        && !is_reserved_windows_name(id)
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn is_reserved_windows_name(id: &str) -> bool {
    let base = id.split('.').next().unwrap_or_default();
    matches!(
        base.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

pub(crate) fn read_all_plans_sync(dir: &Path) -> io::Result<Vec<SavedPlan>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut plans = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Some(mut file) = open_regular_file(&path)? else {
            continue;
        };
        let mut contents = String::new();
        use std::io::Read;
        if let Err(error) = file.read_to_string(&mut contents) {
            warn!("failed to read plan {}: {error}", path.display());
            continue;
        }
        match plan_from_document(&path, &contents) {
            Some(plan) => plans.push(plan),
            None => warn!(
                "ignoring plan without valid front matter: {}",
                path.display()
            ),
        }
    }
    Ok(plans)
}

pub(crate) fn plan_from_document(path: &Path, contents: &str) -> Option<SavedPlan> {
    let (front_matter, body) = parse_document(contents)?;
    let created_at = front_matter.created_at_utc()?;
    let updated_at = front_matter.updated_at_utc()?;
    Some(SavedPlan {
        summary: SavedPlanSummary {
            id: plan_id_for(path)?,
            path: path.to_path_buf(),
            title: front_matter.title,
            thread_id: front_matter.thread_id,
            turn_id: front_matter.turn_id,
            item_id: front_matter.item_id,
            rollout_id: front_matter.rollout_id,
            cwd: front_matter.cwd,
            model: front_matter.model,
            build_revision: front_matter.build_revision,
            config_revision: front_matter.config_revision,
            created_at,
            updated_at,
            revision: front_matter.revision,
        },
        markdown: body,
    })
}

pub(crate) fn plan_id_for(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|id| is_valid_plan_id(id))
        .map(str::to_string)
}

pub(crate) fn normalize_body(markdown: &str) -> String {
    format!(
        "{}\n",
        markdown.replace("\r\n", "\n").trim_end_matches('\n')
    )
}

fn allocate_path_sync(dir: &Path, stem: &str) -> io::Result<PathBuf> {
    let mut candidate = dir.join(format!("{stem}.md"));
    let mut suffix = 2u32;
    loop {
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        candidate = dir.join(format!("{stem}-{suffix}.md"));
        suffix = suffix
            .checked_add(1)
            .ok_or_else(|| invalid_input("plan filename suffix overflow"))?;
    }
    Ok(candidate)
}

fn validate_origin(origin: &PlanOrigin) -> io::Result<()> {
    for value in [
        origin.thread_id.as_deref(),
        origin.turn_id.as_deref(),
        origin.item_id.as_deref(),
        origin.rollout_id.as_deref(),
        origin.build_revision.as_deref(),
        origin.config_revision.as_deref(),
    ] {
        if let Some(value) = value
            && (value.is_empty()
                || value.len() > crate::front_matter::MAX_REVISION_BYTES
                || value.chars().any(char::is_control))
        {
            return Err(invalid_input("plan provenance metadata is out of bounds"));
        }
    }
    Ok(())
}

fn validate_front_matter(front_matter: &PlanFrontMatter) -> io::Result<()> {
    if !front_matter.is_bounded() {
        return Err(invalid_input("plan front matter metadata is out of bounds"));
    }
    Ok(())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
