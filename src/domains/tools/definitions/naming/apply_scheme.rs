//! Apply-naming-scheme tool definition.
//!
//! Pure function (no I/O): takes a template string and a metadata map and
//! returns a sanitised relative path. The point is to centralise:
//!
//! 1. **Sanitisation**: every substituted value is stripped of OS-unsafe
//!    characters (`/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`, `|`, control bytes)
//!    so that a tag like `title = "AC/DC"` does not silently introduce a
//!    directory boundary.
//! 2. **Templating consistency**: the agent stops re-implementing the same
//!    "format the track number with leading zeros and join the album folder"
//!    logic in every conversation.
//! 3. **Safety**: the resolved path must be relative (no leading `/`) and
//!    must not contain `..` components.
//!
//! Template DSL:
//!
//! * `{name}` — required field. Missing or empty → error.
//! * `{name|fallback}` — fall back to another metadata field if `name` is
//!   absent or empty. The fallback is also a field name (not a literal).
//! * `{name:0Nd}` — zero-padded integer of width `N` (e.g. `:02d` →
//!   `01`, `:03d` → `001`). The value must be numeric.
//!
//! Combined: `{name|fallback:0Nd}` (fallback first, then format).

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the apply-naming-scheme tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ApplyNamingSchemeParams {
    /// Template string. Placeholders use `{name}`, `{name|fallback}`,
    /// `{name:0Nd}`, or `{name|fallback:0Nd}`.
    pub template: String,

    /// Metadata key-value map. Values can be strings or numbers.
    pub metadata: Map<String, Value>,

    /// When `true` (default), every substituted value is sanitised: OS-unsafe
    /// characters and control bytes are replaced with `-`, surrounding
    /// whitespace and trailing dots are trimmed.
    #[serde(default = "default_sanitise")]
    pub sanitise: bool,
}

fn default_sanitise() -> bool {
    true
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of an apply-naming-scheme call.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApplyNamingSchemeResult {
    /// The rendered, sanitised, validated relative path.
    pub path: String,
    /// `true` when sanitisation was applied (i.e. the `sanitise` flag was on).
    pub sanitised: bool,
}

// ============================================================================
// Tool Definition
// ============================================================================

pub struct ApplyNamingSchemeTool;

impl ApplyNamingSchemeTool {
    pub const NAME: &'static str = "apply_naming_scheme";

    pub const DESCRIPTION: &'static str = "Render a relative path from a template and a metadata map. \
         Placeholders: {name}, {name|fallback}, {name:0Nd} (zero-padded integer), or combined. \
         Each substituted value is sanitised (OS-unsafe characters replaced with '-') by default. \
         Pure function: no filesystem I/O. Refuses absolute paths and '..' components.";

    /// Execute the tool. The `_config` parameter is accepted for signature
    /// consistency with every other `with_config` tool but unused: this tool
    /// has no filesystem side effects to gate.
    #[instrument(skip_all)]
    pub fn execute(params: &ApplyNamingSchemeParams, _config: &Config) -> CallToolResult {
        info!("apply_naming_scheme called");

        let segments = match parse_template(&params.template) {
            Ok(s) => s,
            Err(e) => {
                warn!("Template parse failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Template parse error: {}",
                    e
                ))]);
            }
        };

        let mut rendered = String::new();
        for seg in &segments {
            match render_segment(seg, &params.metadata, params.sanitise) {
                Ok(s) => rendered.push_str(&s),
                Err(e) => {
                    warn!("Substitution failed: {}", e);
                    return CallToolResult::error(vec![Content::text(format!(
                        "Substitution error: {}",
                        e
                    ))]);
                }
            }
        }

        if let Err(e) = validate_relative_path(&rendered) {
            warn!("Resolved path rejected: {}", e);
            return CallToolResult::error(vec![Content::text(format!(
                "Resolved path rejected: {}",
                e
            ))]);
        }

        let result = ApplyNamingSchemeResult {
            path: rendered.clone(),
            sanitised: params.sanitise,
        };
        let summary = format!("Rendered path: {}", rendered);

        info!("{}", summary);

        crate::domains::tools::result::structured_ok(summary, &result)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: ApplyNamingSchemeParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;

        let result = Self::execute(&params, &config);

        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<ApplyNamingSchemeParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ApplyNamingSchemeResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: ApplyNamingSchemeParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

// ============================================================================
// Template parsing & evaluation
// ============================================================================

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Placeholder {
        name: String,
        fallback: Option<String>,
        format: Option<FormatSpec>,
    },
}

/// A subset of printf-style format specs. Only zero-padded integers are
/// supported — that's what naming schemes actually need (`disc:02d`,
/// `track:02d`). Adding more variants here is cheap if a future need arises.
#[derive(Debug, PartialEq, Eq)]
enum FormatSpec {
    /// `:Nd` or `:0Nd` — width N, zero-padded.
    ZeroPadInt(usize),
}

fn parse_template(template: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                let mut body = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    if c == '{' {
                        return Err(format!(
                            "Nested '{{' inside placeholder '{{{}}}' — placeholders cannot nest",
                            body
                        ));
                    }
                    body.push(c);
                }
                if !closed {
                    return Err(format!("Unclosed placeholder starting at '{{{}'", body));
                }
                segments.push(parse_placeholder(&body)?);
            }
            '}' => {
                return Err(
                    "Unexpected '}' — no matching '{' before this position".to_string()
                );
            }
            _ => literal.push(c),
        }
    }
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    Ok(segments)
}

fn parse_placeholder(body: &str) -> Result<Segment, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("Empty placeholder '{}' — expected a field name".to_string());
    }

    // Split format spec first: at the first ':' inside the body. Anything
    // before it is the name (+ optional fallback), anything after is the
    // format spec.
    let (head, format) = match body.find(':') {
        Some(idx) => (&body[..idx], Some(&body[idx + 1..])),
        None => (body, None),
    };

    // Inside the head, '|' separates name from fallback.
    let (name, fallback) = match head.find('|') {
        Some(idx) => (&head[..idx], Some(&head[idx + 1..])),
        None => (head, None),
    };

    let name = name.trim();
    if !is_valid_field_name(name) {
        return Err(format!(
            "Invalid field name '{}' — letters, digits and '_' only",
            name
        ));
    }
    let fallback = match fallback {
        Some(fb) => {
            let fb = fb.trim();
            if !is_valid_field_name(fb) {
                return Err(format!(
                    "Invalid fallback field name '{}' — letters, digits and '_' only",
                    fb
                ));
            }
            Some(fb.to_string())
        }
        None => None,
    };
    let format = match format {
        Some(fmt) => Some(parse_format_spec(fmt.trim(), name)?),
        None => None,
    };

    Ok(Segment::Placeholder {
        name: name.to_string(),
        fallback,
        format,
    })
}

fn is_valid_field_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_format_spec(spec: &str, field: &str) -> Result<FormatSpec, String> {
    // Currently only zero-padded integer formats. Accepted shapes:
    //   "Nd"   — width N, zero-padded (since filenames don't want spaces)
    //   "0Nd"  — explicit zero-pad notation (printf-style)
    //
    // The whole spec must match `0?\d+d` — anything else is "unsupported"
    // (clearer error than "invalid width" when the spec is `:invalid`).
    let trimmed = spec;
    if !trimmed.ends_with('d') {
        return Err(format!(
            "Unsupported format ':{}' for field '{}' — only zero-padded integers (':Nd' / ':0Nd') are supported",
            spec, field
        ));
    }
    let digits_part = &trimmed[..trimmed.len() - 1];
    let digits_part = digits_part.strip_prefix('0').unwrap_or(digits_part);
    if digits_part.is_empty() || !digits_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "Unsupported format ':{}' for field '{}' — only zero-padded integers (':Nd' / ':0Nd') are supported",
            spec, field
        ));
    }
    let width: usize = digits_part.parse().map_err(|_| {
        format!(
            "Invalid width in format ':{}' for field '{}'",
            spec, field
        )
    })?;
    if width == 0 {
        return Err(format!(
            "Width must be >= 1 in format ':{}' for field '{}'",
            spec, field
        ));
    }
    Ok(FormatSpec::ZeroPadInt(width))
}

fn render_segment(
    segment: &Segment,
    metadata: &Map<String, Value>,
    sanitise: bool,
) -> Result<String, String> {
    match segment {
        Segment::Literal(s) => Ok(s.clone()),
        Segment::Placeholder {
            name,
            fallback,
            format,
        } => {
            let value = lookup_non_empty(metadata, name).or_else(|| {
                fallback
                    .as_ref()
                    .and_then(|fb| lookup_non_empty(metadata, fb))
            });

            let value = match value {
                Some(v) => v,
                None => {
                    let resolution = match fallback {
                        Some(fb) => format!("'{}' (or '|{}')", name, fb),
                        None => format!("'{}'", name),
                    };
                    return Err(format!(
                        "Missing required metadata field {} — not present or empty",
                        resolution
                    ));
                }
            };

            let rendered = match format {
                Some(FormatSpec::ZeroPadInt(width)) => {
                    let n = value_as_i64(value).ok_or_else(|| {
                        format!(
                            "Field '{}' has format ':0{}d' but its value is not an integer: {}",
                            name,
                            width,
                            value
                        )
                    })?;
                    if n < 0 {
                        // Negative numbers don't fit the typical disc/track use case
                        // and would break the zero-padding contract.
                        return Err(format!(
                            "Field '{}' has format ':0{}d' but its value is negative ({})",
                            name, width, n
                        ));
                    }
                    format!("{:0>width$}", n, width = *width)
                }
                None => value_to_string(value),
            };

            if sanitise {
                Ok(sanitise_component(&rendered))
            } else {
                Ok(rendered)
            }
        }
    }
}

fn lookup_non_empty<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.get(key).and_then(|v| {
        if is_empty_value(v) {
            None
        } else {
            Some(v)
        }
    })
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        // Compound types stringify via JSON serialisation; sanitiser will
        // then strip the braces/quotes so the result is at least a valid
        // path component (though probably not what the agent wanted).
        other => other.to_string(),
    }
}

fn value_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Replace OS-unsafe characters and control bytes in a single path component
/// with `-`, then trim surrounding whitespace and trailing dots (Windows
/// rejects names ending in dots or spaces).
fn sanitise_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('-'),
            c if c.is_control() => out.push('-'),
            c => out.push(c),
        }
    }
    out.trim().trim_end_matches('.').to_string()
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("Resolved path is empty".to_string());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("Resolved path '{}' is absolute", path));
    }
    for component in std::path::Path::new(path).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(format!("Resolved path '{}' contains a '..' component", path));
        }
    }
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Config {
        Config::default()
    }

    fn run(template: &str, metadata: Value, sanitise: bool) -> Result<String, String> {
        let params = ApplyNamingSchemeParams {
            template: template.to_string(),
            metadata: metadata.as_object().expect("metadata must be an object").clone(),
            sanitise,
        };
        let result = ApplyNamingSchemeTool::execute(&params, &cfg());
        if result.is_error.unwrap_or(false) {
            let msg = result
                .content
                .iter()
                .filter_map(|c| match &c.raw {
                    rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(msg);
        }
        let structured = result.structured_content.expect("structured content");
        Ok(structured["path"].as_str().unwrap().to_string())
    }

    #[test]
    fn happy_path_roadmap_example() {
        let out = run(
            "{album_artist|artist}/{year} {album}/{disc:02d}-{track:02d} {title}.{ext}",
            json!({
                "artist": "AC/DC",
                "album": "Back in Black",
                "year": 1980,
                "disc": 1,
                "track": 1,
                "title": "Hells Bells",
                "ext": "mp3"
            }),
            true,
        )
        .unwrap();
        assert_eq!(out, "AC-DC/1980 Back in Black/01-01 Hells Bells.mp3");
    }

    #[test]
    fn fallback_uses_other_field_when_primary_missing() {
        // album_artist is absent — fallback to artist.
        let out = run(
            "{album_artist|artist}/{album}",
            json!({"artist": "Radiohead", "album": "OK Computer"}),
            true,
        )
        .unwrap();
        assert_eq!(out, "Radiohead/OK Computer");
    }

    #[test]
    fn fallback_uses_other_field_when_primary_empty() {
        let out = run(
            "{album_artist|artist}/{album}",
            json!({"album_artist": "", "artist": "Pixies", "album": "Doolittle"}),
            true,
        )
        .unwrap();
        assert_eq!(out, "Pixies/Doolittle");
    }

    #[test]
    fn missing_required_field_errors() {
        let err = run(
            "{artist}/{album}",
            json!({"album": "Without Artist"}),
            true,
        )
        .unwrap_err();
        assert!(err.contains("'artist'"), "got error: {}", err);
    }

    #[test]
    fn injection_sanitisation_replaces_unsafe_chars() {
        // Every OS-unsafe character (/, \, :, *, ?, ", <, >, |) becomes '-'.
        // Trailing run "?\"<>|" → five dashes.
        let out = run(
            "{title}",
            json!({"title": "weird/title\\with:every*char?\"<>|"}),
            true,
        )
        .unwrap();
        assert_eq!(out, "weird-title-with-every-char-----");
    }

    #[test]
    fn sanitise_false_lets_unsafe_chars_through() {
        // The literal '/' in the title creates a directory boundary because
        // the agent disabled sanitisation. Use case: the agent has already
        // sanitised manually and doesn't want a second pass.
        let out = run("{title}", json!({"title": "AC/DC"}), false).unwrap();
        assert_eq!(out, "AC/DC");
    }

    #[test]
    fn format_spec_zero_pads_integer() {
        let out = run(
            "{track:02d}",
            json!({"track": 7}),
            true,
        )
        .unwrap();
        assert_eq!(out, "07");

        let out = run("{track:04d}", json!({"track": 7}), true).unwrap();
        assert_eq!(out, "0007");
    }

    #[test]
    fn format_spec_accepts_numeric_string() {
        // serde_json can produce String for an integer field if the agent
        // sent it that way — be tolerant.
        let out = run("{track:03d}", json!({"track": "12"}), true).unwrap();
        assert_eq!(out, "012");
    }

    #[test]
    fn format_spec_rejects_non_integer() {
        let err = run("{title:02d}", json!({"title": "Hells Bells"}), true).unwrap_err();
        assert!(err.contains("not an integer"), "got error: {}", err);
    }

    #[test]
    fn rejects_absolute_path_result() {
        // Template starting with '/' would resolve to an absolute path.
        let err = run("/{album}", json!({"album": "x"}), true).unwrap_err();
        assert!(err.contains("absolute"), "got error: {}", err);
    }

    #[test]
    fn rejects_dotdot_component_in_template() {
        let err = run("../{album}", json!({"album": "x"}), true).unwrap_err();
        assert!(err.contains(".."), "got error: {}", err);
    }

    #[test]
    fn rejects_unclosed_placeholder() {
        let err = run("{artist", json!({"artist": "x"}), true).unwrap_err();
        assert!(err.contains("Unclosed"), "got error: {}", err);
    }

    #[test]
    fn rejects_invalid_format_spec() {
        let err = run("{track:invalid}", json!({"track": 1}), true).unwrap_err();
        assert!(err.contains("Unsupported format"), "got error: {}", err);
    }

    #[test]
    fn rejects_empty_placeholder() {
        let err = run("{}/{album}", json!({"album": "x"}), true).unwrap_err();
        assert!(err.contains("Empty placeholder"), "got error: {}", err);
    }

    #[test]
    fn template_separators_survive_sanitisation() {
        // The literal '/' separators in the template stay as path
        // separators; only substituted values get their '/' chars replaced.
        let out = run(
            "{a}/{b}/{c}",
            json!({"a": "one/x", "b": "two", "c": "three"}),
            true,
        )
        .unwrap();
        assert_eq!(out, "one-x/two/three");
    }

    #[test]
    fn control_characters_are_replaced() {
        let out = run(
            "{title}",
            json!({"title": "line1\nline2\tend"}),
            true,
        )
        .unwrap();
        assert_eq!(out, "line1-line2-end");
    }

    #[test]
    fn trailing_dots_and_whitespace_are_trimmed() {
        let out = run(
            "{album}",
            json!({"album": "  Album Name...  "}),
            true,
        )
        .unwrap();
        assert_eq!(out, "Album Name");
    }

    #[test]
    fn combined_fallback_and_format_spec() {
        // disc not present → fall back to 'disc_alt', then zero-pad.
        let out = run(
            "{disc|disc_alt:02d}-{track:02d}",
            json!({"disc_alt": 3, "track": 5}),
            true,
        )
        .unwrap();
        assert_eq!(out, "03-05");
    }
}
