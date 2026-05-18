//! MusicBrainz tools module.
//!
//! This module provides domain-specific tools for searching the MusicBrainz database:
//! - `artist`: Search for artists and their releases
//! - `release`: Search for releases, release groups, tracks, and versions
//! - `recording`: Search for recordings and find where they appear
//! - `work`: Search for works (musical compositions)
//! - `label`: Search for labels (record labels/publishers)
//! - `identify_record`: Audio fingerprinting via AcoustID
//! - `cover_download`: Download cover art images from Cover Art Archive
//!
//! Each tool has handlers for both HTTP and STDIO/TCP transports.

pub mod artist;
pub mod blocking_tool;
pub mod common;
pub mod cover_download;
pub mod identify_record;
pub mod label;
pub mod recording;
pub mod release;
pub mod work;

pub use blocking_tool::MbBlockingTool;

// Re-export domain-specific tools
pub use artist::{MbArtistParams, MbArtistTool};
pub use cover_download::{MbCoverDownloadParams, MbCoverDownloadTool};
pub use identify_record::MbIdentifyRecordTool;
pub use label::{MbLabelParams, MbLabelTool};
pub use recording::{MbRecordingParams, MbRecordingTool};
pub use release::{MbReleaseParams, MbReleaseTool};
pub use work::{MbWorkParams, MbWorkTool};
