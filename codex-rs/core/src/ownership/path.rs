use std::collections::BTreeMap;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_AUTHORIZED_ROOTS: usize = 256;

/// Errors returned when a path cannot be proven to be inside an authorized workspace root.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OwnershipPathError {
    #[error("workspace path must be absolute: {path}")]
    NotAbsolute { path: PathBuf },
    #[error("workspace path contains a NUL byte")]
    ContainsNul,
    #[error("workspace path exceeds the {MAX_PATH_BYTES}-byte limit")]
    PathTooLong,
    #[error("workspace path contains a component larger than {MAX_COMPONENT_BYTES} bytes")]
    ComponentTooLong,
    #[error("at least one authorized workspace root is required")]
    NoRoots,
    #[error("too many authorized workspace roots")]
    TooManyRoots,
    #[error("authorized workspace root does not exist: {path}")]
    RootMissing { path: PathBuf },
    #[error("authorized workspace root is not a directory: {path}")]
    RootNotDirectory { path: PathBuf },
    #[error("path is outside every authorized workspace root: {path}")]
    OutsideRoots { path: PathBuf },
    #[error("path contains `..` after a missing ancestor: {path}")]
    ParentAfterMissing { path: PathBuf },
    #[error("failed to resolve workspace path {path}: {message}")]
    Resolve { path: PathBuf, message: String },
    #[error("workspace path changed before mutation: {path}")]
    Changed { path: PathBuf },
}

/// Canonicalized, deduplicated roots authorized for a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedWorkspaceRoots {
    roots: Vec<AuthorizedRoot>,
    /// FORK: authorized roots that never require a path lease.
    ///
    /// They stay in `roots` so paths inside them still normalize; they are only
    /// excluded from lease admission. See
    /// `Config::lease_exempt_workspace_roots`.
    lease_exempt: Vec<Vec<String>>,
}

/// A normalized lease path and the root that authorizes it.
///
/// `resolved` follows every existing symlink/junction/reparse component. `display` keeps the
/// caller's logical path (with dot components normalized) for UI and diagnostics. The private
/// component guards make [`Self::revalidate_before_mutation`] fail closed if any ancestor is
/// replaced between admission and mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedLeasePath {
    pub resolved: PathBuf,
    pub display: PathBuf,
    pub comparison_key: String,
    pub authorized_root: PathBuf,
    source_path: PathBuf,
    authorized_root_source: PathBuf,
    guards: Vec<ComponentGuard>,
    authorized_root_guards: Vec<ComponentGuard>,
    components: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthorizedRoot {
    source_path: PathBuf,
    resolved: PathBuf,
    components: Vec<String>,
    guards: Vec<ComponentGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedPath {
    source_path: PathBuf,
    resolved: PathBuf,
    display: PathBuf,
    comparison_key: String,
    components: Vec<String>,
    guards: Vec<ComponentGuard>,
    had_missing_component: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComponentGuard {
    logical_key: String,
    resolved_key: String,
    metadata: MetadataFingerprint,
    link_target_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetadataFingerprint {
    file: bool,
    directory: bool,
    symlink: bool,
    len: u64,
    readonly: bool,
    modified: Option<(u64, u32)>,
}

impl AuthorizedWorkspaceRoots {
    /// Canonicalize and retain a bounded set of existing directory roots.
    pub fn new<I, P>(roots: I) -> Result<Self, OwnershipPathError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut deduplicated = BTreeMap::new();
        for root in roots {
            let root = resolve_path(root.as_ref())?;
            if root.had_missing_component {
                return Err(OwnershipPathError::RootMissing {
                    path: root.source_path,
                });
            }
            if !fs::metadata(&root.resolved)
                .map_err(|error| resolve_error(&root.resolved, error))?
                .is_dir()
            {
                return Err(OwnershipPathError::RootNotDirectory {
                    path: root.source_path,
                });
            }
            deduplicated
                .entry(root.comparison_key.clone())
                .or_insert(root);
            if deduplicated.len() > MAX_AUTHORIZED_ROOTS {
                return Err(OwnershipPathError::TooManyRoots);
            }
        }
        if deduplicated.is_empty() {
            return Err(OwnershipPathError::NoRoots);
        }

        let roots = deduplicated
            .into_values()
            .map(|root| AuthorizedRoot {
                source_path: root.source_path,
                resolved: root.resolved,
                components: root.components,
                guards: root.guards,
            })
            .collect();
        Ok(Self {
            roots,
            lease_exempt: Vec::new(),
        })
    }

    /// FORK: mark roots that are authorized but never need a path lease.
    ///
    /// A root that does not exist yet is retained by its normalized components:
    /// the scratch directory is created lazily and must still be exempt before
    /// its first write.
    pub fn with_lease_exempt_roots<I, P>(mut self, roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        for root in roots {
            let Ok(resolved) = resolve_path(root.as_ref()) else {
                continue;
            };
            if !self.lease_exempt.contains(&resolved.components) {
                self.lease_exempt.push(resolved.components);
            }
            if self.lease_exempt.len() >= MAX_AUTHORIZED_ROOTS {
                break;
            }
        }
        self
    }

    /// FORK: whether a normalized path lies under a lease-exempt root.
    pub fn is_lease_exempt(&self, path: &NormalizedLeasePath) -> bool {
        self.lease_exempt
            .iter()
            .any(|exempt| components_prefix(exempt, &path.components))
    }

    /// FORK: whether any lease-exempt root was configured.
    pub fn has_lease_exempt_roots(&self) -> bool {
        !self.lease_exempt.is_empty()
    }

    /// Normalize an absolute path and select the most-specific authorized root.
    pub fn normalize<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<NormalizedLeasePath, OwnershipPathError> {
        let resolved = resolve_path(path.as_ref())?;
        let root = self
            .roots
            .iter()
            .filter(|root| components_prefix(&root.components, &resolved.components))
            .max_by_key(|root| root.components.len())
            .ok_or_else(|| OwnershipPathError::OutsideRoots {
                path: resolved.source_path.clone(),
            })?;

        Ok(NormalizedLeasePath {
            resolved: resolved.resolved,
            display: resolved.display,
            comparison_key: resolved.comparison_key,
            authorized_root: root.resolved.clone(),
            source_path: resolved.source_path,
            authorized_root_source: root.source_path.clone(),
            guards: resolved.guards,
            authorized_root_guards: root.guards.clone(),
            components: resolved.components,
        })
    }

    /// Return roots in canonical comparison order.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|root| root.resolved.as_path())
    }
}

impl NormalizedLeasePath {
    /// Re-resolve every existing component and reject any filesystem change before mutation.
    pub fn revalidate_before_mutation(&self) -> Result<(), OwnershipPathError> {
        let current_root = resolve_path(&self.authorized_root_source)?;
        if current_root.had_missing_component
            || current_root.comparison_key != comparison_key(&self.authorized_root)
            || current_root.guards != self.authorized_root_guards
        {
            return Err(OwnershipPathError::Changed {
                path: self.source_path.clone(),
            });
        }

        let current_path = resolve_path(&self.source_path)?;
        if !components_prefix(&current_root.components, &current_path.components)
            || current_path.comparison_key != self.comparison_key
            || current_path.guards != self.guards
        {
            return Err(OwnershipPathError::Changed {
                path: self.source_path.clone(),
            });
        }
        Ok(())
    }

    /// True when this path is an ancestor of or equal to `other` by components, not string prefix.
    pub fn is_ancestor_or_equal(&self, other: &Self) -> bool {
        components_prefix(&self.components, &other.components)
    }

    /// True when this path is equal to or below `ancestor` by components.
    pub fn is_equal_or_descendant_of(&self, ancestor: &Self) -> bool {
        ancestor.is_ancestor_or_equal(self)
    }

    /// Return the canonical path used for filesystem operations.
    pub fn resolved_path(&self) -> &Path {
        &self.resolved
    }

    /// Return the logical path supplied by the caller after dot normalization.
    pub fn display_path(&self) -> &Path {
        &self.display
    }

    /// Return the stable, component-aware comparison key.
    pub fn comparison_key(&self) -> &str {
        &self.comparison_key
    }

    /// Return the canonical root that authorized this path.
    pub fn authorized_root_path(&self) -> &Path {
        &self.authorized_root
    }
}

fn resolve_path(path: &Path) -> Result<ResolvedPath, OwnershipPathError> {
    validate_path(path)?;
    if !path.is_absolute() {
        return Err(OwnershipPathError::NotAbsolute {
            path: path.to_path_buf(),
        });
    }

    let source_path = path.to_path_buf();
    let display = normalize_display_path(path)?;
    let mut current = PathBuf::new();
    let mut guards = Vec::new();
    let mut had_missing_component = false;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if had_missing_component {
                    return Err(OwnershipPathError::ParentAfterMissing { path: source_path });
                }
                if !current.pop() {
                    return Err(OwnershipPathError::Resolve {
                        path: source_path,
                        message: "parent component escapes the filesystem root".to_string(),
                    });
                }
            }
            Component::Normal(name) => {
                current.push(name);
                if had_missing_component {
                    continue;
                }
                let metadata = match fs::symlink_metadata(&current) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        had_missing_component = true;
                        continue;
                    }
                    Err(error) => return Err(resolve_error(&current, error)),
                };
                let resolved_component = dunce::canonicalize(&current)
                    .map_err(|error| resolve_error(&current, error))?;
                let resolved_component = normalize_windows_namespace(&resolved_component);
                guards.push(ComponentGuard {
                    logical_key: comparison_key(&current),
                    resolved_key: comparison_key(&resolved_component),
                    metadata: MetadataFingerprint::from_metadata(&metadata),
                    link_target_key: link_target_key(&current, &metadata),
                });
                current = resolved_component;
            }
        }
    }

    let resolved = normalize_windows_namespace(&current);
    let components = comparison_components(&resolved);
    let comparison_key = comparison_key_from_components(&components);
    Ok(ResolvedPath {
        source_path,
        resolved,
        display,
        comparison_key,
        components,
        guards,
        had_missing_component,
    })
}

fn normalize_display_path(path: &Path) -> Result<PathBuf, OwnershipPathError> {
    let mut display = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => display.push(prefix.as_os_str()),
            Component::RootDir => display.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !display.pop() {
                    return Err(OwnershipPathError::Resolve {
                        path: path.to_path_buf(),
                        message: "parent component escapes the filesystem root".to_string(),
                    });
                }
            }
            Component::Normal(name) => display.push(name),
        }
    }
    if !display.is_absolute() {
        return Err(OwnershipPathError::NotAbsolute {
            path: path.to_path_buf(),
        });
    }
    Ok(normalize_windows_namespace(&display))
}

fn validate_path(path: &Path) -> Result<(), OwnershipPathError> {
    let lossy = path.to_string_lossy();
    if lossy.contains('\0') {
        return Err(OwnershipPathError::ContainsNul);
    }
    if lossy.len() > MAX_PATH_BYTES {
        return Err(OwnershipPathError::PathTooLong);
    }
    for component in path.components() {
        if let Component::Normal(component) = component
            && component.to_string_lossy().len() > MAX_COMPONENT_BYTES
        {
            return Err(OwnershipPathError::ComponentTooLong);
        }
    }
    Ok(())
}

fn components_prefix(prefix: &[String], path: &[String]) -> bool {
    path.len() >= prefix.len() && prefix.iter().zip(path).all(|(left, right)| left == right)
}

fn comparison_key(path: &Path) -> String {
    comparison_key_from_components(&comparison_components(path))
}

fn comparison_key_from_components(components: &[String]) -> String {
    components
        .iter()
        .map(|component| format!("{}:{component}", component.len()))
        .collect::<Vec<_>>()
        .join("/")
}

fn comparison_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| match component {
            Component::Prefix(prefix) => normalize_component(prefix.as_os_str()),
            Component::RootDir => "/".to_string(),
            Component::CurDir => ".".to_string(),
            Component::ParentDir => "..".to_string(),
            Component::Normal(name) => normalize_component(name),
        })
        .collect()
}

fn normalize_component(component: &std::ffi::OsStr) -> String {
    let value = component.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        value.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}

fn resolve_error(path: &Path, error: std::io::Error) -> OwnershipPathError {
    OwnershipPathError::Resolve {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn link_target_key(path: &Path, metadata: &fs::Metadata) -> Option<String> {
    metadata
        .file_type()
        .is_symlink()
        .then(|| fs::read_link(path).ok())
        .flatten()
        .map(|target| comparison_key(&target))
}

impl MetadataFingerprint {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        // FORK: a plain directory's size and mtime change whenever *any* entry
        // is created or removed inside it, which is ordinary concurrent work,
        // not tampering. Fingerprinting them made every admitted mutation fail
        // with "workspace path changed before mutation" as soon as a second
        // agent wrote anything in the same directory - and the workspace root
        // is an ancestor of everything. Identity still comes from the
        // canonicalized resolved path, the link target and the entry type;
        // files, symlinks included, keep the full fingerprint.
        let plain_directory = metadata.is_dir() && !metadata.file_type().is_symlink();
        let modified = (!plain_directory)
            .then(|| {
                metadata.modified().ok().and_then(|time| {
                    time.duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
                })
            })
            .flatten();
        Self {
            file: metadata.is_file(),
            directory: metadata.is_dir(),
            symlink: metadata.file_type().is_symlink(),
            len: if plain_directory { 0 } else { metadata.len() },
            readonly: metadata.permissions().readonly(),
            modified,
        }
    }
}

#[cfg(windows)]
fn normalize_windows_namespace(path: &Path) -> PathBuf {
    codex_utils_absolute_path::normalize_windows_device_path(&path.to_string_lossy())
        .map(PathBuf::from)
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(not(windows))]
fn normalize_windows_namespace(path: &Path) -> PathBuf {
    path.to_path_buf()
}
