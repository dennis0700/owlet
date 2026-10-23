use serde_json::{Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Shape of a JSON-RPC 2.0 message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    Notification,
    Response,
    Invalid,
}

/// Classifies a single JSON-RPC message.
///
/// # Examples
///
/// ```ignore
/// assert_eq!(classify(&json!({"jsonrpc":"2.0","id":1,"method":"ping"})), Kind::Request);
/// ```
pub fn classify(msg: &Value) -> Kind {
    let Some(obj) = msg.as_object() else {
        return Kind::Invalid;
    };
    let has_id = obj.get("id").is_some_and(|v| !v.is_null());
    let has_method = obj.get("method").is_some_and(Value::is_string);
    match (has_method, has_id) {
        (true, true) => Kind::Request,
        (true, false) => Kind::Notification,
        (false, true) if obj.contains_key("result") || obj.contains_key("error") => Kind::Response,
        _ => Kind::Invalid,
    }
}

/// Returns the `method` field, if any.
pub fn method(msg: &Value) -> Option<&str> {
    msg.get("method").and_then(Value::as_str)
}

/// Builds a success response.
pub fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Builds an error response.
pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert_eq!(classify(&json!({"id": 1, "method": "x"})), Kind::Request);
        assert_eq!(classify(&json!({"method": "x"})), Kind::Notification);
        assert_eq!(classify(&json!({"id": "a", "result": {}})), Kind::Response);
        assert_eq!(classify(&json!({"id": 1, "error": {}})), Kind::Response);
        assert_eq!(classify(&json!({"id": 1})), Kind::Invalid);
        assert_eq!(classify(&json!([1])), Kind::Invalid);
    }
}
