//! Cross-process admission for broad Rust workspace builds.
//!
//! A broad Cargo/Just build can occupy the checkout's shared target directory
//! for a long time. The lease is deliberately an advisory OS file lock: a
//! focused package build can coexist, while a second broad build receives an
//! immediate typed conflict. The guard is held by the process entry until the
//! child exits or the entry is explicitly released. A broad command outside a
//! Git checkout has no stable identity and therefore runs unmanaged.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use serde::ser::SerializeStruct;
use thiserror::Error;

use super::build_admission_lock;

const LOCK_FILE_NAME: &str = ".codex-build-admission.lock";
const OWNER_METADATA_MAX_BYTES: usize = 4_096;
const MAX_OWNER_SESSION_BYTES: usize = 128;

/// The broad Rust command classes protected by the checkout/target lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BroadRustCommand {
    CargoBuild,
    CargoCheck,
    CargoTest,
    CargoClippy,
    JustTest,
    JustClippy,
    JustFix,
}

/// Stable checkout/target identity used for one admission lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuildAdmissionKey {
    pub(crate) checkout: PathBuf,
    pub(crate) target_dir: PathBuf,
    pub(crate) lock_path: PathBuf,
    pub(crate) owner_path: PathBuf,
}

/// Bounded owner details returned when a broad build is already active.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildAdmissionOwner {
    pub(crate) pid: Option<u32>,
    pub(crate) session_id: Option<String>,
    pub(crate) started_at_ms: Option<i64>,
    pub(crate) elapsed_ms: Option<u64>,
}

/// An immediate admission conflict. This is also a `needsAttention` state:
/// callers may inspect the owner and decide when to retry explicitly.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct BuildAdmissionBusy {
    pub(crate) checkout: PathBuf,
    pub(crate) target_dir: PathBuf,
    pub(crate) owner: BuildAdmissionOwner,
}

impl std::fmt::Debug for BuildAdmissionBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuildAdmissionBusy")
            .field("checkout", &"<redacted>")
            .field("targetDir", &"<redacted>")
            .field("ownerPid", &self.owner.pid)
            .field("ownerSession", &"<redacted>")
            .field("startedAtMs", &self.owner.started_at_ms)
            .field("elapsedMs", &self.owner.elapsed_ms)
            .finish()
    }
}

impl Serialize for BuildAdmissionBusy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut output = serializer.serialize_struct("BuildAdmissionBusy", 4)?;
        output.serialize_field("checkout", "<redacted>")?;
        output.serialize_field("targetDir", "<redacted>")?;
        output.serialize_field("owner", &RedactedOwner::from(&self.owner))?;
        output.serialize_field("needsAttention", &self.needs_attention())?;
        output.end()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RedactedOwner {
    pid: Option<u32>,
    session_id: &'static str,
    started_at_ms: Option<i64>,
    elapsed_ms: Option<u64>,
}

impl From<&BuildAdmissionOwner> for RedactedOwner {
    fn from(owner: &BuildAdmissionOwner) -> Self {
        Self {
            pid: owner.pid,
            session_id: "<redacted>",
            started_at_ms: owner.started_at_ms,
            elapsed_ms: owner.elapsed_ms,
        }
    }
}

impl BuildAdmissionBusy {
    pub(crate) const fn needs_attention(&self) -> bool {
        true
    }
}

impl std::fmt::Display for BuildAdmissionBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "BuildAdmissionBusy(needsAttention): another broad Rust build owns this target"
        )?;
        if let Some(pid) = self.owner.pid {
            write!(formatter, "; ownerPid={pid}")?;
        }
        if self.owner.session_id.is_some() {
            write!(formatter, "; ownerSession=<redacted>")?;
        }
        if let Some(started_at_ms) = self.owner.started_at_ms {
            write!(formatter, "; startedAtMs={started_at_ms}")?;
        }
        if let Some(elapsed_ms) = self.owner.elapsed_ms {
            write!(formatter, "; elapsedMs={elapsed_ms}")?;
        }
        Ok(())
    }
}

/// Failure to acquire or inspect an admission lock.
#[derive(Debug, Error)]
pub(crate) enum BuildAdmissionError {
    #[error("failed to resolve broad Rust build admission: {0}")]
    Resolve(String),
    #[error("failed to open broad Rust build admission lock: {0}")]
    Io(String),
    #[error("{0}")]
    Busy(BuildAdmissionBusy),
}

/// Clock used for owner metadata and deterministic admission tests.
pub(crate) trait BuildAdmissionClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

#[derive(Debug, Default)]
pub(crate) struct SystemBuildAdmissionClock;

impl BuildAdmissionClock for SystemBuildAdmissionClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or_default()
    }
}

/// Build-admission coordinator shared by unified-exec managers in one process.
#[derive(Clone)]
pub(crate) struct BuildAdmission {
    clock: Arc<dyn BuildAdmissionClock>,
}

impl std::fmt::Debug for BuildAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuildAdmission")
            .finish_non_exhaustive()
    }
}

impl BuildAdmission {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            clock: Arc::new(SystemBuildAdmissionClock),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_clock(clock: Arc<dyn BuildAdmissionClock>) -> Arc<Self> {
        Arc::new(Self { clock })
    }

    /// Resolve and classify a command before process creation. Focused
    /// package-filtered commands return `Ok(None)` and are never serialized
    /// behind the broad-workspace lease.
    pub(crate) fn try_acquire(
        &self,
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
        session_id: &str,
    ) -> Result<Option<Arc<BuildAdmissionGuard>>, BuildAdmissionError> {
        let Some(kind) = classify_broad_rust_command(command) else {
            return Ok(None);
        };
        let effective_environment = effective_target_environment(command, env);
        // Broad Rust commands outside a Git checkout still need to run. They
        // simply have no stable checkout identity to coordinate, so leave
        // them unmanaged instead of turning admission into process creation
        // failure.
        let normalized_cwd = normalize_absolute_path(cwd, None)?;
        if find_checkout_root(&normalized_cwd).is_none() {
            return Ok(None);
        }
        let key = resolve_build_admission_key(&normalized_cwd, &effective_environment)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&key.lock_path)
            .map_err(|error| BuildAdmissionError::Io(error.to_string()))?;
        if let Err(error) = build_admission_lock::try_lock_file(&file) {
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(BuildAdmissionError::Io(error.to_string()));
            }
            let owner = read_owner_metadata(&key.owner_path, self.clock.now_ms());
            // A process death releases the OS lock, but the bounded owner
            // record can outlive it. Retry once only when liveness proves that
            // record stale; a live owner is never interrupted or displaced.
            let stale_owner = owner
                .pid
                .filter(|pid| *pid != 0)
                .is_some_and(|pid| !build_admission_lock::process_is_alive(pid));
            if !stale_owner {
                return Err(BuildAdmissionError::Busy(BuildAdmissionBusy {
                    checkout: key.checkout,
                    target_dir: key.target_dir,
                    owner,
                }));
            }
            if let Err(error) = build_admission_lock::try_lock_file(&file) {
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(BuildAdmissionError::Io(error.to_string()));
                }
                return Err(BuildAdmissionError::Busy(BuildAdmissionBusy {
                    checkout: key.checkout,
                    target_dir: key.target_dir,
                    owner,
                }));
            }
        }

        let owner = BuildAdmissionOwner {
            pid: Some(std::process::id()),
            session_id: redact_session_id(session_id),
            started_at_ms: Some(self.clock.now_ms()),
            elapsed_ms: Some(0),
        };
        if let Err(error) = write_owner_metadata(&key.owner_path, kind, &owner) {
            let _ = std::fs::remove_file(&key.owner_path);
            return Err(BuildAdmissionError::Io(error.to_string()));
        }
        Ok(Some(Arc::new(BuildAdmissionGuard { file, key, owner })))
    }
}

/// RAII lease retained by the process entry until exit/cancel/error cleanup.
pub(crate) struct BuildAdmissionGuard {
    file: File,
    key: BuildAdmissionKey,
    owner: BuildAdmissionOwner,
}

impl std::fmt::Debug for BuildAdmissionGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuildAdmissionGuard")
            .field("key", &self.key)
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl BuildAdmissionGuard {
    #[cfg(test)]
    pub(crate) fn key(&self) -> &BuildAdmissionKey {
        &self.key
    }
}

impl Drop for BuildAdmissionGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.key.owner_path);
        build_admission_lock::unlock_file(&self.file);
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnerMetadata {
    pid: u32,
    session_id: String,
    started_at_ms: i64,
}

fn write_owner_metadata(
    path: &Path,
    kind: BroadRustCommand,
    owner: &BuildAdmissionOwner,
) -> std::io::Result<()> {
    let metadata = OwnerMetadata {
        pid: owner.pid.unwrap_or_default(),
        session_id: owner.session_id.clone().unwrap_or_default(),
        started_at_ms: owner.started_at_ms.unwrap_or_default(),
    };
    let mut encoded =
        serde_json::to_vec(&metadata).map_err(|error| std::io::Error::other(error.to_string()))?;
    // Keep a tiny command-kind marker useful during diagnostics without ever
    // persisting the command or its arguments.
    encoded.extend_from_slice(format!("\nkind={kind:?}").as_bytes());
    if encoded.len() > OWNER_METADATA_MAX_BYTES {
        return Err(std::io::Error::other("owner metadata exceeds bound"));
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(&encoded)?;
    file.flush()
}

fn read_owner_metadata(path: &Path, now_ms: i64) -> BuildAdmissionOwner {
    let mut bytes = Vec::new();
    let read_ok = File::open(path)
        .and_then(|mut file| {
            std::io::Read::by_ref(&mut file)
                .take(OWNER_METADATA_MAX_BYTES as u64)
                .read_to_end(&mut bytes)
        })
        .is_ok();
    if !read_ok {
        return BuildAdmissionOwner {
            pid: None,
            session_id: None,
            started_at_ms: None,
            elapsed_ms: None,
        };
    }
    let json = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let Ok(metadata) = serde_json::from_slice::<OwnerMetadata>(json) else {
        return BuildAdmissionOwner {
            pid: None,
            session_id: None,
            started_at_ms: None,
            elapsed_ms: None,
        };
    };
    let elapsed_ms = if metadata.started_at_ms >= 0 && now_ms >= metadata.started_at_ms {
        u64::try_from(now_ms - metadata.started_at_ms).ok()
    } else {
        None
    };
    BuildAdmissionOwner {
        pid: Some(metadata.pid),
        session_id: redact_session_id(&metadata.session_id),
        started_at_ms: Some(metadata.started_at_ms),
        elapsed_ms,
    }
}

fn redact_session_id(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let mut bounded = String::new();
    for character in session_id
        .chars()
        .filter(|character| !character.is_control())
    {
        if bounded.len().saturating_add(character.len_utf8()) > MAX_OWNER_SESSION_BYTES {
            break;
        }
        bounded.push(character);
    }
    (!bounded.is_empty()).then_some(bounded)
}

/// Resolve checkout root and effective Cargo target directory. Relative
/// `CARGO_TARGET_DIR` values follow Cargo's current-working-directory rule.
pub(crate) fn resolve_build_admission_key(
    cwd: &Path,
    env: &[(String, String)],
) -> Result<BuildAdmissionKey, BuildAdmissionError> {
    let cwd = normalize_absolute_path(cwd, None)?;
    let checkout = find_checkout_root(&cwd).ok_or_else(|| {
        BuildAdmissionError::Resolve("working directory is not inside a Git checkout".to_string())
    })?;
    let target_dir = env
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case("CARGO_TARGET_DIR"))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| normalize_absolute_path(Path::new(value), Some(&cwd)))
        .transpose()?
        .unwrap_or_else(|| checkout.join("target"));
    std::fs::create_dir_all(&target_dir)
        .map_err(|error| BuildAdmissionError::Io(error.to_string()))?;
    let target_dir = normalize_existing_or_absolute(&target_dir);
    let lock_path = target_dir.join(LOCK_FILE_NAME);
    let owner_path = target_dir.join(".codex-build-admission.owner");
    Ok(BuildAdmissionKey {
        checkout,
        target_dir,
        lock_path,
        owner_path,
    })
}

fn find_checkout_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd;
    loop {
        if current.join(".git").exists() {
            return Some(normalize_existing_or_absolute(current));
        }
        current = current.parent()?;
    }
}

fn normalize_absolute_path(
    path: &Path,
    base: Option<&Path>,
) -> Result<PathBuf, BuildAdmissionError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.unwrap_or_else(|| Path::new(".")).join(path)
    };
    if path.as_os_str().is_empty() {
        return Err(BuildAdmissionError::Resolve("empty path".to_string()));
    }
    Ok(normalize_existing_or_absolute(&path))
}

fn normalize_existing_or_absolute(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|current| current.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Identify broad commands while tolerating shell wrappers such as `bash -lc`
/// and `pwsh -Command`. A package selector makes the command focused.
pub(crate) fn classify_broad_rust_command(command: &[String]) -> Option<BroadRustCommand> {
    let tokens = shell_tokens(command);
    for (index, token) in tokens.iter().enumerate() {
        let executable = Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(token)
            .to_ascii_lowercase();
        if executable == "cargo" || executable == "cargo.exe" {
            let Some((subcommand_index, subcommand)) = tokens
                .iter()
                .enumerate()
                .skip(index + 1)
                .take_while(|(_, token)| !is_shell_separator(token))
                .find(|(_, token)| matches!(token.as_str(), "build" | "check" | "test" | "clippy"))
            else {
                continue;
            };
            let kind = match subcommand.as_str() {
                "build" => BroadRustCommand::CargoBuild,
                "check" => BroadRustCommand::CargoCheck,
                "test" => BroadRustCommand::CargoTest,
                "clippy" => BroadRustCommand::CargoClippy,
                _ => continue,
            };
            if !has_package_selector(&tokens, subcommand_index) {
                return Some(kind);
            }
            continue;
        }
        if executable != "just" && executable != "just.exe" {
            continue;
        }
        let Some((recipe_index, kind)) = tokens[index + 1..]
            .iter()
            .enumerate()
            .take_while(|(_, token)| !is_shell_separator(token))
            .find_map(|(offset, token)| {
                match token.as_str() {
                    "test" => Some(BroadRustCommand::JustTest),
                    "clippy" => Some(BroadRustCommand::JustClippy),
                    "fix" => Some(BroadRustCommand::JustFix),
                    _ => None,
                }
                .map(|kind| (index + 1 + offset, kind))
            })
        else {
            continue;
        };
        if !has_package_selector(&tokens, recipe_index) {
            return Some(kind);
        }
    }
    None
}

fn has_package_selector(tokens: &[String], command_index: usize) -> bool {
    tokens
        .iter()
        .skip(command_index + 1)
        .take_while(|token| !is_shell_separator(token) && token.as_str() != "--")
        .any(|token| {
            token == "-p"
                || token == "--package"
                || token == "--exclude"
                || (token.starts_with("-p") && token.len() > 2)
                || token.starts_with("--package=")
                || token.starts_with("--exclude=")
        })
}

fn is_shell_separator(token: &str) -> bool {
    matches!(token, ";" | "&&" | "||" | "&" | "|")
}

fn shell_tokens(command: &[String]) -> Vec<String> {
    command
        .iter()
        .flat_map(|part| {
            shlex::split(part).unwrap_or_else(|| {
                part.split_whitespace()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
        })
        .collect()
}

fn effective_target_environment(
    command: &[String],
    env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut resolved = env.to_vec();
    let tokens = shell_tokens(command);
    for token in &tokens {
        let Some((name, value)) = token.split_once('=') else {
            continue;
        };
        if name.eq_ignore_ascii_case("CARGO_TARGET_DIR") {
            set_effective_target_dir(&mut resolved, value);
        }
    }
    if let Some(target_dir) = cargo_target_dir(&tokens) {
        set_effective_target_dir(&mut resolved, &target_dir);
    }
    resolved
}

fn set_effective_target_dir(environment: &mut Vec<(String, String)>, value: &str) {
    if let Some(existing) = environment
        .iter_mut()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case("CARGO_TARGET_DIR"))
    {
        existing.1 = value.to_string();
    } else {
        environment.push(("CARGO_TARGET_DIR".to_string(), value.to_string()));
    }
}

/// Return the target directory selected by a Cargo command-line option.
/// Cargo accepts the long option in both separated and `=` forms. Some Cargo
/// frontends also expose `-t` as the short spelling, so accept its separated
/// and attached forms as well. Only tokens belonging to a Cargo invocation
/// are inspected; shell separators and the test-binary `--` boundary stop
/// option parsing.
fn cargo_target_dir(tokens: &[String]) -> Option<String> {
    let mut target_dir = None;
    for (cargo_index, token) in tokens.iter().enumerate() {
        let executable = Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(token)
            .to_ascii_lowercase();
        if executable != "cargo" && executable != "cargo.exe" {
            continue;
        }
        let mut index = cargo_index + 1;
        while index < tokens.len() && !is_shell_separator(&tokens[index]) {
            let token = &tokens[index];
            if token == "--" {
                break;
            }
            if token == "--target-dir" || token == "-t" {
                if let Some(value) = tokens.get(index + 1)
                    && !is_shell_separator(value)
                    && value != "--"
                {
                    target_dir = Some(value.clone());
                    index += 2;
                    continue;
                }
                index += 1;
                continue;
            }
            if let Some(value) = token.strip_prefix("--target-dir=") {
                if !value.is_empty() {
                    target_dir = Some(value.to_string());
                }
                index += 1;
                continue;
            }
            if let Some(value) = token.strip_prefix("-t=") {
                if !value.is_empty() {
                    target_dir = Some(value.to_string());
                }
                index += 1;
                continue;
            }
            if let Some(value) = token.strip_prefix("-t")
                && !value.is_empty()
                && !value.starts_with('-')
            {
                target_dir = Some(value.to_string());
            }
            index += 1;
        }
    }
    target_dir
}

#[cfg(test)]
#[path = "build_admission_tests.rs"]
mod tests;
