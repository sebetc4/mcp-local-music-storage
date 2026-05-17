//! Filename safety helpers.
//!
//! These checks complement the path validator: while [`validate_path`](super::validate_path)
//! protects against `..` traversal and symlink escapes once a path is known, callers that
//! build a path from a user-supplied filename component (e.g. `dir.join(&params.filename)`)
//! must reject suspicious filenames *before* the join — otherwise a value like
//! `"../escape.jpg"` rewrites the path entirely.

/// Returns `true` when `name` is safe to use as a single filesystem path component.
///
/// A filename is considered safe when it:
/// * is non-empty,
/// * does not contain a path separator (`/` or `\`),
/// * does not contain a NUL byte,
/// * does not start with `.` (rejects both hidden files and `..`/`.`),
/// * is not exactly `.` or `..` (defensive — also covered by the dot check above).
pub fn is_safe_filename(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.starts_with('.') {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_filenames() {
        assert!(is_safe_filename("cover"));
        assert!(is_safe_filename("cover.jpg"));
        assert!(is_safe_filename("My Album Art 2024.png"));
    }

    #[test]
    fn rejects_traversal() {
        assert!(!is_safe_filename(".."));
        assert!(!is_safe_filename("../escape"));
        assert!(!is_safe_filename("../escape.jpg"));
    }

    #[test]
    fn rejects_separators() {
        assert!(!is_safe_filename("foo/bar.jpg"));
        assert!(!is_safe_filename("foo\\bar.jpg"));
        assert!(!is_safe_filename("/absolute.jpg"));
    }

    #[test]
    fn rejects_dotfiles_and_empty() {
        assert!(!is_safe_filename(""));
        assert!(!is_safe_filename("."));
        assert!(!is_safe_filename(".hidden"));
    }

    #[test]
    fn rejects_nul_byte() {
        assert!(!is_safe_filename("a\0b.jpg"));
    }
}
