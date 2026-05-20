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
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

use futures::FutureExt;

use crate::core::mb_request::{TTL_ENTITY, cached_or_fetch_blocking};

/// Common shape for a blocking MusicBrainz tool. Implementations only need to
/// fill in [`Self::Params`], the `NAME`/`DESCRIPTION` constants, and
/// [`Self::execute`]; the transport scaffolding is provided for free.
///
/// Cache + throttle are applied around every call via the default
/// [`Self::execute_cached`] implementation, which derives the cache key from
/// `(NAME, serde_json::to_string(params))`. Tools whose data is more static
/// than the 24h default (e.g. record labels and works) override
/// [`Self::TTL`] to `TTL_STATIC`.
pub trait MbBlockingTool: Send + Sync + 'static {
    /// Deserializable input parameters. Must also be `Serialize` so the
    /// cache layer can derive a stable, opaque key.
    type Params: Serialize + DeserializeOwned + JsonSchema + Send + 'static;

    /// Public MCP tool name (e.g. `"mb_work_search"`).
    const NAME: &'static str;

    /// Description shown to MCP clients.
    const DESCRIPTION: &'static str;

    /// Cache TTL for successful responses. Defaults to 24h — override to
    /// [`crate::core::mb_request::TTL_STATIC`] for slow-moving entities.
    const TTL: Duration = TTL_ENTITY;

    /// Execute the blocking work — always dispatched on a thread that's safe
    /// to block (via `tokio::task::spawn_blocking` for STDIO/TCP, via a fresh
    /// OS thread for the sync HTTP-dispatch path).
    fn execute(params: &Self::Params) -> CallToolResult;

    /// Run `execute` through the shared cache + throttle. Concrete impls
    /// rarely need to override this — the default ties [`Self::NAME`] and a
    /// JSON serialisation of `params` into a cache key, then delegates.
    fn execute_cached(params: &Self::Params) -> CallToolResult {
        let cache_key = match serde_json::to_string(params) {
            Ok(json) => format!("{}:{}", Self::NAME, json),
            // If serialising params somehow fails we still want the tool
            // to work — just skip the cache and pay the network round-trip.
            Err(e) => {
                tracing::warn!(
                    "Failed to serialise params for cache key on {}: {}; running uncached",
                    Self::NAME,
                    e
                );
                return Self::execute(params);
            }
        };
        cached_or_fetch_blocking(&cache_key, Self::TTL, || Self::execute(params))
    }

    /// Tool metadata. Derived from [`Self::Params`]'s `JsonSchema`.
    fn to_tool() -> Tool {
        Tool::new(Self::NAME, Self::DESCRIPTION, schema_for_type::<Self::Params>())
    }

    /// STDIO/TCP route. Deserialises the incoming arguments into
    /// [`Self::Params`] and dispatches `execute_cached` on tokio's blocking
    /// pool. Cache hits never make it to the network; misses go through the
    /// throttle before invoking `execute`.
    fn create_route<S>() -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            async move {
                let params: Self::Params = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

                let result = tokio::task::spawn_blocking(move || Self::execute_cached(&params))
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

        let handle = std::thread::spawn(move || Self::execute_cached(&params));
        let result = handle
            .join()
            .map_err(|_| format!("Thread panicked during {}", Self::NAME))?;

        crate::domains::tools::http_response::tool_result_to_json(result)
    }
}
