use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::AppState;
use crate::aggregator::SearchRequest;
use crate::query::Query;
use crate::version::VERSION;

const TOOL: &str = "web_search";

pub async fn handler(State(state): State<AppState>, Json(message): Json<Value>) -> Response {
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return rpc_error(id, -32600, "Invalid Request");
    };
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response();
    };

    let result = match method {
        "initialize" => json!({
            "protocolVersion": message["params"]["protocolVersion"]
                .as_str()
                .unwrap_or("2024-11-05"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "rustxng", "version": VERSION },
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": [tool_definition()] }),
        "tools/call" => call_tool(&state, &message["params"]).await,
        _ => return rpc_error(Some(id), -32601, "Method not found"),
    };
    let mut response =
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn tool_definition() -> Value {
    json!({
        "name": TOOL,
        "description": "Search the web with the local RustXNG metasearch engine. Returns source URLs and snippets.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 20, "default": 5 },
                "semantic": { "type": "boolean", "default": false, "description": "Re-rank with in-process EmbeddingGemma 2" }
            },
            "required": ["query"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

async fn call_tool(state: &AppState, params: &Value) -> Value {
    if params.get("name").and_then(Value::as_str) != Some(TOOL) {
        return tool_error("Unknown tool");
    }
    let arguments = &params["arguments"];
    let Some(raw_query) = arguments.get("query").and_then(Value::as_str) else {
        return tool_error("query must be a non-empty string");
    };
    if raw_query.trim().is_empty() {
        return tool_error("query must be a non-empty string");
    }
    let max_results = arguments
        .get("max_results")
        .and_then(Value::as_u64)
        .map(|limit| limit.clamp(1, 20) as usize)
        .unwrap_or(5);
    let query = Query::parse(raw_query, &state.searcher.registry, &[]);
    let mut request = SearchRequest::new(query);
    request.max_results = Some(max_results);
    request.semantic = arguments
        .get("semantic")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    request.strategy = state.settings().search.strategy;

    let response = state.searcher.search(request).await;
    if let Some(error) = response.error {
        return tool_error(&error);
    }
    let sources: Vec<Value> = response
        .results
        .iter()
        .map(|result| {
            json!({
                "url": result.url,
                "title": result.title,
                "snippet": result.content,
                "publishedAt": result.published_date,
            })
        })
        .collect();
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&sources).unwrap_or_default() }] })
}

fn tool_error(message: &str) -> Value {
    json!({ "isError": true, "content": [{ "type": "text", "text": message }] })
}

fn rpc_error(id: Option<Value>, code: i32, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}
