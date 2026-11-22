use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use focusstage_compositor::control::ControlClient;
use focusstage_compositor::protocol::{Action, ControlRequest, ControlResponse, StageKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

pub fn run_mcp_stdio(socket_path: PathBuf) -> anyhow::Result<()> {
    let client = ControlClient::new(socket_path);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut out = stdout.lock();

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {e}"),
                    }),
                };
                writeln!(out, "{}", serde_json::to_string(&err_resp)?)?;
                out.flush()?;
                continue;
            }
        };

        // Notifications have no id and expect no response.
        let Some(id) = req.id else {
            continue;
        };

        let resp = handle_rpc(&client, id, &req.method, req.params);
        writeln!(out, "{}", serde_json::to_string(&resp)?)?;
        out.flush()?;
    }
    Ok(())
}

fn handle_rpc(client: &ControlClient, id: Value, method: &str, params: Value) -> JsonRpcResponse {
    match method {
        "initialize" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "focusstage-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            error: None,
        },
        "ping" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({})),
            error: None,
        },
        "tools/list" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({ "tools": mcp_tools_schema() })),
            error: None,
        },
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let tool_result = call_tool(client, name, args);
            JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(tool_result),
                error: None,
            }
        }
        _ => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("Method not found: {}", method),
            }),
        },
    }
}

fn call_tool(client: &ControlClient, name: &str, args: Value) -> Value {
    let normalized = name.replace('.', "_");
    match normalized.as_str() {
        "stage_list" => match client.call(&ControlRequest::List) {
            Ok(ControlResponse::Stages { stages }) => text_ok(json!({ "stages": stages })),
            Ok(other) => text_ok(json!(other)),
            Err(e) => text_err(e.to_string()),
        },
        "stage_launch" => {
            let app = match args.get("app").and_then(|v| v.as_str()) {
                Some(a) => a.to_string(),
                None => return text_err("Missing required parameter 'app'"),
            };
            let launch_args: Vec<String> = args
                .get("args")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let kind: Option<StageKind> = args
                .get("kind")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let width = args.get("width").and_then(|v| v.as_u64()).map(|n| n as u32);
            let height = args.get("height").and_then(|v| v.as_u64()).map(|n| n as u32);
            let show_on_desktop = args.get("show_on_desktop").and_then(|v| v.as_bool());
            let cwd = args.get("cwd").and_then(|v| v.as_str()).map(String::from);

            let req = ControlRequest::Launch {
                app,
                args: launch_args,
                kind,
                env: Default::default(),
                cwd,
                width,
                height,
                show_on_desktop,
            };
            match client.call(&req) {
                Ok(ControlResponse::Launched { stage_id, pid }) => {
                    text_ok(json!({ "stage_id": stage_id, "pid": pid }))
                }
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_acquire" => {
            let stage_id = match args.get("stage_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return text_err("Missing 'stage_id'"),
            };
            let ttl_s = args.get("ttl_s").and_then(|v| v.as_u64()).unwrap_or(60);
            let hide = args.get("hide").and_then(|v| v.as_bool()).unwrap_or(true);
            let agent_name = args
                .get("agent_name")
                .and_then(|v| v.as_str())
                .map(String::from);
            match client.call(&ControlRequest::Acquire {
                stage_id,
                ttl_s,
                hide,
                agent_name,
            }) {
                Ok(ControlResponse::Acquired {
                    lease,
                    stage_id,
                    expires_in_s,
                }) => text_ok(json!({
                    "lease": lease,
                    "stage_id": stage_id,
                    "expires_in_s": expires_in_s
                })),
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_renew" => {
            let lease = match args.get("lease").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return text_err("Missing 'lease'"),
            };
            let ttl_s = args.get("ttl_s").and_then(|v| v.as_u64()).unwrap_or(60);
            match client.call(&ControlRequest::Renew { lease, ttl_s }) {
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_observe" => {
            let lease = args.get("lease").and_then(|v| v.as_str()).map(String::from);
            if lease.is_none() {
                return text_err("Missing 'lease'");
            }
            let image = args.get("image").and_then(|v| v.as_bool()).unwrap_or(true);
            let a11y = args.get("a11y").and_then(|v| v.as_bool()).unwrap_or(false);
            match client.call(&ControlRequest::Observe {
                lease,
                stage_id: None,
                image,
                a11y,
            }) {
                Ok(ControlResponse::Observed {
                    stage_id,
                    frame_seq,
                    width,
                    height,
                    png_base64,
                    a11y_tree,
                }) => {
                    let mut content = vec![json!({
                        "type": "text",
                        "text": serde_json::to_string(&json!({
                            "stage_id": stage_id,
                            "frame_seq": frame_seq,
                            "width": width,
                            "height": height,
                            "a11y_tree": a11y_tree,
                        })).unwrap()
                    })];
                    if let Some(b64) = png_base64 {
                        content.push(json!({
                            "type": "image",
                            "data": b64,
                            "mimeType": "image/png"
                        }));
                    }
                    json!({ "content": content, "isError": false })
                }
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_act" => {
            let lease = match args.get("lease").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return text_err("Missing 'lease'"),
            };
            let actions: Vec<Action> = match args
                .get("actions")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
            {
                Some(a) => a,
                None => return text_err("Missing or invalid 'actions' array"),
            };
            let based_on = args.get("based_on").and_then(|v| v.as_u64());
            match client.call(&ControlRequest::Act {
                lease,
                actions,
                based_on,
            }) {
                Ok(ControlResponse::ActResult {
                    act_status,
                    executed_steps,
                    frame_seq,
                    message,
                }) => text_ok(json!({
                    "result": act_status,
                    "executed_steps": executed_steps,
                    "frame_seq": frame_seq,
                    "message": message
                })),
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_clipboard_get" => {
            let lease = args.get("lease").and_then(|v| v.as_str()).map(String::from);
            match client.call(&ControlRequest::ClipboardGet {
                lease,
                stage_id: None,
            }) {
                Ok(ControlResponse::Clipboard { text, seq }) => {
                    text_ok(json!({ "text": text, "seq": seq }))
                }
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_clipboard_set" => {
            let lease = args.get("lease").and_then(|v| v.as_str()).map(String::from);
            let text = match args.get("text").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => return text_err("Missing 'text'"),
            };
            match client.call(&ControlRequest::ClipboardSet {
                lease,
                stage_id: None,
                text,
            }) {
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_release" => {
            let lease = match args.get("lease").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return text_err("Missing 'lease'"),
            };
            let show_on_desktop = args
                .get("show_on_desktop")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            match client.call(&ControlRequest::Release {
                lease,
                show_on_desktop,
            }) {
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        "stage_close" => {
            let stage_id = match args.get("stage_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return text_err("Missing 'stage_id'"),
            };
            match client.call(&ControlRequest::Close { stage_id }) {
                Ok(ControlResponse::Error { code, message }) => {
                    text_err(format!("{code}: {message}"))
                }
                Ok(other) => text_ok(json!(other)),
                Err(e) => text_err(e.to_string()),
            }
        }
        _ => text_err(format!("Unknown tool: {}", name)),
    }
}

fn text_ok(val: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string())
        }],
        "isError": false
    })
}

fn text_err(msg: impl Into<String>) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": msg.into()
        }],
        "isError": true
    })
}

fn mcp_tools_schema() -> Vec<Value> {
    vec![
        json!({
            "name": "stage_list",
            "description": "List all managed FocusStage app stages with display/input ownership state, size, and frame sequence.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "stage_launch",
            "description": "Launch an application inside an isolated FocusStage (independent process/profile/Xwayland).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app": { "type": "string", "description": "Executable name or path (e.g. chromium, foot, xterm)" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "kind": { "type": "string", "enum": ["wayland", "chromium", "electron", "x11"] },
                    "cwd": { "type": "string" },
                    "width": { "type": "integer" },
                    "height": { "type": "integer" },
                    "show_on_desktop": { "type": "boolean" }
                },
                "required": ["app"]
            }
        }),
        json!({
            "name": "stage_acquire",
            "description": "Acquire an exclusive AI control lease for a Stage (optionally hiding it from the Windows desktop).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "stage_id": { "type": "string" },
                    "ttl_s": { "type": "integer", "default": 60 },
                    "hide": { "type": "boolean", "default": true },
                    "agent_name": { "type": "string" }
                },
                "required": ["stage_id"]
            }
        }),
        json!({
            "name": "stage_renew",
            "description": "Extend an active Stage lease TTL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lease": { "type": "string" },
                    "ttl_s": { "type": "integer", "default": 60 }
                },
                "required": ["lease"]
            }
        }),
        json!({
            "name": "stage_observe",
            "description": "Capture an on-demand screenshot (PNG) and/or AT-SPI2 accessibility tree for the leased Stage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lease": { "type": "string" },
                    "image": { "type": "boolean", "default": true },
                    "a11y": { "type": "boolean", "default": false }
                },
                "required": ["lease"]
            }
        }),
        json!({
            "name": "stage_act",
            "description": "Execute a sequence of input actions on a leased Stage (coordinates in Stage-relative logical pixels).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lease": { "type": "string" },
                    "based_on": { "type": "integer", "description": "Optional frame_seq for stale-frame guard" },
                    "actions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "description": "Action object with 'action' discriminator: move, click, double_click, drag, scroll, key, type_text, wait"
                        }
                    }
                },
                "required": ["lease", "actions"]
            }
        }),
        json!({
            "name": "stage_clipboard_get",
            "description": "Read the isolated clipboard text of the leased Stage.",
            "inputSchema": {
                "type": "object",
                "properties": { "lease": { "type": "string" } },
                "required": ["lease"]
            }
        }),
        json!({
            "name": "stage_clipboard_set",
            "description": "Set the isolated clipboard text of the leased Stage without affecting Windows or other Stages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lease": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["lease", "text"]
            }
        }),
        json!({
            "name": "stage_release",
            "description": "Release the Stage lease and optionally restore the window to its saved desktop position.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lease": { "type": "string" },
                    "show_on_desktop": { "type": "boolean", "default": true }
                },
                "required": ["lease"]
            }
        }),
        json!({
            "name": "stage_close",
            "description": "Close and clean up a Stage.",
            "inputSchema": {
                "type": "object",
                "properties": { "stage_id": { "type": "string" } },
                "required": ["stage_id"]
            }
        })
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_schema_lists_all_tools() {
        let tools = mcp_tools_schema();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(names.len(), 10);
        assert!(names.contains(&"stage_list"));
        assert!(names.contains(&"stage_launch"));
        assert!(names.contains(&"stage_acquire"));
        assert!(names.contains(&"stage_observe"));
        assert!(names.contains(&"stage_act"));
        assert!(names.contains(&"stage_release"));
        assert!(names.contains(&"stage_close"));
    }

    #[test]
    fn mcp_rpc_initialize_and_unknown_method() {
        let client = ControlClient::new(PathBuf::from("/tmp/nonexistent-focusstage.sock"));
        let init = handle_rpc(&client, json!(1), "initialize", json!({}));
        assert!(init.error.is_none());
        assert_eq!(
            init.result.unwrap()["serverInfo"]["name"],
            json!("focusstage-mcp")
        );

        let unknown = handle_rpc(&client, json!(2), "does/not/exist", json!({}));
        assert!(unknown.result.is_none());
        assert_eq!(unknown.error.unwrap().code, -32601);
    }
}

