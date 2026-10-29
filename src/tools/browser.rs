//! 浏览器控制：按需启动 Playwright MCP（独立的 Edge 窗口），闲置一段时间后自动关闭，平时不占内存。
//! 这里自己实现了一个最小的 MCP 客户端（JSON-RPC over stdio）。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use crate::config::{self, NoWindow};

const TOOLS_JSON: &str = include_str!("../../assets/browser_tools.json");
const MAX_RESULT_CHARS: usize = 30_000;

pub fn definitions() -> Vec<Value> {
    serde_json::from_str(TOOLS_JSON).unwrap_or_default()
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

struct Session {
    child: Child,
    stdin: ChildStdin,
    pending: Pending,
    next_id: u64,
}

impl Session {
    async fn send(&mut self, msg: Value) -> anyhow::Result<()> {
        self.stdin.write_all(format!("{msg}\n").as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let resp = tokio::time::timeout(Duration::from_secs(120), rx)
            .await
            .context("浏览器响应超时")?
            .context("浏览器进程已退出")?;
        if let Some(e) = resp.get("error") {
            bail!("{}", e["message"].as_str().unwrap_or("未知错误"));
        }
        Ok(resp["result"].clone())
    }

    fn kill(&self) {
        // npx 会再拉起 node 子进程，要连同子进程一起结束
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .creation_flags_no_window()
                .output();
        }
    }
}

pub struct Browser {
    package: String,
    idle: Duration,
    session: Mutex<Option<Session>>,
    last_used: std::sync::Mutex<Instant>,
}

impl Browser {
    pub fn new(package: &str, idle_minutes: u64) -> Arc<Self> {
        let browser = Arc::new(Self {
            package: package.to_string(),
            idle: Duration::from_secs(idle_minutes.max(1) * 60),
            session: Mutex::new(None),
            last_used: std::sync::Mutex::new(Instant::now()),
        });
        let weak: Weak<Self> = Arc::downgrade(&browser);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(b) = weak.upgrade() else { break };
                if b.last_used.lock().unwrap().elapsed() > b.idle {
                    if let Some(s) = b.session.lock().await.take() {
                        s.kill();
                    }
                }
            }
        });
        browser
    }

    pub async fn call(&self, name: &str, args: &Value) -> (String, bool) {
        *self.last_used.lock().unwrap() = Instant::now();
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            match self.start().await {
                Ok(s) => *guard = Some(s),
                Err(e) => return (format!("启动浏览器控制失败：{e}"), true),
            }
        }
        let session = guard.as_mut().unwrap();
        let result = session.request("tools/call", json!({"name": name, "arguments": args})).await;
        *self.last_used.lock().unwrap() = Instant::now();
        match result {
            Ok(v) => convert(&v),
            Err(e) => {
                if let Some(s) = guard.take() {
                    s.kill();
                }
                (format!("浏览器操作失败：{e}"), true)
            }
        }
    }

    async fn start(&self) -> anyhow::Result<Session> {
        let out_dir = config::app_dir().join("playwright");
        let mut child = Command::new("cmd")
            .args(["/c", "npx", "-y", &self.package, "--browser", "msedge", "--codegen", "none", "--output-dir"])
            .arg(&out_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .kill_on_drop(true)
            .spawn()
            .context("需要安装 Node.js 才能控制浏览器")?;
        let stdin = child.stdin.take().context("无法连接浏览器进程")?;
        let stdout = child.stdout.take().context("无法连接浏览器进程")?;
        let pending: Pending = Default::default();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = v["id"].as_u64() {
                    if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(v);
                    }
                }
            }
        });
        let mut session = Session { child, stdin, pending, next_id: 1 };
        session
            .request(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "voice-assistant", "version": "0.1"}}),
            )
            .await?;
        session.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
        Ok(session)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.session.try_lock() {
            if let Some(s) = guard.take() {
                s.kill();
            }
        }
    }
}

/// MCP 结果转成文字；截图换成说明（模型默认不看图，省 token、更快）
fn convert(result: &Value) -> (String, bool) {
    let mut text = String::new();
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("text") => text.push_str(item["text"].as_str().unwrap_or("")),
            Some("image") => text.push_str("（截图已省略，请用 browser_snapshot 读取页面结构）"),
            _ => {}
        }
        text.push('\n');
    }
    if text.chars().count() > MAX_RESULT_CHARS {
        text = text.chars().take(MAX_RESULT_CHARS).collect::<String>() + "\n…（内容过长，已截断）";
    }
    if text.trim().is_empty() {
        text = "（无输出）".into();
    }
    (text, result["isError"].as_bool().unwrap_or(false))
}
