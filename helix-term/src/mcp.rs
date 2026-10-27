//! `hx mcp`: a stdio MCP server that gives agents without an IDE protocol (OpenCode) the
//! editor's context: the selection, open files, language server diagnostics, and opening a file
//! at a line. Each tool call is answered by the running editor over its IPC socket
//! (`HELIDE_SOCKET`, inherited by agents the editor starts), so the tools see that editor.
//!
//! The transport is MCP over stdio: one JSON-RPC message per line.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2024-11-05";

fn tools() -> Value {
    let none = json!({ "type": "object", "properties": {} });
    json!([
        {
            "name": "get_selection",
            "description": "The file focused in the user's Helix editor, the cursor's line and column, and the selected text. Use it when the user says \"this\", \"here\" or \"the selection\".",
            "inputSchema": none,
        },
        {
            "name": "get_open_files",
            "description": "The files open in the user's Helix editor, with unsaved-change flags.",
            "inputSchema": none,
        },
        {
            "name": "get_diagnostics",
            "description": "Language server errors and warnings from the editor, for one file or every file.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Absolute path; omit for all files" } },
            },
        },
        {
            "name": "open_file",
            "description": "Show a file in the user's editor, at a line when given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path" },
                    "line": { "type": "integer", "description": "1-based line" },
                },
                "required": ["path"],
            },
        },
    ])
}

/// Answers one tool call through the editor's IPC socket.
fn call(name: &str, arguments: &Value) -> anyhow::Result<Value> {
    let (method, params) = match name {
        "get_selection" => ("status", json!({})),
        "get_open_files" => ("status", json!({})),
        "get_diagnostics" => ("diagnostics", json!({ "path": arguments.get("path") })),
        "open_file" => (
            "open",
            json!({ "path": arguments["path"], "line": arguments.get("line") }),
        ),
        other => anyhow::bail!("unknown tool '{other}'"),
    };
    let response = crate::ipc::client::request(method, params)?;
    if response["ok"].as_bool() != Some(true) {
        anyhow::bail!(
            "{}",
            response["error"].as_str().unwrap_or("the editor refused")
        );
    }
    let result = &response["result"];
    Ok(match name {
        "get_selection" => result["focused"].clone(),
        "get_open_files" => result["buffers"].clone(),
        _ => result.clone(),
    })
}

fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "helide", "version": helix_loader::VERSION_AND_GIT_HASH },
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "ping" => Ok(json!({})),
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or_default();
            let arguments = &message["params"]["arguments"];
            Ok(match call(name, arguments) {
                Ok(value) => json!({ "content": [{ "type": "text", "text": value.to_string() }] }),
                Err(err) => json!({
                    "content": [{ "type": "text", "text": err.to_string() }],
                    "isError": true,
                }),
            })
        }
        _ => Err(json!({ "code": -32601, "message": "Method not found" })),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    })
}

/// Serves MCP on stdin and stdout until stdin closes.
pub fn run() -> anyhow::Result<i32> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications (no id) need no answer.
        if let Some(response) = handle(&message) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_the_handshake_and_lists_tools() {
        let init = handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" } }))
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "helide");
        let list = handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 4);
        assert!(
            handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none()
        );
        let unknown = handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" })).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }
}
