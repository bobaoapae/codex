use super::AuthorizedWorkspaceRoots;
use super::NormalizedLeasePath;
use super::OwnershipPathError;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

fn workspace() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().expect("workspace tempdir should be created");
    let root = temp_dir.path().join("workspace");
    fs::create_dir(&root).expect("workspace root should be created");
    (temp_dir, root)
}

fn roots(root: &Path) -> AuthorizedWorkspaceRoots {
    AuthorizedWorkspaceRoots::new([root.to_path_buf()]).expect("root should be authorized")
}

fn normalized(authorized: &AuthorizedWorkspaceRoots, path: &Path) -> NormalizedLeasePath {
    authorized
        .normalize(path)
        .expect("path should be normalized")
}

#[test]
fn exact_paths_have_component_aware_ancestor_relationships() {
    let (_temp_dir, root) = workspace();
    let authorized = roots(&root);
    let child = normalized(&authorized, &root.join("src").join("file.rs"));
    let sibling_root = {
        let sibling = root.with_file_name("workspace-other");
        fs::create_dir(&sibling).expect("sibling root should be created");
        sibling
    };
    let sibling_path_source = sibling_root.join("file.rs");
    let sibling = AuthorizedWorkspaceRoots::new([root, sibling_root])
        .expect("both roots should be authorized");
    let sibling_path = normalized(&sibling, &sibling_path_source);

    assert!(child.is_equal_or_descendant_of(&child));
    assert!(child.is_ancestor_or_equal(&child));
    assert!(!child.is_ancestor_or_equal(&sibling_path));
    assert!(!sibling_path.is_equal_or_descendant_of(&child));
    assert!(child.comparison_key().contains("/"));
}

#[test]
fn roots_are_deduplicated_sorted_and_most_specific_root_wins() {
    let (_temp_dir, root) = workspace();
    let nested = root.join("nested");
    fs::create_dir(&nested).expect("nested root should be created");
    let canonical_root = dunce::canonicalize(&root).expect("root should canonicalize");
    let canonical_nested = dunce::canonicalize(&nested).expect("nested should canonicalize");
    let authorized = AuthorizedWorkspaceRoots::new([root.clone(), nested, root])
        .expect("roots should be authorized");

    let roots = authorized.roots().collect::<Vec<_>>();
    assert_eq!(roots.len(), 2);
    assert_eq!(
        roots,
        vec![canonical_root.as_path(), canonical_nested.as_path()]
    );
    let path = normalized(&authorized, &canonical_nested.join("file.rs"));
    assert_eq!(path.authorized_root_path(), canonical_nested.as_path());
}

#[cfg(windows)]
#[test]
fn windows_paths_normalize_drive_case_and_separators() {
    let (_temp_dir, root) = workspace();
    let authorized = roots(&root);
    let exact = normalized(&authorized, &root.join("src").join("file.rs"));
    let windows_text = root.to_string_lossy().replace('\\', "/").to_uppercase();
    let equivalent = normalized(
        &authorized,
        &PathBuf::from(format!("{windows_text}/SRC/FILE.RS")),
    );

    assert_eq!(exact.comparison_key(), equivalent.comparison_key());
}

#[test]
fn nonexistent_leaf_uses_canonical_existing_ancestor_without_parent_fallback() {
    let (_temp_dir, root) = workspace();
    let authorized = roots(&root);
    let missing = root.join("new").join("leaf.txt");
    let normalized_missing = normalized(&authorized, &missing);
    let canonical_root = dunce::canonicalize(&root).expect("root should canonicalize");
    assert_eq!(
        normalized_missing.resolved_path(),
        canonical_root.join("new").join("leaf.txt")
    );
    normalized_missing
        .revalidate_before_mutation()
        .expect("unchanged nonexistent leaf should remain valid");

    let parent_after_missing = authorized.normalize(root.join("new").join("..").join("escape"));
    assert_eq!(
        parent_after_missing,
        Err(OwnershipPathError::ParentAfterMissing {
            path: root.join("new").join("..").join("escape"),
        })
    );
}

#[test]
fn relative_nul_and_component_bounds_fail_closed() {
    let (_temp_dir, root) = workspace();
    let authorized = roots(&root);

    assert!(matches!(
        authorized.normalize("relative/path"),
        Err(OwnershipPathError::NotAbsolute { .. })
    ));
    assert_eq!(
        authorized.normalize(PathBuf::from(format!("{}\0", root.display()))),
        Err(OwnershipPathError::ContainsNul)
    );
    assert_eq!(
        authorized.normalize(root.join("x".repeat(256))),
        Err(OwnershipPathError::ComponentTooLong)
    );
}

#[cfg(windows)]
#[test]
fn windows_unc_component_bounds_fail_before_filesystem_access() {
    let (_temp_dir, root) = workspace();
    let authorized = roots(&root);
    let unc = PathBuf::from(format!(r"\\server\share\{}", "x".repeat(256)));

    assert_eq!(
        authorized.normalize(unc),
        Err(OwnershipPathError::ComponentTooLong)
    );
}

#[cfg(unix)]
#[test]
fn symlink_inside_root_is_allowed_but_escape_is_rejected() {
    use std::os::unix::fs::symlink;

    let (temp_dir, root) = workspace();
    let inside = root.join("inside");
    let outside = temp_dir.path().join("outside");
    fs::create_dir(&inside).expect("inside target should be created");
    fs::create_dir(&outside).expect("outside target should be created");
    let inside_link = root.join("inside-link");
    let outside_link = root.join("outside-link");
    symlink(&inside, &inside_link).expect("inside symlink should be created");
    symlink(&outside, &outside_link).expect("outside symlink should be created");
    let authorized = roots(&root);

    let inside_path = normalized(&authorized, &inside_link.join("file.rs"));
    assert_eq!(inside_path.resolved_path(), &inside.join("file.rs"));
    assert_eq!(
        inside_path.authorized_root_path(),
        dunce::canonicalize(&root).unwrap().as_path()
    );
    assert!(matches!(
        authorized.normalize(outside_link.join("file.rs")),
        Err(OwnershipPathError::OutsideRoots { .. })
    ));
}

#[cfg(unix)]
#[test]
fn revalidation_detects_ancestor_link_swap() {
    use std::os::unix::fs::symlink;

    let (temp_dir, root) = workspace();
    let first = root.join("first");
    let second = root.join("second");
    let outside = temp_dir.path().join("outside");
    fs::create_dir(&first).expect("first target should be created");
    fs::create_dir(&second).expect("second target should be created");
    fs::create_dir(&outside).expect("outside target should be created");
    let link = root.join("link");
    symlink(&first, &link).expect("initial symlink should be created");
    let authorized = roots(&root);
    let lease = normalized(&authorized, &link.join("file.rs"));

    fs::remove_file(&link).expect("initial symlink should be removed");
    symlink(&second, &link).expect("replacement symlink should be created");
    assert!(matches!(
        lease.revalidate_before_mutation(),
        Err(OwnershipPathError::Changed { .. })
    ));

    fs::remove_file(&link).expect("second symlink should be removed");
    symlink(&outside, &link).expect("escape symlink should be created");
    assert!(lease.revalidate_before_mutation().is_err());
}

#[cfg(unix)]
#[test]
fn revalidation_detects_existing_ancestor_replacement() {
    use std::os::unix::fs::symlink;

    let (temp_dir, root) = workspace();
    let ancestor = root.join("ancestor");
    let outside = temp_dir.path().join("outside");
    fs::create_dir(&ancestor).expect("ancestor should be created");
    fs::create_dir(&outside).expect("outside should be created");
    let authorized = roots(&root);
    let lease = normalized(&authorized, &ancestor.join("missing.txt"));

    fs::remove_dir(&ancestor).expect("ancestor should be removed");
    symlink(&outside, &ancestor).expect("ancestor replacement should be created");
    assert!(matches!(
        lease.revalidate_before_mutation(),
        Err(OwnershipPathError::Changed { .. })
    ));
}
