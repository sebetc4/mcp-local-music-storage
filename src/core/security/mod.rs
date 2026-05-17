// Security module for path validation and access control
//
// This module provides utilities to ensure that file system operations
// are restricted to configured safe directories, preventing path traversal
// attacks and unauthorized access.

pub mod filename;
pub mod path_validator;

pub use filename::is_safe_filename;
pub use path_validator::{validate_path, PathSecurityError};
