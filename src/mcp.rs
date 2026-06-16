//! Minimal Model Context Protocol (MCP) server over stdio.
//!
//! Exposes a single `render_diagram` tool that renders Mermaid source to SVG
//! or PNG using the same pipeline as the CLI. The transport is newline-
//! delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio convention): one JSON
//! message per line, responses written back one per line. All diagnostics go
//! to stderr so stdout stays a clean JSON-RPC channel.
//!
//! Implemented by hand (serde_json only) to avoid pulling an async runtime or
//! the rmcp SDK into an otherwise small, synchronous renderer.

use crate::cli::merge_init_config;
use crate::config::load_config;
use crate::layout::compute_layout_with_metrics;
use crate::parser::parse_mermaid;
use crate::render::render_svg_with_dimensions;
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the MCP server until stdin closes (EOF).
pub fn serve() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    eprintln!("mmdr mcp: server started (protocol {PROTOCOL_VERSION})");

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // Parse error — no id is recoverable, reply per JSON-RPC.
                write_message(&mut stdout, &error_response(&Value::Null, -32700, &format!("parse error: {e}")))?;
                continue;
            }
        };

        // Notifications carry no `id` and must not be answered.
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        if id.is_none() {
            // It's a notification (e.g. notifications/initialized). Ignore.
            continue;
        }
        let id = id.unwrap();

        let response = match method {
            "initialize" => success(&id, initialize_result()),
            "tools/list" => success(&id, tools_list_result()),
            "tools/call" => handle_tools_call(&id, &params),
            "ping" => success(&id, json!({})),
            other => error_response(&id, -32601, &format!("method not found: {other}")),
        };
        write_message(&mut stdout, &response)?;
    }
    Ok(())
}

fn write_message(out: &mut impl Write, msg: &Value) -> Result<()> {
    let line = serde_json::to_string(msg)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn success(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "mmdr", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [{
            "name": "render_diagram",
            "description": "Render a Mermaid diagram (flowchart, C4, sequence, etc.) to SVG or PNG. \
PNG is returned inline as a base64 image unless output_path is given. Pass config_path to apply \
a JSON config such as a C4 layout template.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Mermaid diagram source text" },
                    "format": { "type": "string", "enum": ["svg", "png"], "description": "Output format (default png)" },
                    "width": { "type": "number", "description": "Canvas width (default 1200)" },
                    "height": { "type": "number", "description": "Canvas height (default 800)" },
                    "scale": { "type": "number", "description": "PNG supersampling factor 1-8 (default 2)" },
                    "config_path": { "type": "string", "description": "Path to a JSON config file (e.g. mmdr-c4.json)" },
                    "output_path": { "type": "string", "description": "If set, write the rendered file here and return its path instead of inline content" }
                },
                "required": ["code"]
            }
        }]
    })
}

fn handle_tools_call(id: &Value, params: &Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name != "render_diagram" {
        return error_response(id, -32602, &format!("unknown tool: {name}"));
    }
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match render_tool(&args) {
        Ok(content) => success(id, json!({ "content": [content], "isError": false })),
        // Tool-execution failures are reported in-band (isError) per MCP, not
        // as JSON-RPC protocol errors, so the client model can see the message.
        Err(msg) => success(
            id,
            json!({ "content": [{ "type": "text", "text": format!("render_diagram failed: {msg}") }], "isError": true }),
        ),
    }
}

/// Render per the tool arguments, returning a single MCP content item.
fn render_tool(args: &Value) -> Result<Value, String> {
    let code = args
        .get("code")
        .and_then(|c| c.as_str())
        .ok_or("missing required string argument `code`")?;
    let format = args.get("format").and_then(|v| v.as_str()).unwrap_or("png");
    let width = args.get("width").and_then(|v| v.as_f64()).unwrap_or(1200.0) as f32;
    let height = args.get("height").and_then(|v| v.as_f64()).unwrap_or(800.0) as f32;
    let scale = args.get("scale").and_then(|v| v.as_f64()).unwrap_or(2.0) as f32;
    let config_path = args.get("config_path").and_then(|v| v.as_str()).map(PathBuf::from);
    let output_path = args.get("output_path").and_then(|v| v.as_str()).map(PathBuf::from);

    let mut config = load_config(config_path.as_deref()).map_err(|e| e.to_string())?;
    config.render.width = width;
    config.render.height = height;
    config.render.scale = scale;

    let parsed = parse_mermaid(code).map_err(|e| e.to_string())?;
    if let Some(init_cfg) = parsed.init_config {
        config = merge_init_config(config, init_cfg);
    }
    let (layout, _stages) =
        compute_layout_with_metrics(&parsed.graph, &config.theme, &config.layout);
    let svg = render_svg_with_dimensions(
        &layout,
        &config.theme,
        &config.layout,
        Some((config.render.width, config.render.height)),
    );

    match format {
        "svg" => {
            if let Some(path) = &output_path {
                std::fs::write(path, svg.as_bytes()).map_err(|e| e.to_string())?;
                Ok(json!({ "type": "text", "text": format!("Wrote SVG to {}", path.display()) }))
            } else {
                Ok(json!({ "type": "text", "text": svg }))
            }
        }
        "png" => render_png(&svg, &config, output_path.as_deref()),
        other => Err(format!("unknown format `{other}` (expected svg or png)")),
    }
}

#[cfg(feature = "png")]
fn render_png(
    svg: &str,
    config: &crate::config::Config,
    output_path: Option<&std::path::Path>,
) -> Result<Value, String> {
    let bytes = crate::render::svg_to_png_bytes(svg, &config.render, &config.theme)
        .map_err(|e| e.to_string())?;
    if let Some(path) = output_path {
        std::fs::write(path, &bytes).map_err(|e| e.to_string())?;
        Ok(json!({ "type": "text", "text": format!("Wrote PNG to {}", path.display()) }))
    } else {
        Ok(json!({ "type": "image", "data": base64_encode(&bytes), "mimeType": "image/png" }))
    }
}

#[cfg(not(feature = "png"))]
fn render_png(
    _svg: &str,
    _config: &crate::config::Config,
    _output_path: Option<&std::path::Path>,
) -> Result<Value, String> {
    Err("PNG output requires the `png` feature".to_string())
}

/// Standard base64 (RFC 4648) with padding. Hand-rolled to avoid a dependency.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn render_tool_svg_roundtrips() {
        let out = render_tool(&json!({ "code": "flowchart LR\n A --> B", "format": "svg" }))
            .expect("render");
        assert_eq!(out["type"], "text");
        assert!(out["text"].as_str().unwrap().contains("<svg"));
    }

    #[test]
    fn render_tool_missing_code_errors() {
        assert!(render_tool(&json!({ "format": "svg" })).is_err());
    }
}
