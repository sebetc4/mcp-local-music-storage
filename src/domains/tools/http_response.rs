//! Shared helper for HTTP-transport tool handlers.
//!
//! Centralises the conversion from rmcp's [`CallToolResult`] to the JSON-RPC
//! `tools/call` response envelope (`{content, isError, structuredContent?}`),
//! so individual handlers don't need to hand-roll the same JSON shape and the
//! previous `as_object_mut().unwrap()` pattern disappears at every call site.

use rmcp::model::CallToolResult;

/// Encode a [`CallToolResult`] into the JSON-RPC envelope returned by HTTP
/// handlers. Built directly as a [`serde_json::Map`] to avoid the implicit
/// unwrap involved in `serde_json::json!({...}).as_object_mut().unwrap()`.
pub fn tool_result_to_json(result: CallToolResult) -> Result<serde_json::Value, String> {
    let content = serde_json::to_value(&result.content)
        .map_err(|e| format!("Failed to serialize tool content: {}", e))?;

    let mut response = serde_json::Map::new();
    response.insert("content".to_string(), content);
    response.insert(
        "isError".to_string(),
        serde_json::Value::Bool(result.is_error.unwrap_or(false)),
    );

    if let Some(structured) = result.structured_content {
        response.insert("structuredContent".to_string(), structured);
    }

    Ok(serde_json::Value::Object(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    #[test]
    fn encodes_error_result_with_flag_set() {
        let result = CallToolResult::error(vec![Content::text("nope".to_string())]);
        let json = tool_result_to_json(result).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.get("isError"), Some(&serde_json::Value::Bool(true)));
        assert!(obj.get("content").unwrap().is_array());
        assert!(obj.get("structuredContent").is_none());
    }

    #[test]
    fn encodes_success_with_structured_content() {
        let mut result = CallToolResult::success(vec![Content::text("ok".to_string())]);
        result.structured_content = Some(serde_json::json!({"count": 3}));
        let json = tool_result_to_json(result).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.get("isError"), Some(&serde_json::Value::Bool(false)));
        assert_eq!(
            obj.get("structuredContent"),
            Some(&serde_json::json!({"count": 3}))
        );
    }

    #[test]
    fn missing_is_error_defaults_to_false() {
        // Default-constructed CallToolResult has `is_error = None`; the
        // tool_result_to_json helper coerces that to `false`.
        let result = CallToolResult::default();
        let json = tool_result_to_json(result).unwrap();
        assert_eq!(
            json.as_object().unwrap().get("isError"),
            Some(&serde_json::Value::Bool(false))
        );
    }
}
