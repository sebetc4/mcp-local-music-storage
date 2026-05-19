use std::io;
use std::path::{Path, PathBuf};

use crate::core::config::Config;

/// Errors that can occur during path validation
#[derive(Debug, thiserror::Error)]
pub enum PathSecurityError {
    #[error("Path '{path}' is outside allowed root directory '{root}'")]
    OutsideRootDirectory { path: PathBuf, root: PathBuf },

    #[error("Symlink '{path}' points outside allowed root directory")]
    SymlinkOutsideRoot { path: PathBuf },

    #[error("Symlink '{path}' is not allowed by current security policy")]
    SymlinkNotAllowed { path: PathBuf },

    #[error("Cannot canonicalize path '{path}': {error}")]
    CannotCanonicalize { path: PathBuf, error: io::Error },

    #[error("Path does not exist: '{path}'")]
    PathNotFound { path: PathBuf },

    #[error("IO error for path '{path}': {error}")]
    IoError { path: PathBuf, error: io::Error },
}

/// Validates that a given path is within the configured security boundaries.
///
/// # Symlink policy
///
/// * `allow_symlinks = false` (strict): any symlink encountered as the input path is
///   rejected outright with [`PathSecurityError::SymlinkNotAllowed`], regardless of
///   where it points.
/// * `allow_symlinks = true`: symlinks are followed via canonicalization. If the
///   canonical target lies outside the configured root, the error is reported as
///   [`PathSecurityError::SymlinkOutsideRoot`] (vs. `OutsideRootDirectory` for plain
///   `..` traversal) so callers can distinguish the two cases.
///
/// # Arguments
///
/// * `input_path` - The path to validate (can be relative or absolute)
/// * `config` - The server configuration containing security settings
///
/// # Returns
///
/// * `Ok(PathBuf)` - The canonicalized, validated path
/// * `Err(PathSecurityError)` - If validation fails
pub fn validate_path(input_path: &str, config: &Config) -> Result<PathBuf, PathSecurityError> {
    let path = Path::new(input_path);

    // If no root path is configured, only do basic canonicalization
    let Some(ref root) = config.security.root_path else {
        return canonicalize_path(path);
    };

    // Canonicalize the root path first
    let canonical_root = root
        .canonicalize()
        .map_err(|e| PathSecurityError::IoError {
            path: root.clone(),
            error: e,
        })?;

    // Check if path exists before canonicalization
    if !path.exists() {
        return Err(PathSecurityError::PathNotFound {
            path: path.to_path_buf(),
        });
    }

    // Strict symlink policy: when symlinks are disallowed, reject any symlink encountered
    // as the input path regardless of where its target lies.
    let is_symlink = path.is_symlink();
    if is_symlink && !config.security.allow_symlinks {
        return Err(PathSecurityError::SymlinkNotAllowed {
            path: path.to_path_buf(),
        });
    }

    // Canonicalize the input path (resolves both `..` traversal and symlinks)
    let canonical_path =
        path.canonicalize()
            .map_err(|e| PathSecurityError::CannotCanonicalize {
                path: path.to_path_buf(),
                error: e,
            })?;

    // Verify the canonical path is within the root. Distinguish symlink escapes
    // from plain `..` traversal so callers can handle the two cases differently.
    if !is_within_root(&canonical_path, &canonical_root) {
        return Err(if is_symlink {
            PathSecurityError::SymlinkOutsideRoot {
                path: path.to_path_buf(),
            }
        } else {
            PathSecurityError::OutsideRootDirectory {
                path: canonical_path,
                root: canonical_root,
            }
        });
    }

    Ok(canonical_path)
}

/// Validates a path that does **not** yet exist on disk (typically the
/// destination of a `mkdir` or cross-directory `move`).
///
/// `validate_path` canonicalises its input via `Path::canonicalize`, which
/// requires the file to exist. For new files / directories we must instead:
///
/// 1. lexically normalise the input (resolve `.` / `..` without touching
///    disk), and
/// 2. walk up to the deepest existing ancestor and validate *that* against
///    the configured root using [`validate_path`].
///
/// The returned [`PathBuf`] is the absolute target path joined onto the
/// canonical ancestor — guaranteed to live under the configured root, but
/// not canonicalised (it can't be: it doesn't exist yet).
pub fn validate_unborn_path(
    input_path: &str,
    config: &Config,
) -> Result<PathBuf, PathSecurityError> {
    let path = Path::new(input_path);

    // 1. Absolutise (resolve relative paths against the current working dir).
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| PathSecurityError::IoError {
                path: path.to_path_buf(),
                error: e,
            })?
            .join(path)
    };

    // 2. Lexically resolve `.` and `..`. Since the path doesn't fully exist
    // we cannot rely on canonicalize, but `..` traversal must still be caught
    // before we let it leak past the root.
    let cleaned = normalize_path(&absolute);

    // 3. Walk up to the deepest existing ancestor.
    let mut ancestor = cleaned.as_path();
    while !ancestor.exists() {
        match ancestor.parent() {
            // Stop the climb at the filesystem root; if even that doesn't
            // exist, something is very wrong (or we resolved past the root).
            Some(parent) if parent != ancestor => ancestor = parent,
            _ => {
                return Err(PathSecurityError::PathNotFound {
                    path: cleaned.clone(),
                });
            }
        }
    }

    // 4. Validate the existing ancestor against the configured root. This
    // also enforces the symlink policy: a symlinked ancestor pointing
    // outside the root is rejected exactly as it would be for an existing
    // file path.
    let canonical_ancestor = validate_path(&ancestor.to_string_lossy(), config)?;

    // 5. Stitch the not-yet-existing suffix onto the canonical ancestor.
    let suffix = cleaned
        .strip_prefix(ancestor)
        .map_err(|_| PathSecurityError::OutsideRootDirectory {
            path: cleaned.clone(),
            root: ancestor.to_path_buf(),
        })?;

    // Short-circuit: the input path already exists (suffix is empty). Joining
    // an empty path onto `canonical_ancestor` would tack on a trailing
    // separator — `fs::rename` and other POSIX calls then treat the target as
    // a directory and reject it. Return the canonical ancestor verbatim.
    if suffix.as_os_str().is_empty() {
        return Ok(canonical_ancestor);
    }

    // Defensive: `normalize_path` already strips `..`, so any survivor here
    // would mean an upstream bug — reject rather than risk a traversal.
    if suffix
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(PathSecurityError::OutsideRootDirectory {
            path: cleaned.clone(),
            root: canonical_ancestor.clone(),
        });
    }

    let final_path = canonical_ancestor.join(suffix);

    // 6. Final sanity check — the join must still land inside the canonical
    // ancestor (which itself sits under the root).
    if !final_path.starts_with(&canonical_ancestor) {
        return Err(PathSecurityError::OutsideRootDirectory {
            path: final_path,
            root: canonical_ancestor,
        });
    }

    Ok(final_path)
}

/// Resolve `.` / `..` components in a path lexically (no filesystem access).
/// Used by [`validate_unborn_path`] so that traversal attempts in
/// not-yet-existing paths are caught before we hand the path to `mkdir` or
/// `rename`.
fn normalize_path(p: &Path) -> PathBuf {
    use std::path::Component;

    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // Cannot escape the root — drop a `..` that would try to.
                Some(Component::Prefix(_)) | Some(Component::RootDir) | None => {}
                // Relative-path edge case: stack of `..` accumulates.
                Some(Component::ParentDir) => out.push(comp),
                Some(Component::CurDir) => unreachable!("CurDir filtered earlier"),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Checks if a path is within (or equal to) a root directory
fn is_within_root(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Attempts to canonicalize a path, returning it as-is if canonicalization fails
/// (e.g., for non-existent paths)
fn canonicalize_path(path: &Path) -> Result<PathBuf, PathSecurityError> {
    path.canonicalize().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            PathSecurityError::PathNotFound {
                path: path.to_path_buf(),
            }
        } else {
            PathSecurityError::CannotCanonicalize {
                path: path.to_path_buf(),
                error: e,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config(root: Option<PathBuf>, allow_symlinks: bool) -> Config {
        use crate::core::config::SecurityConfig;

        let mut config = Config::default();
        config.security = SecurityConfig {
            root_path: root,
            allow_symlinks,
        };
        config
    }

    #[test]
    fn test_no_root_allows_existing_paths() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "test").unwrap();

        let config = create_test_config(None, true);
        let result = validate_path(test_file.to_str().unwrap(), &config);

        assert!(result.is_ok());
    }

    #[test]
    fn test_path_within_root() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "test").unwrap();

        let config = create_test_config(Some(temp_dir.path().to_path_buf()), true);
        let result = validate_path(test_file.to_str().unwrap(), &config);

        assert!(result.is_ok());
    }

    #[test]
    fn test_path_outside_root() {
        let root_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let outside_file = outside_dir.path().join("outside.txt");
        fs::write(&outside_file, "test").unwrap();

        let config = create_test_config(Some(root_dir.path().to_path_buf()), true);
        let result = validate_path(outside_file.to_str().unwrap(), &config);

        assert!(matches!(
            result,
            Err(PathSecurityError::OutsideRootDirectory { .. })
        ));
    }

    #[test]
    fn test_path_traversal_blocked() {
        let temp_dir = TempDir::new().unwrap();
        let subdir = temp_dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();

        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "test").unwrap();

        // Try to access parent directory file from subdir using ../
        let config = create_test_config(Some(subdir.clone()), true);
        let traversal_path = subdir.join("../test.txt");

        let result = validate_path(traversal_path.to_str().unwrap(), &config);

        // Should fail because canonical path resolves to temp_dir/test.txt
        // which is outside the subdir root
        assert!(matches!(
            result,
            Err(PathSecurityError::OutsideRootDirectory { .. })
        ));
    }

    #[test]
    fn test_nonexistent_path() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent = temp_dir.path().join("does_not_exist.txt");

        let config = create_test_config(Some(temp_dir.path().to_path_buf()), true);
        let result = validate_path(nonexistent.to_str().unwrap(), &config);

        assert!(matches!(
            result,
            Err(PathSecurityError::PathNotFound { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_within_root() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let target_file = temp_dir.path().join("target.txt");
        let link_file = temp_dir.path().join("link.txt");

        fs::write(&target_file, "test").unwrap();
        symlink(&target_file, &link_file).unwrap();

        let config = create_test_config(Some(temp_dir.path().to_path_buf()), true);
        let result = validate_path(link_file.to_str().unwrap(), &config);

        assert!(result.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_outside_root_blocked() {
        use std::os::unix::fs::symlink;

        let root_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();

        let target_file = outside_dir.path().join("target.txt");
        let link_file = root_dir.path().join("link.txt");

        fs::write(&target_file, "test").unwrap();
        symlink(&target_file, &link_file).unwrap();

        let config = create_test_config(Some(root_dir.path().to_path_buf()), true);
        let result = validate_path(link_file.to_str().unwrap(), &config);

        assert!(matches!(
            result,
            Err(PathSecurityError::SymlinkOutsideRoot { .. })
        ));
    }

    #[test]
    fn unborn_path_under_root_returns_target() {
        let root = TempDir::new().unwrap();
        let config = create_test_config(Some(root.path().to_path_buf()), true);

        // root/new/album doesn't exist yet — should be accepted (root exists).
        let target = root.path().join("new").join("album");
        let result = validate_unborn_path(target.to_str().unwrap(), &config).unwrap();

        // Returned path lives under the canonical root.
        let canonical_root = root.path().canonicalize().unwrap();
        assert!(result.starts_with(&canonical_root));
        assert!(result.ends_with("new/album"));
        // And the directory was NOT created — pure validation, no I/O on the target.
        assert!(!target.exists());
    }

    #[test]
    fn unborn_path_rejects_traversal() {
        let root = TempDir::new().unwrap();
        let config = create_test_config(Some(root.path().to_path_buf()), true);

        // root/foo/../../escape resolves lexically to <parent of root>/escape,
        // which sits outside the configured root.
        let traversal = root.path().join("foo").join("..").join("..").join("escape");
        let result = validate_unborn_path(traversal.to_str().unwrap(), &config);
        assert!(matches!(
            result,
            Err(PathSecurityError::OutsideRootDirectory { .. })
        ));
    }

    #[test]
    fn unborn_path_resolves_dot_components() {
        let root = TempDir::new().unwrap();
        let config = create_test_config(Some(root.path().to_path_buf()), true);

        // root/./a/./b should resolve to root/a/b.
        let weird = root.path().join(".").join("a").join(".").join("b");
        let result = validate_unborn_path(weird.to_str().unwrap(), &config).unwrap();

        let canonical_root = root.path().canonicalize().unwrap();
        assert_eq!(result, canonical_root.join("a").join("b"));
    }

    #[test]
    fn unborn_path_walks_up_through_missing_ancestors() {
        let root = TempDir::new().unwrap();
        let config = create_test_config(Some(root.path().to_path_buf()), true);

        // 4 levels deep, none exist: root/A/B/C/D
        let deep = root.path().join("A").join("B").join("C").join("D");
        let result = validate_unborn_path(deep.to_str().unwrap(), &config).unwrap();

        let canonical_root = root.path().canonicalize().unwrap();
        assert_eq!(result, canonical_root.join("A/B/C/D"));
    }

    #[cfg(unix)]
    #[test]
    fn unborn_path_rejects_symlinked_ancestor_when_disallowed() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        // root/link -> outside/, then ask to mkdir root/link/album.
        let link = root.path().join("link");
        symlink(outside.path(), &link).unwrap();

        let config = create_test_config(Some(root.path().to_path_buf()), false);
        let target = link.join("album");

        let result = validate_unborn_path(target.to_str().unwrap(), &config);
        assert!(matches!(
            result,
            Err(PathSecurityError::SymlinkNotAllowed { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_disallowed_by_config() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let target_file = temp_dir.path().join("target.txt");
        let link_file = temp_dir.path().join("link.txt");

        fs::write(&target_file, "test").unwrap();
        symlink(&target_file, &link_file).unwrap();

        // Strict policy: even a symlink whose target is inside the root must be rejected.
        let config = create_test_config(Some(temp_dir.path().to_path_buf()), false);
        let result = validate_path(link_file.to_str().unwrap(), &config);

        assert!(matches!(
            result,
            Err(PathSecurityError::SymlinkNotAllowed { .. })
        ));
    }
}
