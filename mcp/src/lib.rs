pub mod som;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine as _;
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

#[derive(Debug, Clone)]
pub struct CachedStageMarks {
    pub stage_id: String,
    pub frame_seq: u64,
    pub logical_width: f64,
    pub logical_height: f64,
    pub scale: f64,
    pub marks: HashMap<String, som::MarkTarget>,
}

#[derive(Debug, Default)]
pub struct McpState {
    by_lease: Mutex<HashMap<String, CachedStageMarks>>,
    by_stage: Mutex<HashMap<String, CachedStageMarks>>,
}

impl McpState {
    pub fn store_marks(&self, lease: Option<&str>, entry: CachedStageMarks) {
        if let Ok(mut map) = self.by_stage.lock() {
            map.insert(entry.stage_id.clone(), entry.clone());
        }
        if let Some(l) = lease {
            if let Ok(mut map) = self.by_lease.lock() {
                map.insert(l.to_string(), entry);
            }
        }
    }

    pub fn get_for_lease(&self, lease: &str) -> Option<CachedStageMarks> {
        self.by_lease.lock().ok()?.get(lease).cloned()
    }

    pub fn clear_lease(&self, lease: &str) {
        if let Ok(mut map) = self.by_lease.lock() {
            map.remove(lease);
        }
    }

    pub fn clear_stage(&self, stage_id: &str) {
        if let Ok(mut map) = self.by_stage.lock() {
            map.remove(stage_id);
        }
        if let Ok(mut map) = self.by_lease.lock() {
            map.retain(|_, v| v.stage_id != stage_id);
        }
    }
}

pub fn run_mcp_stdio(socket_path: PathBuf) -> anyhow::Result<()> {
    let client = ControlClient::new(socket_path);
    let state = McpState::default();
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

        let resp = handle_rpc(&client, &state, id, &req.method, req.params);
        writeln!(out, "{}", serde_json::to_string(&resp)?)?;
        out.flush()?;
    }
    Ok(())
}

fn handle_rpc(
    client: &ControlClient,
    state: &McpState,
    id: Value,
    method: &str,
    params: Value,
) -> JsonRpcResponse {
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
            let tool_result = call_tool(client, state, name, args);
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

fn call_tool(client: &ControlClient, state: &McpState, name: &str, args: Value) -> Value {
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
            let wm_class = args.get("wm_class").and_then(|v| v.as_str()).map(String::from);

            let req = ControlRequest::Launch {
                app,
                args: launch_args,
                kind,
                env: Default::default(),
                cwd,
                width,
                height,
                show_on_desktop,
                wm_class,
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
            let lease = args.get("lease").and_then(Value::as_str);
            let stage_id = args.get("stage_id").and_then(Value::as_str);
            let image = args.get("image").and_then(|v| v.as_bool()).unwrap_or(true);
            let som_enabled = args.get("som").and_then(|v| v.as_bool()).unwrap_or(false);
            let a11y = args.get("a11y").and_then(|v| v.as_bool()).unwrap_or(false);
            let include_marks = args
                .get("include_marks")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            observe_with_som(client, state, lease, stage_id, image, som_enabled, a11y, include_marks)
        }
        "stage_act" => {
            let lease = match args.get("lease").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return text_err("Missing 'lease'"),
            };
            let raw_actions = match args.get("actions").and_then(|v| v.as_array()) {
                Some(arr) => arr,
                None => return text_err("Missing or invalid 'actions' array"),
            };
            let cached = state.get_for_lease(&lease);
            let actions = match parse_and_expand_actions(raw_actions, cached.as_ref()) {
                Ok(a) => a,
                Err(e) => return text_err(e),
            };
            let based_on = args.get("based_on").and_then(|v| v.as_u64());
            let follow_observe = args
                .get("observe")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let som_enabled = args.get("som").and_then(|v| v.as_bool()).unwrap_or(false);
            let settle = args.get("settle").and_then(Value::as_bool);
            let settle = settle.or((!follow_observe).then_some(false));
            let settle_ms = args.get("settle_ms").and_then(Value::as_u64);

            match client.call(&ControlRequest::Act {
                lease: lease.clone(),
                actions,
                based_on,
                settle,
                settle_ms,
            }) {
                Ok(ControlResponse::ActResult {
                    act_status,
                    executed_steps,
                    frame_seq,
                    message,
                }) => {
                    let act_meta = json!({
                        "result": act_status,
                        "executed_steps": executed_steps,
                        "frame_seq": frame_seq,
                        "message": message
                    });
                    if follow_observe {
                        let obs = observe_with_som(
                            client,
                            state,
                            Some(&lease),
                            None,
                            true,
                            som_enabled,
                            false,
                            false,
                        );
                        merge_act_and_observe(act_meta, obs)
                    } else {
                        text_ok(act_meta)
                    }
                }
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
            state.clear_lease(&lease);
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
            state.clear_stage(&stage_id);
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

fn observe_with_som(
    client: &ControlClient,
    state: &McpState,
    lease: Option<&str>,
    stage_id: Option<&str>,
    image: bool,
    som_enabled: bool,
    a11y: bool,
    include_marks: bool,
) -> Value {
    if lease.is_none() && stage_id.is_none() {
        return text_err("Either 'lease' or 'stage_id' is required");
    }
    match client.call(&ControlRequest::Observe {
        lease: lease.map(String::from),
        stage_id: stage_id.filter(|_| lease.is_none()).map(String::from),
        image,
        a11y,
    }) {
        Ok(ControlResponse::Observed {
            stage_id,
            frame_seq,
            width,
            height,
            scale,
            png_base64,
            a11y_tree,
        }) => {
            let eff_scale = if scale > 0.0 { scale } else { 1.0 };
            let logical_w = (width as f64) / eff_scale;
            let logical_h = (height as f64) / eff_scale;

            let mut out_b64 = png_base64.clone();
            let mut mark_count: Option<usize> = None;
            let mut marks_payload: Option<Value> = None;

            if image && som_enabled {
                if let Some(ref raw_b64) = png_base64 {
                    match base64::engine::general_purpose::STANDARD.decode(raw_b64) {
                        Ok(raw_png) => match som::annotate_png(&raw_png, eff_scale) {
                            Ok(ann) => {
                                mark_count = Some(ann.marks.len());
                                if include_marks {
                                    let list: Vec<Value> = ann
                                        .ordered_codes
                                        .iter()
                                        .filter_map(|code| {
                                            ann.marks.get(code).map(|m| {
                                                json!({
                                                    "code": code,
                                                    "x": (m.x * 10.0).round() / 10.0,
                                                    "y": (m.y * 10.0).round() / 10.0,
                                                    "bbox": m.bbox
                                                })
                                            })
                                        })
                                        .collect();
                                    marks_payload = Some(Value::Array(list));
                                }
                                state.store_marks(
                                    lease,
                                    CachedStageMarks {
                                        stage_id: stage_id.clone(),
                                        frame_seq,
                                        logical_width: logical_w,
                                        logical_height: logical_h,
                                        scale: eff_scale,
                                        marks: ann.marks,
                                    },
                                );
                                out_b64 = Some(
                                    base64::engine::general_purpose::STANDARD
                                        .encode(ann.png_bytes),
                                );
                            }
                            Err(e) => {
                                return text_err(format!("SoM annotation failed: {e}"));
                            }
                        },
                        Err(e) => {
                            return text_err(format!("Failed to decode Stage PNG: {e}"));
                        }
                    }
                }
            }

            let mut meta = json!({
                "stage_id": stage_id,
                "frame_seq": frame_seq,
                "width": width,
                "height": height,
                "logical_width": logical_w.round() as u32,
                "logical_height": logical_h.round() as u32,
                "scale": eff_scale,
                "som": image && som_enabled,
            });
            if let Some(mc) = mark_count {
                meta["mark_count"] = json!(mc);
            }
            if let Some(mp) = marks_payload {
                meta["marks"] = mp;
            }
            if let Some(tree) = a11y_tree {
                meta["a11y_tree"] = json!(tree);
            }

            let mut content = vec![json!({
                "type": "text",
                "text": serde_json::to_string(&meta).unwrap()
            })];
            if let Some(b64) = out_b64 {
                content.push(json!({
                    "type": "image",
                    "data": b64,
                    "mimeType": "image/png"
                }));
            }
            json!({ "content": content, "isError": false })
        }
        Ok(ControlResponse::Error { code, message }) => text_err(format!("{code}: {message}")),
        Ok(other) => text_ok(json!(other)),
        Err(e) => text_err(e.to_string()),
    }
}

fn merge_act_and_observe(mut act_meta: Value, obs: Value) -> Value {
    let is_err = obs.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut content = Vec::new();
    let mut image_item: Option<Value> = None;

    if let Some(arr) = obs.get("content").and_then(|v| v.as_array()) {
        for item in arr {
            let kind = item.get("type").and_then(|v| v.as_str());
            if kind == Some("image") {
                image_item = Some(item.clone());
            } else if kind == Some("text") {
                if let Some(txt) = item.get("text").and_then(|v| v.as_str()) {
                    if let Ok(Value::Object(obs_map)) = serde_json::from_str::<Value>(txt) {
                        if let Some(act_map) = act_meta.as_object_mut() {
                            for (k, v) in obs_map {
                                act_map.insert(k, v);
                            }
                            continue;
                        }
                    }
                    act_meta["observe_message"] = Value::String(txt.to_string());
                }
            }
        }
    }

    content.push(json!({
        "type": "text",
        "text": serde_json::to_string(&act_meta).unwrap()
    }));
    if let Some(img) = image_item {
        content.push(img);
    }
    json!({ "content": content, "isError": is_err })
}

fn resolve_point_value(
    val: &Value,
    cache: Option<&CachedStageMarks>,
) -> Result<(f64, f64), String> {
    if let Some(s) = val.as_str() {
        let code = som::normalize_mark_code(s)
            .ok_or_else(|| format!("Invalid mark code '{s}' (expected e.g. 'AE' or 'P3')"))?;
        let marks = cache.ok_or_else(|| {
            format!("Cannot resolve mark '{code}': no cached marks for this lease; call stage_observe first")
        })?;
        let target = marks.marks.get(&code).ok_or_else(|| {
            format!(
                "Mark '{code}' not found in frame_seq {} ({} marks); call stage_observe to refresh",
                marks.frame_seq,
                marks.marks.len()
            )
        })?;
        return Ok((target.x, target.y));
    }
    if let Some(idx) = val.as_u64() {
        let code = som::encode_id(idx as usize);
        let marks = cache.ok_or_else(|| {
            format!("Cannot resolve mark '{code}': no cached marks for this lease; call stage_observe first")
        })?;
        let target = marks.marks.get(&code).ok_or_else(|| {
            format!(
                "Mark '{code}' not found in frame_seq {}; call stage_observe to refresh",
                marks.frame_seq
            )
        })?;
        return Ok((target.x, target.y));
    }
    if let Some(arr) = val.as_array() {
        if arr.len() == 2 {
            if let (Some(x), Some(y)) = (arr[0].as_f64(), arr[1].as_f64()) {
                return Ok((x, y));
            }
        }
        return Err("Coordinate array target must be [x, y]".to_string());
    }
    if let Some(obj) = val.as_object() {
        if let (Some(x), Some(y)) = (
            obj.get("x").and_then(|v| v.as_f64()),
            obj.get("y").and_then(|v| v.as_f64()),
        ) {
            return Ok((x, y));
        }
    }
    Err(format!("Unsupported target value: {val}"))
}

fn resolve_xy_or_target(
    obj: &serde_json::Map<String, Value>,
    cache: Option<&CachedStageMarks>,
) -> Result<(f64, f64), String> {
    if let Some(target) = obj.get("target").filter(|v| !v.is_null()) {
        return resolve_point_value(target, cache);
    }
    if let (Some(x), Some(y)) = (
        obj.get("x").and_then(|v| v.as_f64()),
        obj.get("y").and_then(|v| v.as_f64()),
    ) {
        return Ok((x, y));
    }
    Err("Action requires 'target' (mark code or [x,y]) or numeric 'x'/'y'".to_string())
}

fn parse_chord(raw: &str, explicit_mods: Option<Vec<String>>) -> (String, Option<Vec<String>>) {
    if explicit_mods.is_some() || !raw.contains('+') || raw == "+" {
        return (raw.to_string(), explicit_mods);
    }
    let parts: Vec<&str> = raw.split('+').collect();
    if parts.len() < 2 {
        return (raw.to_string(), None);
    }
    let key = parts.last().unwrap().trim();
    if key.is_empty() {
        return (raw.to_string(), None);
    }
    let mods: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
        .collect();
    if mods.is_empty() {
        (key.to_string(), None)
    } else {
        let norm_key = if key.len() == 1 {
            key.to_ascii_lowercase()
        } else {
            key.to_string()
        };
        (norm_key, Some(mods))
    }
}

pub fn parse_and_expand_actions(
    raw_actions: &[Value],
    cache: Option<&CachedStageMarks>,
) -> Result<Vec<Action>, String> {
    let mut out = Vec::with_capacity(raw_actions.len());
    for (idx, item) in raw_actions.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("actions[{idx}] must be an object"))?;
        let kind = obj
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("actions[{idx}] missing 'action' string"))?;

        match kind {
            "move" | "hover" => {
                let (x, y) = resolve_xy_or_target(obj, cache)?;
                out.push(Action::Move { x, y });
            }
            "click" => {
                let (x, y) = resolve_xy_or_target(obj, cache)?;
                let button = obj
                    .get("button")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let clicks = obj.get("clicks").and_then(|v| v.as_u64()).unwrap_or(1);
                if clicks == 2 {
                    out.push(Action::DoubleClick { x, y, button });
                } else {
                    out.push(Action::Click { x, y, button });
                }
            }
            "double_click" => {
                let (x, y) = resolve_xy_or_target(obj, cache)?;
                let button = obj
                    .get("button")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                out.push(Action::DoubleClick { x, y, button });
            }
            "drag" => {
                let (from_x, from_y) = if let Some(from_v) = obj.get("from").filter(|v| !v.is_null()) {
                    resolve_point_value(from_v, cache)?
                } else if let Some(target_v) = obj.get("target").filter(|v| !v.is_null()) {
                    resolve_point_value(target_v, cache)?
                } else if let (Some(x), Some(y)) = (
                    obj.get("from_x").and_then(|v| v.as_f64()),
                    obj.get("from_y").and_then(|v| v.as_f64()),
                ) {
                    (x, y)
                } else {
                    return Err(format!(
                        "actions[{idx}] drag requires 'from'/'target' or 'from_x'/'from_y'"
                    ));
                };

                let (to_x, to_y) = if let Some(to_v) = obj.get("to").filter(|v| !v.is_null()) {
                    resolve_point_value(to_v, cache)?
                } else if let Some(by_arr) = obj.get("by").and_then(|v| v.as_array()) {
                    if by_arr.len() == 2 {
                        let dx = by_arr[0].as_f64().ok_or("drag 'by[0]' must be a number")?;
                        let dy = by_arr[1].as_f64().ok_or("drag 'by[1]' must be a number")?;
                        (from_x + dx, from_y + dy)
                    } else {
                        return Err("drag 'by' must be [dx, dy]".to_string());
                    }
                } else if let (Some(x), Some(y)) = (
                    obj.get("to_x").and_then(|v| v.as_f64()),
                    obj.get("to_y").and_then(|v| v.as_f64()),
                ) {
                    (x, y)
                } else {
                    return Err(format!(
                        "actions[{idx}] drag requires 'to', 'by', or 'to_x'/'to_y'"
                    ));
                };

                let button = obj
                    .get("button")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let steps = obj.get("steps").and_then(|v| v.as_u64()).map(|n| n as u32);
                out.push(Action::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    button,
                    steps,
                });
            }
            "scroll" => {
                let (x, y) = if obj.get("target").is_some_and(|v| !v.is_null())
                    || (obj.get("x").is_some() && obj.get("y").is_some())
                {
                    resolve_xy_or_target(obj, cache)?
                } else if let Some(c) = cache {
                    (c.logical_width * 0.5, c.logical_height * 0.5)
                } else {
                    return Err(format!(
                        "actions[{idx}] scroll requires 'target'/'x'+'y' or an earlier stage_observe"
                    ));
                };

                let (dx, dy) = if let Some(dir) = obj.get("direction").and_then(|v| v.as_str()) {
                    let amount = obj.get("amount").and_then(|v| v.as_f64()).unwrap_or(120.0);
                    match dir.to_ascii_lowercase().as_str() {
                        "up" => (0.0, -amount),
                        "down" => (0.0, amount),
                        "left" => (-amount, 0.0),
                        "right" => (amount, 0.0),
                        other => {
                            return Err(format!("actions[{idx}] invalid scroll direction '{other}'"))
                        }
                    }
                } else {
                    let dx = obj.get("dx").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let dy = obj.get("dy").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    (dx, dy)
                };
                out.push(Action::Scroll { x, y, dx, dy });
            }
            "key" => {
                let explicit_mods: Option<Vec<String>> = obj
                    .get("modifiers")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let state = obj
                    .get("state")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let key_val = obj
                    .get("key")
                    .or_else(|| obj.get("target"))
                    .ok_or_else(|| format!("actions[{idx}] key requires 'key'"))?;

                if let Some(arr) = key_val.as_array() {
                    for entry in arr {
                        let s = entry
                            .as_str()
                            .ok_or_else(|| format!("actions[{idx}] key array items must be strings"))?;
                        let (k, mods) = parse_chord(s, explicit_mods.clone());
                        out.push(Action::Key {
                            key: k,
                            modifiers: mods,
                            state: state.clone(),
                        });
                    }
                } else if let Some(s) = key_val.as_str() {
                    let (k, mods) = parse_chord(s, explicit_mods);
                    out.push(Action::Key {
                        key: k,
                        modifiers: mods,
                        state,
                    });
                } else {
                    return Err(format!("actions[{idx}] 'key' must be string or array"));
                }
            }
            "type_text" | "type" => {
                let text = obj
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("actions[{idx}] {kind} requires 'text'"))?;
                if let Some(target) = obj.get("target").filter(|v| !v.is_null()) {
                    let (x, y) = resolve_point_value(target, cache)?;
                    out.push(Action::Click {
                        x,
                        y,
                        button: Some("left".to_string()),
                    });
                } else if let (Some(x), Some(y)) = (
                    obj.get("x").and_then(|v| v.as_f64()),
                    obj.get("y").and_then(|v| v.as_f64()),
                ) {
                    out.push(Action::Click {
                        x,
                        y,
                        button: Some("left".to_string()),
                    });
                }

                let mode = obj
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("add");
                if mode.eq_ignore_ascii_case("cover") {
                    out.push(Action::Key {
                        key: "a".to_string(),
                        modifiers: Some(vec!["ctrl".to_string()]),
                        state: None,
                    });
                    out.push(Action::Key {
                        key: "BackSpace".to_string(),
                        modifiers: None,
                        state: None,
                    });
                }

                out.push(Action::TypeText {
                    text: text.to_string(),
                });

                if obj.get("send").and_then(|v| v.as_bool()).unwrap_or(false) {
                    out.push(Action::Key {
                        key: "Return".to_string(),
                        modifiers: None,
                        state: None,
                    });
                }
            }
            "wait" | "sleep" => {
                let ms = if let Some(ms) = obj.get("ms").and_then(|v| v.as_u64()) {
                    ms
                } else if let Some(sec) = obj.get("seconds").and_then(|v| v.as_f64()) {
                    (sec.clamp(0.0, 60.0) * 1000.0).round() as u64
                } else {
                    return Err(format!("actions[{idx}] {kind} requires 'ms' or 'seconds'"));
                };
                out.push(Action::Wait { ms });
            }
            other => {
                return Err(format!("actions[{idx}] unknown action '{other}'"));
            }
        }
    }
    Ok(out)
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
                    "show_on_desktop": { "type": "boolean" },
                    "wm_class": { "type": "string", "description": "Optional application identity override for window grouping and icon lookup" }
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
            "description": "Observe a Stage by lease or by stage_id. Observing by stage_id is read-only and leaves input ownership and visibility unchanged. Screenshots are unmarked by default. Set som to true to generate MarkScreen Set-of-Mark badges; use marks from a leased observation as targets in stage_act.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lease": { "type": "string", "description": "Lease token. Takes precedence when both identifiers are supplied." },
                    "stage_id": { "type": "string", "description": "Stage ID for observation without a lease." },
                    "image": { "type": "boolean", "default": true },
                    "som": { "type": "boolean", "default": false, "description": "Annotate the screenshot with MarkScreen SoM boxes and 2-letter badges." },
                    "a11y": { "type": "boolean", "default": false },
                    "include_marks": { "type": "boolean", "default": false, "description": "Include resolved mark coordinates in text output when som and image are enabled." }
                },
                "anyOf": [{ "required": ["lease"] }, { "required": ["stage_id"] }]
            }
        }),
        json!({
            "name": "stage_act",
            "description": "Execute input actions on a leased Stage. By default, wait for the UI to settle and return a fresh unmarked screenshot. Targets accept [x, y] Stage logical coordinates, explicit x/y fields, or mark codes (e.g. 'AE' or 'P3') from a screenshot taken with som enabled on the same lease. Supports click (with clicks=2/button), double_click, move/hover, drag (from/to or by), scroll (direction up/down/left/right or dx/dy), key (single key or chord like 'Ctrl+A' or array), type_text/type (with optional target click, mode='cover', send=true), and wait/sleep.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lease": { "type": "string" },
                    "based_on": { "type": "integer", "description": "Optional frame_seq for stale-frame guard" },
                    "observe": { "type": "boolean", "default": true, "description": "Return a fresh screenshot after executing actions." },
                    "settle": { "type": "boolean", "description": "Wait for the UI to settle after executing actions. Defaults to the value of observe." },
                    "settle_ms": { "type": "integer", "minimum": 0, "maximum": 10000, "description": "Maximum UI settling time in milliseconds (default 600). Zero skips settling." },
                    "som": { "type": "boolean", "default": false, "description": "Annotate the follow-up screenshot with MarkScreen SoM boxes and badges." },
                    "actions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "description": "Action object with 'action': click, double_click, move, hover, drag, scroll, key, type_text, type, wait, sleep"
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

    fn sample_cache() -> CachedStageMarks {
        let mut marks = HashMap::new();
        marks.insert(
            "AA".to_string(),
            som::MarkTarget {
                x: 42.5,
                y: 18.0,
                bbox: [20, 10, 65, 26],
                area: 500,
            },
        );
        marks.insert(
            "AC".to_string(),
            som::MarkTarget {
                x: 310.0,
                y: 220.0,
                bbox: [260, 200, 360, 240],
                area: 2000,
            },
        );
        CachedStageMarks {
            stage_id: "stage-1".to_string(),
            frame_seq: 7,
            logical_width: 800.0,
            logical_height: 600.0,
            scale: 1.0,
            marks,
        }
    }

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
        for name in ["stage_observe", "stage_act"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(tool["inputSchema"]["properties"]["som"]["default"], false);
        }
    }

    #[test]
    fn observe_schema_accepts_either_identifier() {
        let schema = &mcp_tools_schema().into_iter()
            .find(|tool| tool["name"] == "stage_observe").unwrap()["inputSchema"];
        assert_eq!(schema["properties"]["lease"]["type"], "string");
        assert_eq!(schema["properties"]["stage_id"]["type"], "string");
        assert_eq!(schema["anyOf"], json!([
            { "required": ["lease"] }, { "required": ["stage_id"] }
        ]));
    }

    #[test]
    fn observe_rejects_missing_identifiers_before_using_control_socket() {
        let client = ControlClient::new("/tmp/nonexistent-focusstage.sock");
        let result = call_tool(&client, &McpState::default(), "stage_observe", json!({}));
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "Either 'lease' or 'stage_id' is required");
    }

    #[test]
    fn observe_by_stage_sends_no_lease_and_caches_only_by_stage() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, 64, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.write_header().unwrap().write_image_data(&[255; 64 * 64 * 4]).unwrap();
        }
        let png_base64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        let raw_png = png_base64.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                assert!(matches!(
                    serde_json::from_str(&request).unwrap(),
                    ControlRequest::Observe { lease: None, stage_id: Some(id), image: true, a11y: true }
                        if id == "stage-1"
                ));
                let response = ControlResponse::Observed {
                    stage_id: "stage-1".into(),
                    frame_seq: 8,
                    width: 64,
                    height: 64,
                    scale: 1.0,
                    png_base64: Some(png_base64.clone()),
                    a11y_tree: Some(Vec::new()),
                };
                writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
            }
        });

        let state = McpState::default();
        state.store_marks(Some("lease-1"), sample_cache());
        let client = ControlClient::new(socket);
        let result = call_tool(&client, &state, "stage_observe", json!({
            "stage_id": "stage-1", "a11y": true
        }));
        assert_eq!(result["isError"], false, "{result}");
        let meta: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(meta["som"], false);
        assert_eq!(result["content"][1]["data"], raw_png);
        assert_eq!(state.by_stage.lock().unwrap()["stage-1"].frame_seq, 7);

        let result = call_tool(&client, &state, "stage_observe", json!({
            "stage_id": "stage-1", "a11y": true, "som": true
        }));
        assert_eq!(result["isError"], false, "{result}");
        let meta: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(meta["som"], true);
        assert_eq!(state.by_stage.lock().unwrap()["stage-1"].frame_seq, 8);
        assert_eq!(state.get_for_lease("lease-1").unwrap().frame_seq, 7);
        assert_eq!(state.by_lease.lock().unwrap().len(), 1);
        server.join().unwrap();
    }

    #[test]
    fn mcp_rpc_initialize_and_unknown_method() {
        let client = ControlClient::new(PathBuf::from("/tmp/nonexistent-focusstage.sock"));
        let state = McpState::default();
        let init = handle_rpc(&client, &state, json!(1), "initialize", json!({}));
        assert!(init.error.is_none());
        assert_eq!(
            init.result.unwrap()["serverInfo"]["name"],
            json!("focusstage-mcp")
        );

        let unknown = handle_rpc(&client, &state, json!(2), "does/not/exist", json!({}));
        assert!(unknown.result.is_none());
        assert_eq!(unknown.error.unwrap().code, -32601);
    }

    #[test]
    fn resolves_mark_codes_chords_and_markscreen_ergonomics() {
        let cache = sample_cache();
        let raw = vec![
            json!({ "action": "click", "target": "aa", "button": "right" }),
            json!({ "action": "click", "target": "P1", "clicks": 2 }),
            json!({ "action": "drag", "from": "AA", "to": "ac" }),
            json!({ "action": "drag", "from": [10, 20], "by": [50, -5] }),
            json!({ "action": "scroll", "direction": "down", "amount": 90 }),
            json!({ "action": "type", "target": "AC", "text": "hello", "mode": "cover", "send": true }),
            json!({ "action": "key", "key": ["Ctrl+A", "BackSpace"] }),
            json!({ "action": "sleep", "seconds": 0.25 }),
        ];

        let expanded = parse_and_expand_actions(&raw, Some(&cache)).expect("expand actions");
        assert!(matches!(
            &expanded[0],
            Action::Click { x, y, button: Some(b) } if (*x - 42.5).abs() < 1e-6 && (*y - 18.0).abs() < 1e-6 && b == "right"
        ));
        assert!(matches!(
            &expanded[1],
            Action::DoubleClick { x, y, .. } if (*x - 310.0).abs() < 1e-6 && (*y - 220.0).abs() < 1e-6
        ));
        assert!(matches!(
            &expanded[2],
            Action::Drag { from_x, from_y, to_x, to_y, .. }
                if (*from_x - 42.5).abs() < 1e-6 && (*from_y - 18.0).abs() < 1e-6
                && (*to_x - 310.0).abs() < 1e-6 && (*to_y - 220.0).abs() < 1e-6
        ));
        assert!(matches!(
            &expanded[3],
            Action::Drag { from_x, from_y, to_x, to_y, .. }
                if (*from_x - 10.0).abs() < 1e-6 && (*from_y - 20.0).abs() < 1e-6
                && (*to_x - 60.0).abs() < 1e-6 && (*to_y - 15.0).abs() < 1e-6
        ));
        assert!(matches!(
            &expanded[4],
            Action::Scroll { x, y, dx, dy }
                if (*x - 400.0).abs() < 1e-6 && (*y - 300.0).abs() < 1e-6
                && dx.abs() < 1e-6 && (*dy - 90.0).abs() < 1e-6
        ));
        // type with target + cover + send expands into Click -> Ctrl+A -> BackSpace -> TypeText -> Return
        assert!(matches!(&expanded[5], Action::Click { x, y, .. } if (*x - 310.0).abs() < 1e-6 && (*y - 220.0).abs() < 1e-6));
        assert!(matches!(&expanded[6], Action::Key { key, modifiers: Some(m), .. } if key == "a" && m == &vec!["ctrl".to_string()]));
        assert!(matches!(&expanded[7], Action::Key { key, .. } if key == "BackSpace"));
        assert!(matches!(&expanded[8], Action::TypeText { text } if text == "hello"));
        assert!(matches!(&expanded[9], Action::Key { key, .. } if key == "Return"));
        assert!(matches!(&expanded[10], Action::Key { key, modifiers: Some(m), .. } if key == "a" && m == &vec!["ctrl".to_string()]));
        assert!(matches!(&expanded[11], Action::Key { key, .. } if key == "BackSpace"));
        assert!(matches!(&expanded[12], Action::Wait { ms: 250 }));
    }
}
