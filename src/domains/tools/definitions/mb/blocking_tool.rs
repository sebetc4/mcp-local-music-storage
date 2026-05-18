//! Shared trait for MusicBrainz tools backed by the blocking `musicbrainz_rs` crate.
//!
//! The five MB search tools (`work`, `artist`, `label`, `recording`, `release`)
//! share the exact same skeleton for `to_tool` / `create_route` / `http_handler`:
//! deserialise params, dispatch the blocking call on the right thread, encode
//! the result. Each used to hand-roll those ~70 lines.
//!
//! This trait provides default implementations for all three; concrete tools
//! only need to declare their `Params` type, the public `NAME`/`DESCRIPTION`
//! constants, and the body of `execute`.
//!
//! Tools that need extra runtime context (`cover_download` and
//! `identify_record` need `Arc<Config>`) intentionally stay outside the trait
//! — the abstraction would degrade otherwise.

use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Tool},
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use futures::FutureExt;

/// Common shape for a blocking MusicBrainz tool. Implementations only need to
/// fill in [`Self::Params`], the `NAME`/`DESCRIPTION` constants, and
/// [`Self::execute`]; the transport scaffolding is provided for free.
pub trait MbBlockingTool: Send + Sync + 'static {
    /// Deserializable input parameters.
    type Params: DeserializeOwned + JsonSchema + Send + 'static;

    /// Public MCP tool name (e.g. `"mb_work_search"`).
    const NAME: &'static str;

    /// Description shown to MCP clients.
    const DESCRIPTION: &'static str;

    /// Execute the blocking work — always dispatched on a thread that's safe
    /// to block (via `tokio::task::spawn_blocking` for STDIO/TCP, via a fresh
    /// OS thread for the sync HTTP-dispatch path).
    fn execute(params: &Self::Params) -> CallToolResult;

    /// Tool metadata. Derived from [`Self::Params`]'s `JsonSchema`.
    fn to_tool() -> Tool {
        Tool {
            name: Self::NAME.into(),
            description: Some(Self::DESCRIPTION.into()),
            input_schema: schema_for_type::<Self::Params>(),
            annotations: None,
            output_schema: None,
            icons: None,
            meta: None,
            title: None,
        }
    }

    /// STDIO/TCP route. Deserialises the incoming arguments into
    /// [`Self::Params`] and dispatches `execute` on tokio's blocking pool.
    fn create_route<S>() -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            async move {
                let params: Self::Params = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

                let result = tokio::task::spawn_blocking(move || Self::execute(&params))
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("Task execution failed: {}", e), None)
                    })?;

                Ok(result)
            }
            .boxed()
        })
    }

    /// HTTP dispatch handler. The registry's HTTP path is synchronous, so the
    /// blocking call runs on a fresh OS thread — no tokio dependency in this
    /// codepath.
    #[cfg(feature = "http")]
    fn http_handler(arguments: serde_json::Value) -> Result<serde_json::Value, String> {
        let params: Self::Params = serde_json::from_value(arguments)
            .map_err(|e| format!("Invalid parameters for {}: {}", Self::NAME, e))?;

        let handle = std::thread::spawn(move || Self::execute(&params));
        let result = handle
            .join()
            .map_err(|_| format!("Thread panicked during {}", Self::NAME))?;

        crate::domains::tools::http_response::tool_result_to_json(result)
    }
}
