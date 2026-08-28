//! Reading and writing the plan files under `$CODEX_HOME/plans/`.

use crate::front_matter::PlanFrontMatter;
use crate::front_matter::format_timestamp;
use crate::front_matter::parse_document;
use crate::front_matter::render_document;
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
use std::io;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;

/// Everything needed to persist one `<proposed_plan>`.
#[derive(Clone, Debug)]
pub struct SavePlanRequest {
    pub codex_home: AbsolutePathBuf,
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub cwd: Option<AbsolutePathBuf>,
    pub model: Option<String>,
    pub markdown: String,
}

/// Where a plan ended up, and whether anything was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPlanPath {
    pub id: String,
    pub path: PathBuf,
    pub revision: u32,
    /// `false` when the plan body was byte-identical to what was already stored.
    pub written: bool,
}

/// Listing entry for a saved plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPlanSummary {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revision: u32,
}

/// A saved plan plus its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPlan {
    pub summary: SavedPlanSummary,
    pub markdown: String,
}

/// Persist one plan, keyed by thread.
pub async fn save_plan(request: SavePlanRequest) -> io::Result<SavedPlanPath> {
    save_plan_at(request, Local::now()).await
}

/// [`save_plan`] with an injectable clock.
pub async fn save_plan_at(
    request: SavePlanRequest,
    now: DateTime<Local>,
) -> io::Result<SavedPlanPath> {
    let dir = plans_dir(request.codex_home.as_path());
    let existing = read_all_plans(&dir).await?;
    let body = normalize_body(&request.markdown);
    let title = extract_title(&body, now);
    let thread_id = request.thread_id.to_string();

    let previous = existing
        .into_iter()
        .filter(|plan| plan.summary.thread_id.as_deref() == Some(thread_id.as_str()))
        .max_by(|left, right| {
            left.summary
                .updated_at
                .cmp(&right.summary.updated_at)
                .then_with(|| left.summary.id.cmp(&right.summary.id))
        });

    let (path, created_at, revision) = match previous {
        Some(previous) => {
            if normalize_body(&previous.markdown) == body {
                return Ok(SavedPlanPath {
                    id: previous.summary.id,
                    path: previous.summary.path,
                    revision: previous.summary.revision,
                    written: false,
                });
            }
            let revision = previous.summary.revision.saturating_add(1);
            (previous.summary.path, previous.summary.created_at, revision)
        }
        None => {
            let path = allocate_path(&dir, &file_stem_for(now, &slugify(&title))).await?;
            (path, now.with_timezone(&Utc), 1)
        }
    };

    let updated_at = now.with_timezone(&Utc);
    let front_matter = PlanFrontMatter {
        title,
        thread_id: Some(thread_id),
        turn_id: Some(request.turn_id),
        cwd: request
            .cwd
            .as_ref()
            .map(|cwd| cwd.as_path().to_string_lossy().to_string()),
        model: request.model,
        created_at: format_timestamp(created_at),
        updated_at: format_timestamp(updated_at),
        revision,
    };
    let document = render_document(&front_matter, &body);
    let id = plan_id_for(&path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("plan path {} has no file name", path.display()),
        )
    })?;

    let write_path = path.clone();
    tokio::task::spawn_blocking(move || write_atomically(&write_path, &document))
        .await
        .map_err(io::Error::other)??;

    Ok(SavedPlanPath {
        id,
        path,
        revision,
        written: true,
    })
}

/// Every saved plan, newest first.
pub async fn list_plans(codex_home: &AbsolutePathBuf) -> io::Result<Vec<SavedPlanSummary>> {
    let mut plans: Vec<SavedPlanSummary> = read_all_plans(&plans_dir(codex_home.as_path()))
        .await?
        .into_iter()
        .map(|plan| plan.summary)
        .collect();
    plans.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(plans)
}

/// Read one saved plan by id. Unknown or malformed ids return `Ok(None)`.
pub async fn read_plan(codex_home: &AbsolutePathBuf, id: &str) -> io::Result<Option<SavedPlan>> {
    if !is_valid_plan_id(id) {
        return Ok(None);
    }
    let path = plans_dir(codex_home.as_path()).join(id);
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => Ok(plan_from_document(&path, &contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Ids are plain file names inside the plans directory; anything else is rejected.
pub fn is_valid_plan_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

async fn read_all_plans(dir: &Path) -> io::Result<Vec<SavedPlan>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut plans = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => contents,
            Err(err) => {
                warn!("failed to read plan {}: {err}", path.display());
                continue;
            }
        };
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

fn plan_from_document(path: &Path, contents: &str) -> Option<SavedPlan> {
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
            cwd: front_matter.cwd,
            model: front_matter.model,
            created_at,
            updated_at,
            revision: front_matter.revision,
        },
        markdown: body,
    })
}

fn plan_id_for(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

fn normalize_body(markdown: &str) -> String {
    format!(
        "{}\n",
        markdown.replace("\r\n", "\n").trim_end_matches('\n')
    )
}

async fn allocate_path(dir: &Path, stem: &str) -> io::Result<PathBuf> {
    let mut candidate = dir.join(format!("{stem}.md"));
    let mut suffix = 2u32;
    while tokio::fs::try_exists(&candidate).await? {
        candidate = dir.join(format!("{stem}-{suffix}.md"));
        suffix += 1;
    }
    Ok(candidate)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
