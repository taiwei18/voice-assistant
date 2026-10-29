//! `voice-assistant.exe --mcp`：把本程序的桌面工具（启动/关闭程序、UI Automation 读控件、点击、输入、快捷键）
//! 以 MCP（标准输入输出 + JSON-RPC）提供给 Codex / Claude Code 这些后台智能体。
//! 这个模式不开界面、不占麦克风，由智能体按需启动，任务结束时随它一起退出。

use std::io::{BufRead, Write};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::tools::Tools;
use crate::tools::apps::AppIndex;

pub fn serve() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let apps = Arc::new(AppIndex::load_cached());
    let tools = Tools::desktop_only(apps);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = req.get("id").cloned() else { continue }; // 通知不用回复
        let result = match req["method"].as_str().unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "voice_desktop", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "操作用户 Windows 桌面的工具：启动/关闭程序、读窗口控件、点击、输入、按快捷键。先 desktop_focus 或 launch_app 把目标窗口切到前台，再 desktop_snapshot 拿控件编号。"
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools.definitions().iter().map(to_mcp_tool).collect::<Vec<_>>()}),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                let args = req["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
                let (text, is_error) = runtime.block_on(tools.call(name, &args));
                json!({"content": [{"type": "text", "text": text}], "isError": is_error})
            }
            other => {
                reply(&mut stdout, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("不支持的方法：{other}")}}))?;
                continue;
            }
        };
        reply(&mut stdout, json!({"jsonrpc": "2.0", "id": id, "result": result}))?;
    }
    Ok(())
}

fn reply(out: &mut std::io::Stdout, msg: Value) -> anyhow::Result<()> {
    writeln!(out, "{msg}")?;
    out.flush()?;
    Ok(())
}

/// 我们的工具定义是 Anthropic 格式（input_schema），MCP 用 inputSchema
fn to_mcp_tool(def: &Value) -> Value {
    json!({"name": def["name"], "description": def["description"], "inputSchema": def["input_schema"]})
}
