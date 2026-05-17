//! HTTP transport implementation.
//!
//! HTTP server with JSON-RPC over POST requests.
//! This allows standard HTTP clients (curl, browsers, etc.) to communicate with the MCP server.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, instrument, warn};

use super::{TransportConfig, TransportError, TransportResult, config::HttpConfig};
use crate::core::McpServer;

/// Resolved CORS policy for a given [`HttpConfig`]. Computed up-front so the
/// "no Any on a public bind" rule can be exercised in unit tests without
/// spinning up a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CorsDecision {
    /// CORS layer is omitted entirely (`enable_cors = false`).
    Disabled,
    /// No explicit allow-list, but host is loopback — `Any` is permitted with
    /// a startup warning.
    AllowAnyLoopback,
    /// Explicit allow-list of origins.
    Allowlist(Vec<String>),
    /// Startup must be refused — non-loopback bind without explicit origins.
    Reject(String),
}

/// True if `host` denotes a loopback target: any IP that parses as a loopback
/// address, or the literal hostname `localhost`.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Decide what CORS behavior to apply based on the config + bind host.
pub(crate) fn decide_cors_policy(config: &HttpConfig) -> CorsDecision {
    if !config.enable_cors {
        return CorsDecision::Disabled;
    }
    if !config.cors_allow_origins.is_empty() {
        return CorsDecision::Allowlist(config.cors_allow_origins.clone());
    }
    if is_loopback_host(&config.host) {
        return CorsDecision::AllowAnyLoopback;
    }
    CorsDecision::Reject(format!(
        "CORS is enabled with a wildcard origin on a non-loopback bind ({}). \
         Set MCP_HTTP_CORS_ORIGINS to an explicit comma-separated list of \
         allowed origins, or set MCP_HTTP_CORS=false to disable CORS, or bind \
         to a loopback host.",
        config.host
    ))
}

/// HTTP transport handler.
pub struct HttpTransport {
    config: HttpConfig,
}

/// JSON-RPC request structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC response structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    /// Create a success response.
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Method not found error.
    pub fn method_not_found(id: Option<serde_json::Value>) -> Self {
        Self::error(id, -32601, "Method not found")
    }

    /// Invalid request error.
    pub fn invalid_request(id: Option<serde_json::Value>) -> Self {
        Self::error(id, -32600, "Invalid Request")
    }

    /// Invalid params error.
    pub fn invalid_params(id: Option<serde_json::Value>, msg: impl Into<String>) -> Self {
        Self::error(id, -32602, msg)
    }

    /// Internal error.
    pub fn internal_error(id: Option<serde_json::Value>, msg: impl Into<String>) -> Self {
        Self::error(id, -32603, msg)
    }
}

/// Application state shared across HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    /// The MCP server instance.
    server: McpServer,
    /// Session state for maintaining conversation context.
    session: Arc<RwLock<Option<SessionState>>>,
}

/// Session state for a client.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct SessionState {
    initialized: bool,
    protocol_version: String,
}

impl HttpTransport {
    /// Create a new HTTP transport with the given config.
    pub fn new(config: HttpConfig) -> Self {
        Self { config }
    }

    /// Create from TransportConfig (extracts HTTP config).
    pub fn from_transport_config(config: &TransportConfig) -> Option<Self> {
        match config {
            TransportConfig::Http(http_config) => Some(Self::new(http_config.clone())),
            _ => None,
        }
    }

    /// Get the bind address.
    pub fn address(&self) -> String {
        format!("{}:{}", self.config.host, self.config.port)
    }

    /// Run the HTTP transport.
    pub async fn run(self, server: McpServer) -> TransportResult<()> {
        let addr = self.address();

        let state = AppState {
            server,
            session: Arc::new(RwLock::new(None)),
        };

        // Build router
        let mut app = Router::new()
            .route(&self.config.rpc_path, post(handle_rpc))
            .route("/health", get(health_check))
            .route("/", get(root_handler))
            .with_state(state);

        // Resolve and apply CORS policy before binding. Misconfigurations refuse
        // startup so an operator notices immediately instead of silently
        // exposing `Any` on a public interface.
        let cors_status: &'static str;
        match decide_cors_policy(&self.config) {
            CorsDecision::Disabled => {
                cors_status = "disabled";
            }
            CorsDecision::AllowAnyLoopback => {
                warn!(
                    "CORS Any/Any/Any in effect on loopback host {} — fine for \
                     local dev. Set MCP_HTTP_CORS_ORIGINS before exposing this \
                     binary on a non-loopback interface.",
                    self.config.host
                );
                let cors = CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any);
                app = app.layer(cors);
                cors_status = "any (loopback only)";
            }
            CorsDecision::Allowlist(origins) => {
                let parsed: Vec<HeaderValue> = origins
                    .iter()
                    .map(|o| {
                        HeaderValue::from_str(o).map_err(|e| {
                            TransportError::init(format!(
                                "Invalid CORS origin {:?}: {}",
                                o, e
                            ))
                        })
                    })
                    .collect::<Result<_, _>>()?;
                let cors = CorsLayer::new()
                    .allow_origin(parsed)
                    .allow_methods(Any)
                    .allow_headers(Any);
                app = app.layer(cors);
                info!("CORS allow-list: {}", origins.join(", "));
                cors_status = "allowlist";
            }
            CorsDecision::Reject(msg) => {
                return Err(TransportError::init(msg));
            }
        }

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| TransportError::bind(&addr, e))?;

        info!(
            "Ready - listening on {} (JSON-RPC over HTTP, CORS {})",
            addr, cors_status
        );
        info!("  → JSON-RPC: POST {}", self.config.rpc_path);
        info!("  → Health:   GET /health");

        axum::serve(listener, app)
            .await
            .map_err(|e| TransportError::http(e.to_string()))?;

        Ok(())
    }
}

/// Root handler - provides API info.
async fn root_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "MCP Server",
        "version": env!("CARGO_PKG_VERSION"),
        "transport": "HTTP",
        "endpoints": {
            "rpc": "/mcp",
            "health": "/health"
        },
        "protocol": "JSON-RPC 2.0",
        "documentation": "Send POST requests to /mcp with JSON-RPC messages"
    }))
}

/// Health check endpoint.
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "timestamp": chrono::Utc::now().to_rfc3339()
    }))
}

/// Handle JSON-RPC requests.
#[instrument(skip_all, fields(method))]
async fn handle_rpc(
    State(state): State<AppState>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("method", &request.method);
    info!("Received JSON-RPC request: {}", request.method);

    let response = process_request(&state, request).await;

    (StatusCode::OK, Json(response))
}

/// Process a JSON-RPC request and return the response.
async fn process_request(state: &AppState, request: JsonRpcRequest) -> JsonRpcResponse {
    // Validate JSON-RPC version
    if request.jsonrpc != "2.0" {
        return JsonRpcResponse::invalid_request(request.id);
    }

    match request.method.as_str() {
        // Initialize the MCP session
        "initialize" => handle_initialize(state, request).await,

        // List available tools
        "tools/list" => handle_tools_list(state, request).await,

        // Call a tool
        "tools/call" => handle_tools_call(state, request).await,

        // Notifications (no response needed for stateless HTTP)
        method if method.starts_with("notifications/") => {
            handle_notification(state, &request).await;
            // Return empty success for notifications
            JsonRpcResponse::success(request.id, serde_json::json!(null))
        }

        // Unknown method
        _ => {
            warn!("Unknown method: {}", request.method);
            JsonRpcResponse::method_not_found(request.id)
        }
    }
}

/// Handle initialize request.
async fn handle_initialize(state: &AppState, request: JsonRpcRequest) -> JsonRpcResponse {
    info!("Processing initialize request");

    // Extract client info from params
    let _params = request.params.clone().unwrap_or(serde_json::json!({}));

    // Store session state
    let mut session = state.session.write().await;
    *session = Some(SessionState {
        initialized: true,
        protocol_version: "2024-11-05".to_string(),
    });

    // Return server capabilities
    let result = serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": state.server.name(),
            "version": state.server.version()
        },
        "instructions": "MCP server for music library automation: filesystem, audio metadata, and MusicBrainz tooling."
    });

    JsonRpcResponse::success(request.id, result)
}

/// Handle tools/list request.
async fn handle_tools_list(state: &AppState, request: JsonRpcRequest) -> JsonRpcResponse {
    info!("Processing tools/list request");

    let tools = state.server.list_tools();
    let result = serde_json::json!({
        "tools": tools
    });

    JsonRpcResponse::success(request.id, result)
}

/// Handle tools/call request.
async fn handle_tools_call(state: &AppState, request: JsonRpcRequest) -> JsonRpcResponse {
    info!("Processing tools/call request");

    let params = match request.params {
        Some(p) => p,
        None => return JsonRpcResponse::invalid_params(request.id.clone(), "Missing params"),
    };

    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return JsonRpcResponse::invalid_params(request.id.clone(), "Missing tool name"),
    };

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    match state.server.call_tool(&name, arguments).await {
        Ok(result) => JsonRpcResponse::success(request.id, result),
        Err(e) => JsonRpcResponse::invalid_params(request.id, e.to_string()),
    }
}

/// Handle notifications (no response needed).
async fn handle_notification(state: &AppState, request: &JsonRpcRequest) {
    match request.method.as_str() {
        "notifications/initialized" => {
            info!("Client sent initialized notification");
            let mut session = state.session.write().await;
            if let Some(ref mut s) = *session {
                s.initialized = true;
            }
        }
        _ => {
            info!("Received notification: {}", request.method);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(host: &str, enable_cors: bool, origins: &[&str]) -> HttpConfig {
        HttpConfig {
            port: 8080,
            host: host.to_string(),
            rpc_path: "/mcp".to_string(),
            enable_cors,
            cors_allow_origins: origins.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn loopback_recognized() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LocalHost"));
        // Any IP in 127.0.0.0/8 is loopback.
        assert!(is_loopback_host("127.5.6.7"));
    }

    #[test]
    fn non_loopback_rejected() {
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("10.0.0.1"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn cors_disabled_when_flag_off() {
        let c = config("0.0.0.0", false, &[]);
        assert_eq!(decide_cors_policy(&c), CorsDecision::Disabled);
    }

    #[test]
    fn cors_allowlist_used_when_provided() {
        let c = config("0.0.0.0", true, &["https://app.example.com"]);
        assert_eq!(
            decide_cors_policy(&c),
            CorsDecision::Allowlist(vec!["https://app.example.com".to_string()])
        );
    }

    #[test]
    fn cors_any_allowed_on_loopback() {
        let c = config("127.0.0.1", true, &[]);
        assert_eq!(decide_cors_policy(&c), CorsDecision::AllowAnyLoopback);
    }

    #[test]
    fn cors_rejected_on_public_bind_without_allowlist() {
        let c = config("0.0.0.0", true, &[]);
        match decide_cors_policy(&c) {
            CorsDecision::Reject(msg) => {
                assert!(msg.contains("MCP_HTTP_CORS_ORIGINS"));
                assert!(msg.contains("0.0.0.0"));
            }
            other => panic!("expected Reject, got {:?}", other),
        }
    }

    #[test]
    fn cors_allowlist_wins_over_loopback() {
        let c = config("127.0.0.1", true, &["https://app.example.com"]);
        assert!(matches!(decide_cors_policy(&c), CorsDecision::Allowlist(_)));
    }
}
