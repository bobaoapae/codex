//! Process-shared serialization for plan filesystem operations.

use fd_lock::RwLock;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub(crate) const PLAN_LOCK_FILE_NAME: &str = ".lock";

/// Run blocking filesystem work on the Tokio blocking pool.
pub(crate) async fn blocking<T, F>(operation: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    blocking_result(operation).await
}

/// Run a blocking operation whose domain-specific error can absorb I/O failures.
pub(crate) async fn blocking_result<T, E, F>(operation: F) -> Result<T, E>
where
    T: Send + 'static,
    E: From<io::Error> + Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| E::from(io::Error::other(error)))?
}

/// Hold the one plans lock across scan, decision, and write operations.
pub(crate) async fn with_write_lock<T, E, F>(plans_dir: PathBuf, operation: F) -> Result<T, E>
where
    T: Send + 'static,
    E: From<io::Error> + Send + 'static,
    F: FnOnce(&Path) -> Result<T, E> + Send + 'static,
{
    blocking_result(move || {
        ensure_plans_directory(&plans_dir).map_err(E::from)?;
        let lock_path = plans_dir.join(PLAN_LOCK_FILE_NAME);
        ensure_lock_destination(&lock_path).map_err(E::from)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(E::from)?;
        let mut lock = RwLock::new(file);
        let _guard = lock.write().map_err(E::from)?;
        operation(&plans_dir)
    })
    .await
}

fn ensure_lock_destination(path: &Path) -> io::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (!metadata.file_type().is_file() || is_reparse_point(&metadata))
    {
        return Err(unsafe_path_error(path));
    }
    Ok(())
}

fn ensure_plans_directory(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("unsafe plans directory: {}", path.display()),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error),
    }
    ensure_safe_directory(path)
}

/// Open a regular file while rejecting symlink/reparse-point targets.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<Option<File>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        validate_existing_ancestors(parent)?;
    }
    File::open(path).map(Some)
}

/// Reject a directory if any existing component is a symlink/reparse point.
pub(crate) fn ensure_safe_directory(path: &Path) -> io::Result<()> {
    validate_existing_ancestors(path)?;
    if let Err(error) = std::fs::create_dir_all(path)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(error);
    }
    validate_existing_ancestors(path)?;
    Ok(())
}

/// Check a directory without creating it. `false` means that the final path is absent.
pub(crate) fn check_safe_directory(path: &Path) -> io::Result<bool> {
    validate_existing_ancestors(path)?;
    let Some(metadata) = (match std::fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    }) else {
        return Ok(false);
    };
    if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
        return Err(unsafe_path_error(path));
    }
    Ok(true)
}

/// Reject an existing destination that is not a regular, non-reparse file.
pub(crate) fn check_safe_file_destination(path: &Path) -> io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        return Err(unsafe_path_error(path));
    }
    validate_existing_ancestors(
        path.parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?,
    )?;
    Ok(true)
}

fn validate_existing_ancestors(path: &Path) -> io::Result<()> {
    let mut current = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
                    return Err(unsafe_path_error(&current));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(())
}

fn unsafe_path_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsafe plan path component: {}", path.display()),
    )
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}
