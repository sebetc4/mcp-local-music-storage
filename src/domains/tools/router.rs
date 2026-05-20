//! Tool Router - builds the rmcp [`ToolRouter`] for STDIO/TCP transport.
//!
//! The list of tools is derived from the single [`crate::foreach_tool!`]
//! X-macro in `definitions/mod.rs`. Adding a tool there propagates here.

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;

use crate::core::config::Config;
use crate::domains::tools::definitions::mb::MbBlockingTool;

/// Build the tool router with every tool listed in [`crate::foreach_tool!`].
pub fn build_tool_router<S>(config: Arc<Config>) -> ToolRouter<S>
where
    S: Send + Sync + 'static,
{
    let mut router = ToolRouter::new();
    macro_rules! add_route {
        ($t:ty, with_config) => {
            router = router.with_route(<$t>::create_route(config.clone()));
        };
        ($t:ty, no_config) => {
            router = router.with_route(<$t>::create_route());
        };
    }
    crate::foreach_tool!(add_route);
    router
}

#[cfg(test)]
mod tests {
    use super::super::registry::ToolRegistry;
    use super::*;

    struct TestServer {}

    fn test_config() -> Arc<Config> {
        Arc::new(Config::default())
    }

    #[test]
    fn test_build_router() {
        let router: ToolRouter<TestServer> = build_tool_router(test_config());
        let tools = router.list_all();
        assert_eq!(tools.len(), 17);

        let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(names.contains(&"apply_naming_scheme"));
        assert!(names.contains(&"embed_cover"));
        assert!(names.contains(&"fs_delete"));
        assert!(names.contains(&"fs_list_dir"));
        assert!(names.contains(&"fs_mkdir"));
        assert!(names.contains(&"fs_move"));
        assert!(names.contains(&"fs_scan_audio"));
        assert!(names.contains(&"mb_artist_search"));
        assert!(names.contains(&"mb_cover_download"));
        assert!(names.contains(&"mb_release_search"));
        assert!(names.contains(&"mb_recording_search"));
        assert!(names.contains(&"mb_label_search"));
        assert!(names.contains(&"mb_work_search"));
        assert!(names.contains(&"mb_identify_record"));
    }

    /// Consistency safety-net. Even though both lists are derived from the
    /// same `foreach_tool!`, this test ensures any future divergence (e.g.
    /// someone bypasses the macro) is caught immediately.
    #[test]
    fn test_registry_matches_router() {
        let config = test_config();
        let registry = ToolRegistry::new(config.clone());
        let registry_names = registry.tool_names();

        let router: ToolRouter<TestServer> = build_tool_router(config);
        let router_tools = router.list_all();
        let router_names: Vec<_> = router_tools.iter().map(|t| t.name.as_ref()).collect();

        assert_eq!(registry_names.len(), router_names.len());
        for name in registry_names {
            assert!(router_names.contains(&name));
        }
    }
}
