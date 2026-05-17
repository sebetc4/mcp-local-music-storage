# Configuration Workflow Architecture

This document describes how configuration flows through the MCP server from environment variables to tool execution.

## Overview

The configuration system uses a centralized, type-safe approach with environment variable loading at startup. All configuration is immutable after initialization and shared efficiently via `Arc<Config>`.

## Configuration Loading Flow

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Startup                      │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│                      main.rs: main()                        │
│                                                             │
│  1. Initialize logging                                      │
│  2. Call Config::from_env()                                 │
│  3. Create McpServer                                        │
│  4. Start transport layer                                   │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│          src/core/config.rs: Config::from_env()             │
│                                                             │
│  ┌────────────────────────────────────────────┐             │
│  │ 1. dotenvy::dotenv().ok()                  │             │
│  │    • Load .env file if present             │             │
│  │    • Merge with system environment         │             │
│  └────────────────────────────────────────────┘             │
│                     │                                       │
│  ┌────────────────────────────────────────────┐             │
│  │ 2. Create Config with defaults             │             │
│  │    • ServerConfig::default()               │             │
│  │    • LoggingConfig::default()              │             │
│  │    • CredentialsConfig::default()          │             │
│  └────────────────────────────────────────────┘             │
│                     │                                       │
│  ┌────────────────────────────────────────────┐             │
│  │ 3. Load from environment variables         │             │
│  │    • std::env::var("MCP_SERVER_NAME")      │             │
│  │    • std::env::var("MCP_LOG_LEVEL")        │             │
│  │    • std::env::var("MCP_ACOUSTID_API_KEY") │             │
│  │    • TransportConfig::from_env()           │             │
│  └────────────────────────────────────────────┘             │
│                     │                                       │
│  ┌────────────────────────────────────────────┐             │
│  │ 4. Apply validation & logging              │             │
│  │    • Log loaded API keys (REDACTED)        │             │
│  │    • Warn if using default keys            │             │
│  └────────────────────────────────────────────┘             │
│                     │                                       │
│                     ▼                                       │
│               Return Config                                 │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│           src/core/server.rs: McpServer::new()              │
│                                                             │
│  let config = Arc::new(config);  // Wrap for sharing        │
│                                                             │
│  McpServer {                                                │
│      config: Arc<Config>,         // Store reference        │
│      tool_router: build_tool_router(config.clone()),        │
│      resource_service: ResourceService::new(config),        │
│      prompt_service: PromptService::new(config),            │
│  }                                                          │
└─────────────────────────────────────────────────────────────┘
                            │
         ┌──────────────────┴──────────────────┐
         │                                      │
         ▼                                      ▼
┌──────────────────────┐          ┌──────────────────────┐
│   STDIO/TCP Path     │          │     HTTP Path        │
└──────────────────────┘          └──────────────────────┘
```

## Detailed Component Flows

### 1. STDIO/TCP Transport Flow

```
┌─────────────────────────────────────────────────────────────┐
│            build_tool_router(config: Arc<Config>)           │
│                                                              │
│  Creates ToolRouter<McpServer> with all tools:              │
│    • FsListDirTool::create_route()                          │
│    • MbIdentifyRecordTool::create_route(config.clone())     │
│    • ... other tools ...                                    │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│      MbIdentifyRecordTool::create_route(config)             │
│                                                              │
│  ToolRoute::new_dyn(tool_metadata, move |ctx| {             │
│      let config = config.clone();  // Capture in closure   │
│      async move {                                           │
│          let params = parse_arguments(ctx.arguments)?;     │
│          let result = spawn_blocking(move || {             │
│              Tool::execute(&params, &config) // Pass config│
│          }).await?;                                         │
│          Ok(result)                                         │
│      }                                                      │
│  })                                                         │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│         Tool::execute(params, config) [Blocking]            │
│                                                              │
│  1. Extract API key from config:                            │
│     let api_key = config.credentials.acoustid_api_key       │
│                         .as_deref()                         │
│                         .unwrap_or_default();               │
│                                                              │
│  2. Use API key in external API call                        │
│  3. Return result to client                                 │
└─────────────────────────────────────────────────────────────┘
```

### 2. HTTP Transport Flow

```
┌─────────────────────────────────────────────────────────────┐
│          McpServer::call_tool(name, arguments)              │
│                                                              │
│  let registry = ToolRegistry::new(self.config.clone());     │
│  registry.call_tool(name, arguments)                        │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│           ToolRegistry::call_tool(name, args)               │
│                                                              │
│  match name {                                               │
│      "mb_identify_record" => {                              │
│          MbIdentifyRecordTool::http_handler(               │
│              arguments,                                     │
│              self.config.clone()  // Pass config           │
│          )                                                  │
│      }                                                      │
│      _ => ...                                               │
│  }                                                          │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│   Tool::http_handler(arguments, config) [OS Thread]         │
│                                                              │
│  std::thread::spawn(move || {                               │
│      Tool::execute(&params, &config)  // Pass config       │
│  })                                                         │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│         Tool::execute(params, config) [Same as STDIO]       │
└─────────────────────────────────────────────────────────────┘
```

## Configuration Priority

Environment variables are resolved in this order (highest to lowest priority):

1. **System Environment Variables** (set via `export`, systemd, Docker, etc.)
2. **`.env` File** (in project root, loaded via `dotenvy`)
3. **Default Values** (hardcoded in `Config::default()`)

### Example Priority Resolution

```rust
// Scenario: Multiple sources for MCP_LOG_LEVEL

// 1. System environment (highest priority)
$ export MCP_LOG_LEVEL=debug

// 2. .env file (ignored if system env exists)
// File: .env
MCP_LOG_LEVEL=info

// 3. Default value (used if neither above exists)
impl Default for LoggingConfig {
    fn default() -> Self {
        Self { level: "info".to_string() }
    }
}

// Result: LOG_LEVEL = "debug" (from system environment)
```

## Security Architecture

### API Key Redaction

```rust
// Custom Debug implementation prevents key leakage in logs
impl Debug for CredentialsConfig {
    fn fmt(&self, f: &mut Formatter) -> Result {
        f.debug_struct("CredentialsConfig")
            .field("acoustid_api_key", &"[REDACTED]")
            .finish()
    }
}

// Example log output:
// Config {
//     credentials: CredentialsConfig {
//         acoustid_api_key: "[REDACTED]"
//     }
// }
```

### Immutability

```rust
// Config is Arc-wrapped and never mutated after creation
pub struct McpServer {
    config: Arc<Config>,  // Read-only reference
    // ...
}

// Cloning is cheap (just increments reference count)
let config_clone = self.config.clone();  // O(1) operation
```

## Adding New Configuration Options

To add a new environment variable:

### Step 1: Add to Config Struct

```rust
// src/core/config.rs
pub struct CredentialsConfig {
    pub acoustid_api_key: Option<String>,
    pub new_api_key: Option<String>,  // Add new field
}
```

### Step 2: Update from_env()

```rust
// src/core/config.rs
pub fn from_env() -> Self {
    // ... existing code ...

    // Load new API key
    if let Ok(key) = std::env::var("MCP_NEW_API_KEY") {
        config.credentials.new_api_key = Some(key);
        info!("New API key loaded from environment");
    }

    config
}
```

### Step 3: Update Default Implementation

```rust
impl Default for CredentialsConfig {
    fn default() -> Self {
        Self {
            // Credentials default to None — never embed real keys.
            acoustid_api_key: None,
            new_api_key: None,
        }
    }
}
```

### Step 4: Document

1. Add to [Configuration Guide](../guides/configuration.md)
2. Add to `.env.example`
3. Update this architecture doc

## Performance Characteristics

### Memory Usage

```
Config struct size: ~500 bytes
Arc overhead: 16 bytes (reference count)
Total per clone: 16 bytes (just pointer + refcount)
```

### Initialization Time

```
Typical startup sequence:
1. Load .env file: <1ms
2. Read environment vars: <1ms
3. Validate config: <1ms
4. Create Arc: <1μs
5. Pass to services: O(1)

Total config overhead: ~1-2ms
```

### Runtime Overhead

```
Config access: O(1) - just pointer dereference
API key extraction: O(1) - direct field access
Config cloning: O(1) - increment refcount

No runtime configuration overhead!
```

## Troubleshooting

### Issue: Config not loading from .env

**Check**:
```bash
# 1. File exists in project root
ls -la .env

# 2. File is readable
chmod 644 .env

# 3. No syntax errors
cat .env | grep -v '^#' | grep -v '^$'

# 4. No system env vars overriding
env | grep MCP_
```

### Issue: API key not taking effect

**Debug**:
```rust
// Add temporary debug logging
eprintln!("Config: {:?}", config);  // Should show [REDACTED]
eprintln!("API key present: {}", config.credentials.acoustid_api_key.is_some());
```

### Issue: Different config in different transports

**Cause**: Config loaded once at startup, shared across all transports

**Verify**: All transports should see identical config

## See Also

- [Configuration Guide](../guides/configuration.md) - User-facing documentation
- [Config Source Code](../../src/core/config.rs) - Implementation
- [Adding Tools Guide](../guides/adding-tools.md) - How tools access config
