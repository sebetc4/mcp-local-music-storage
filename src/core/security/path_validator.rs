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
    let canonical_root = root.canonicalize().map_err(|e| PathSecurityError::IoError {
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
        path.canonicalize().map_err(|e| PathSecurityError::CannotCanonicalize {
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

        assert!(matches!(result, Err(PathSecurityError::PathNotFound { .. })));
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
