//! Tool definitions module.
//!
//! This module exports all available tool definitions.
//! Each tool is defined in its own file for better maintainability.

pub mod fs;
pub mod mb;
pub mod metadata;
pub mod naming;
pub mod plan;

pub use fs::{
    FindDuplicatesTool, FsDeleteTool, FsHashTool, FsListDirTool, FsMkdirTool, FsMoveTool,
    FsRenameTool, FsScanAudioTool,
};
pub use mb::{
    MbArtistParams, MbArtistTool, MbCoverDownloadParams, MbCoverDownloadTool, MbIdentifyRecordTool,
    MbLabelParams, MbLabelTool, MbMatchFromTagsParams, MbMatchFromTagsTool, MbRecordingParams,
    MbRecordingTool, MbReleaseParams, MbReleaseTool, MbWorkParams, MbWorkTool,
};
pub use metadata::{
    EmbedCoverTool, ReadMetadataBatchTool, ReadMetadataTool, WriteMetadataBatchTool,
    WriteMetadataTool,
};
pub use naming::ApplyNamingSchemeTool;
pub use plan::ApplyPlanTool;

/// X-macro listing every tool the server exposes. Editing this list is the
/// **single source of truth**: `tool_names`, `get_all_tools`, `call_tool`
/// (HTTP dispatch), and `build_tool_router` each invoke `foreach_tool!` with a
/// visitor macro to derive their per-tool code.
///
/// Each entry is `(ToolType, with_config | no_config)`. `with_config` tools
/// need an `Arc<Config>` at construction time; `no_config` tools (the five
/// stateless MB search tools backed by the [`mb::MbBlockingTool`] trait) don't.
#[macro_export]
macro_rules! foreach_tool {
    ($visit:ident) => {
        $visit!(
            $crate::domains::tools::definitions::FsDeleteTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::FsListDirTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::FsRenameTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::FsMkdirTool,
            with_config
        );
        $visit!($crate::domains::tools::definitions::FsMoveTool, with_config);
        $visit!(
            $crate::domains::tools::definitions::FsScanAudioTool,
            with_config
        );
        $visit!($crate::domains::tools::definitions::FsHashTool, with_config);
        $visit!(
            $crate::domains::tools::definitions::FindDuplicatesTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::ReadMetadataTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::ReadMetadataBatchTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::WriteMetadataTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::WriteMetadataBatchTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::EmbedCoverTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::ApplyNamingSchemeTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::ApplyPlanTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::MbCoverDownloadTool,
            with_config
        );
        $visit!(
            $crate::domains::tools::definitions::MbIdentifyRecordTool,
            with_config
        );
        $visit!($crate::domains::tools::definitions::MbArtistTool, no_config);
        $visit!($crate::domains::tools::definitions::MbLabelTool, no_config);
        $visit!(
            $crate::domains::tools::definitions::MbMatchFromTagsTool,
            no_config
        );
        $visit!(
            $crate::domains::tools::definitions::MbRecordingTool,
            no_config
        );
        $visit!(
            $crate::domains::tools::definitions::MbReleaseTool,
            no_config
        );
        $visit!($crate::domains::tools::definitions::MbWorkTool, no_config);
    };
}
