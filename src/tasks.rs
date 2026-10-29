//! 后台任务：复杂的事交给外部智能体（Codex / Claude Code）或内置的 DeepSeek 去做，前台照常对话。
//! 每个任务一个子进程，读它的 JSON 事件流显示进度；做完后把结果发给调度中心播报。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::Agent;
use crate::assistant::{Role, UiEvent, UiSink};
use crate::config::{self, Config, NoWindow};
use crate::llm::Llm;
use crate::tools::Tools;

/// 交给后台智能体的说明。它是无人值守运行的，问不了用户，所以不可撤销的事要留给用户确认
const TASK_PROMPT: &str = "你是用户 Windows 电脑上语音助手的后台执行者。用户是用语音下达的任务，下面是转述。

要求：
- 直接动手完成，不要向用户提问（这是无人值守运行，没人能回答）；信息不够时按最合理的理解去做，并在总结里说明你的假设。
- 操作桌面软件（启动/关闭程序、读窗口里的控件、点击、输入、快捷键）和网页（browser_ 开头的工具，在独立的 Edge 窗口里打开、点击、填写、读取）优先用 voice_desktop 提供的工具；它们比在命令行里绕路可靠。
- 只是要让用户看到某个网站的搜索结果时，用 web_search（在用户自己的浏览器里打开，带着他的登录状态）。
- 删除文件、发送消息或邮件、付款下单、提交表单这类不可撤销的操作不要做，做到这一步前停下，在总结里说清楚需要用户确认什么。
- 完成后检查目标是否达成，说明实际结果；未完成、部分完成或需要确认时如实说明，不能把尝试过当成做完。
- 最后一条消息用一两句口语化的中文总结结果，它会被直接朗读出来：不要用 Markdown、列表、代码块，也不要念完整路径或网址。

任务：";

#[derive(Clone, Copy, PartialEq)]
enum State {
    Running,
    Done,
    Failed,
    Cancelled,
}

struct TaskInfo {
    id: u32,
    title: String,
    description: String,
    result: String,
    state: State,
    progress: String,
    started: Instant,
    /// 外部进程的 pid，取消时连同子进程一起结束
    pid: Option<u32>,
    /// 内置智能体跑在异步任务里，取消时直接中止
    handle: Option<tokio::task::AbortHandle>,
}

/// 任务结束后发给调度中心
pub struct TaskDone {
    pub id: u32,
    pub ok: bool,
    pub text: String,
}

/// 启动任务时需要的设置，保存设置后由调度中心更新
#[derive(Clone)]
pub struct TaskSettings {
    pub executor: String,
    pub access: String,
    pub workdir: PathBuf,
    pub model: String,
    /// 内置执行者用：大模型参数（没有 key 时为 None）和全套工具
    pub builtin: Option<(Config, String)>,
    pub full_tools: Option<Arc<Tools>>,
}

impl TaskSettings {
    pub fn from_config(cfg: &Config, full_tools: Arc<Tools>) -> Self {
        Self {
            executor: cfg.executor.clone(),
            access: cfg.agent_access.clone(),
            workdir: cfg.agent_dir(),
            model: cfg.agent_model.trim().to_string(),
            builtin: config::llm_key(cfg).map(|k| (cfg.clone(), k)),
            full_tools: Some(full_tools),
        }
    }
}

pub fn executor_name(executor: &str) -> &'static str {
    match executor {
        "claude" => "Claude Code",
        "builtin" => "内置助手",
        _ => "Codex",
    }
}

pub struct TaskManager {
    tasks: Mutex<Vec<TaskInfo>>,
    settings: Mutex<Option<TaskSettings>>,
    done_tx: UnboundedSender<TaskDone>,
    ui: UiSink,
}

impl TaskManager {
    pub fn new(done_tx: UnboundedSender<TaskDone>, ui: UiSink) -> Arc<Self> {
        Arc::new(Self { tasks: Mutex::new(Vec::new()), settings: Mutex::new(None), done_tx, ui })
    }

    pub fn update_settings(&self, s: TaskSettings) {
        *self.settings.lock().unwrap() = Some(s);
    }

    /// 开始一个后台任务，立即返回给前台的说明文字
    pub fn start(self: &Arc<Self>, description: &str) -> (String, bool) {
        let description = description.trim();
        if description.is_empty() {
            return ("任务内容为空".into(), true);
        }
        let Some(settings) = self.settings.lock().unwrap().clone() else {
            return ("后台智能体还没准备好".into(), true);
        };
        let name = executor_name(&settings.executor);
        let id = {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(active) = tasks.iter().find(|t| t.state == State::Running) {
                return (format!("任务 {} 正在执行，为避免争抢鼠标和窗口，请等它结束或先取消。", active.id), true);
            }
            let id = tasks.last().map_or(1, |t| t.id + 1);
            tasks.push(TaskInfo {
                id,
                title: description.chars().take(40).collect(),
                description: description.to_string(),
                result: String::new(),
                state: State::Running,
                progress: "正在启动".into(),
                started: Instant::now(),
                pid: None,
                handle: None,
            });
            id
        };
        (self.ui)(UiEvent::Chat(Role::Tool, format!("任务 #{id} 交给 {name} 在后台处理：{description}")));
        (self.ui)(UiEvent::TaskRunning(true));
        let me = self.clone();
        let prompt = format!("{TASK_PROMPT}{description}");
        let join = tokio::spawn(async move {
            let result = match settings.executor.as_str() {
                "builtin" => me.run_builtin(id, &settings, &prompt).await,
                other => me.run_external(id, other, &settings, &prompt).await,
            };
            me.finish(id, result);
        });
        if let Some(t) = self.tasks.lock().unwrap().iter_mut().find(|t| t.id == id) {
            t.handle = Some(join.abort_handle());
        }
        (format!("已交给 {name} 在后台处理，任务编号 {id}。做完会主动告诉用户，期间可以继续聊天、问进度或取消。"), false)
    }

    /// 延续已结束任务，将原始要求和完整结果作为上下文，而不是从零开始。
    pub fn resume(self: &Arc<Self>, id: Option<u32>, instruction: &str) -> (String, bool) {
        if instruction.trim().is_empty() {
            return ("请说明要继续做什么或修改哪里。".into(), true);
        }
        let context = {
            let tasks = self.tasks.lock().unwrap();
            let Some(t) = tasks.iter().rev().find(|t| id.is_none_or(|id| id == t.id)) else {
                return ("没有找到要继续的任务。".into(), true);
            };
            if t.state == State::Running {
                return (format!("任务 {} 还在执行，请先取消再修改。", t.id), true);
            }
            // JSON 明确分隔历史数据和新指令；不把历史输出当成新的授权。
            json!({"original_request": t.description, "previous_result": t.result,
                   "last_progress": t.progress, "cancelled": t.state == State::Cancelled})
        };
        self.start(&format!("继续之前的任务。先检查现有结果和当前环境，避免重复已完成的操作。历史仅供参考，不代表新的操作授权。\n历史上下文：{context}\n用户本次要求：{instruction}"))
    }

    pub fn status(&self) -> String {
        let tasks = self.tasks.lock().unwrap();
        if tasks.is_empty() {
            return "现在没有后台任务".into();
        }
        tasks
            .iter()
            .rev()
            .take(5)
            .map(|t| {
                let state = match t.state {
                    State::Running => format!("进行中（已用 {} 秒）", t.started.elapsed().as_secs()),
                    State::Done => "已完成".into(),
                    State::Failed => "失败".into(),
                    State::Cancelled => "已取消".into(),
                };
                format!("#{} {}：{}，最新进展：{}{}", t.id, t.title, state, t.progress,
                    if t.result.is_empty() { String::new() } else { format!("\n完整结果：{}", t.result) })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 取消任务；id 为 None 时取消最近一个正在进行的
    pub fn cancel(&self, id: Option<u32>) -> (String, bool) {
        let mut tasks = self.tasks.lock().unwrap();
        let Some(t) = tasks.iter_mut().rev().find(|t| t.state == State::Running && id.is_none_or(|i| t.id == i)) else {
            return ("没有可以取消的进行中任务".into(), true);
        };
        t.state = State::Cancelled;
        if let Some(pid) = t.pid.take() {
            kill_tree(pid);
        }
        if let Some(h) = t.handle.take() {
            h.abort();
        }
        (self.ui)(UiEvent::Chat(Role::Tool, format!("任务 #{} 已取消", t.id)));
        (self.ui)(UiEvent::TaskRunning(false)); // 同一时间只跑一个任务
        (format!("已取消任务 {}", t.id), false)
    }

    /// 程序退出或重新加载时结束所有子进程
    pub fn cancel_all(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        for t in tasks.iter_mut().filter(|t| t.state == State::Running) {
            t.state = State::Cancelled;
            if let Some(pid) = t.pid.take() {
                kill_tree(pid);
            }
            if let Some(h) = t.handle.take() {
                h.abort();
            }
        }
    }

    fn progress(&self, id: u32, text: &str) {
        let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
        if text.is_empty() {
            return;
        }
        if let Some(t) = self.tasks.lock().unwrap().iter_mut().find(|t| t.id == id) {
            t.progress = text.clone();
        }
        (self.ui)(UiEvent::TaskProgress(id, text));
    }

    fn finish(&self, id: u32, result: anyhow::Result<String>) {
        let mut tasks = self.tasks.lock().unwrap();
        let Some(t) = tasks.iter_mut().find(|t| t.id == id) else { return };
        if t.state != State::Running {
            return; // 已经被取消
        }
        t.pid = None;
        t.handle = None;
        let (ok, text) = match result {
            Ok(text) => (true, text),
            Err(e) => (false, format!("{e:#}")),
        };
        t.state = if ok { State::Done } else { State::Failed };
        t.progress = text.chars().take(80).collect();
        t.result = text.clone();
        (self.ui)(UiEvent::TaskRunning(false));
        let _ = self.done_tx.send(TaskDone { id, ok, text });
    }

    async fn run_builtin(&self, id: u32, s: &TaskSettings, prompt: &str) -> anyhow::Result<String> {
        let (cfg, key) = s.builtin.clone().ok_or_else(|| anyhow::anyhow!("内置助手需要先在设置里填 DeepSeek 的 API Key"))?;
        let tools = s.full_tools.clone().ok_or_else(|| anyhow::anyhow!("工具没有准备好"))?;
        let llm = Llm::new(&cfg.llm_base_url, key, &cfg.llm_model, cfg.llm_thinking, &cfg.llm_effort)?;
        let mut agent = Agent::backend(llm, tools);
        agent.ask(prompt, |name, input| self.progress(id, &crate::assistant::describe_tool(name, input))).await
    }

    async fn run_external(&self, id: u32, executor: &str, s: &TaskSettings, prompt: &str) -> anyhow::Result<String> {
        let claude = executor == "claude";
        let program = if claude { find_program(&["claude.exe", "claude.cmd"]) } else { find_program(&["codex.exe", "codex.cmd"]) };
        let program = program.ok_or_else(|| {
            anyhow::anyhow!(
                "没有找到 {} 命令行，请先安装（npm install -g {}）",
                executor_name(executor),
                if claude { "@anthropic-ai/claude-code" } else { "@openai/codex" }
            )
        })?;
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(external_args(claude, s)?)
            .current_dir(&s.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .creation_flags_no_window();
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("启动 {} 失败：{e}", executor_name(executor)))?;
        if let Some(t) = self.tasks.lock().unwrap().iter_mut().find(|t| t.id == id) {
            t.pid = child.id();
        }
        // 任务内容从标准输入传进去，不拼进命令行（避免引号、换行和命令注入问题）
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            drop(stdin);
        }
        let stderr = child.stderr.take();
        let stderr_tail = tokio::spawn(async move {
            let mut tail = Vec::new();
            if let Some(err) = stderr {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tail.push(line);
                    if tail.len() > 5 {
                        tail.remove(0);
                    }
                }
            }
            tail.join(" ")
        });

        let mut last_message = String::new();
        let mut error: Option<String> = None;
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("读取输出失败"))?;
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
            let parsed = if claude { parse_claude(&event) } else { parse_codex(&event) };
            match parsed {
                Parsed::Progress(text) => self.progress(id, &text),
                Parsed::Message(text) => {
                    self.progress(id, &text);
                    last_message = text;
                }
                Parsed::Final(text) => last_message = text,
                Parsed::Error(text) => error = Some(text),
                Parsed::Nothing => {}
            }
        }
        let status = child.wait().await?;
        let tail = stderr_tail.await.unwrap_or_default();
        validate_external_result(executor_name(executor), status.success(), status.code(), &last_message, error.as_deref(), &tail)
    }
}

/// 进程退出成功和有效结果缺一不可；中途的进度文字不能掩盖异常退出。
fn validate_external_result(name: &str, success: bool, code: Option<i32>, text: &str, error: Option<&str>, tail: &str) -> anyhow::Result<String> {
    if let Some(error) = error {
        anyhow::bail!("{name} 出错：{}", error.chars().take(200).collect::<String>());
    }
    if !success {
        anyhow::bail!("{name} 异常退出（退出码 {}），任务未确认完成。{}", code.unwrap_or(-1), tail.chars().take(200).collect::<String>());
    }
    if text.trim().is_empty() {
        anyhow::bail!("{name} 没有给出执行结果，任务未确认完成。{}", tail.chars().take(200).collect::<String>());
    }
    Ok(text.trim().to_string())
}

enum Parsed {
    Progress(String),
    /// 智能体说的话；最后一条就是结果
    Message(String),
    Final(String),
    Error(String),
    Nothing,
}

/// codex exec --json 的事件：thread.started / turn.started / item.started / item.completed / turn.completed / turn.failed / error
fn parse_codex(e: &Value) -> Parsed {
    let item = &e["item"];
    match (e["type"].as_str().unwrap_or(""), item["type"].as_str().unwrap_or("")) {
        ("item.started", "command_execution") => Parsed::Progress(format!("执行命令：{}", item["command"].as_str().unwrap_or(""))),
        ("item.started", "mcp_tool_call") => Parsed::Progress(format!("使用工具：{}", item["tool"].as_str().unwrap_or(""))),
        ("item.started", "web_search") => Parsed::Progress(format!("搜索：{}", item["query"].as_str().unwrap_or(""))),
        ("item.completed", "agent_message") => Parsed::Message(item["text"].as_str().unwrap_or("").to_string()),
        ("turn.failed", _) => Parsed::Error(e["error"]["message"].as_str().unwrap_or("任务失败").to_string()),
        ("error", _) => Parsed::Error(e["message"].as_str().unwrap_or("未知错误").to_string()),
        _ => Parsed::Nothing,
    }
}

/// claude -p --output-format stream-json 的事件：system / assistant / user / result
fn parse_claude(e: &Value) -> Parsed {
    match e["type"].as_str().unwrap_or("") {
        "assistant" => {
            let content = e["message"]["content"].as_array().cloned().unwrap_or_default();
            if let Some(tool) = content.iter().find(|c| c["type"] == "tool_use") {
                return Parsed::Progress(format!("使用工具：{}", tool["name"].as_str().unwrap_or("")));
            }
            let text: String = content.iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
            if text.trim().is_empty() { Parsed::Nothing } else { Parsed::Message(text) }
        }
        "result" => {
            let text = e["result"].as_str().unwrap_or("").to_string();
            if e["is_error"].as_bool() == Some(true) {
                Parsed::Error(if text.contains("Not logged in") { "Claude Code 还没登录：请在终端运行 claude 并登录".into() } else { text })
            } else {
                Parsed::Final(text)
            }
        }
        _ => Parsed::Nothing,
    }
}

/// 外部智能体的命令行参数。都不含用户说的话（从标准输入传），也不含双引号
fn external_args(claude: bool, s: &TaskSettings) -> anyhow::Result<Vec<String>> {
    let exe = std::env::current_exe()?.display().to_string();
    let mut args: Vec<String> = Vec::new();
    if claude {
        // MCP 配置写到文件里，免得在命令行里转义 JSON
        let mcp_file = config::app_dir().join("agent-mcp.json");
        std::fs::create_dir_all(config::app_dir())?;
        std::fs::write(&mcp_file, json!({"mcpServers": {"voice_desktop": {"command": exe, "args": ["--mcp"]}}}).to_string())?;
        args.extend(["-p", "--output-format", "stream-json", "--verbose", "--no-session-persistence", "--mcp-config"].map(String::from));
        args.push(mcp_file.display().to_string());
        match s.access.as_str() {
            "full" => args.push("--dangerously-skip-permissions".into()),
            "workspace" => args.extend(["--permission-mode", "acceptEdits", "--allowedTools", "mcp__voice_desktop"].map(String::from)),
            _ => args.extend(["--allowedTools", "Read", "Glob", "Grep", "WebSearch", "WebFetch"].map(String::from)),
        }
    } else {
        args.extend(["exec", "--json", "--skip-git-repo-check", "--ephemeral", "--color", "never"].map(String::from));
        // TOML 字面字符串用单引号，Windows 路径里的反斜杠不用转义
        args.push("-c".into());
        args.push(format!("mcp_servers.voice_desktop.command='{exe}'"));
        args.push("-c".into());
        args.push("mcp_servers.voice_desktop.args=['--mcp']".into());
        match s.access.as_str() {
            "full" => args.push("--dangerously-bypass-approvals-and-sandbox".into()),
            "workspace" => args.extend(["-s", "workspace-write"].map(String::from)),
            _ => args.extend(["-s", "read-only"].map(String::from)),
        }
    }
    if !s.model.is_empty() {
        args.push(if claude { "--model" } else { "-m" }.into());
        args.push(s.model.clone());
    }
    if !claude {
        args.push("-".into()); // 从标准输入读任务
    }
    Ok(args)
}

/// 在 PATH 和 npm 全局目录里找命令
pub fn find_program(names: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        dirs.push(PathBuf::from(home).join(".local").join("bin"));
    }
    names.iter().find_map(|n| dirs.iter().map(|d| d.join(n)).find(|p| p.is_file()))
}

fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).creation_flags_no_window().output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_failed_process_even_with_a_message() {
        assert!(validate_external_result("test", false, Some(1), "正在处理", None, "connection lost").is_err());
        assert!(validate_external_result("test", true, Some(0), " ", None, "").is_err());
        assert!(validate_external_result("test", true, Some(0), "完成", Some("failed"), "").is_err());
        assert_eq!(validate_external_result("test", true, Some(0), "  结果  ", None, "").unwrap(), "结果");
    }

    #[tokio::test]
    async fn preserves_result_and_continues_with_context() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let manager = TaskManager::new(tx, Arc::new(|_| {}));
        manager.update_settings(TaskSettings {
            executor: "builtin".into(), access: "workspace".into(), workdir: PathBuf::from("."),
            model: String::new(), builtin: None, full_tools: None,
        });
        manager.tasks.lock().unwrap().push(TaskInfo {
            id: 1, title: "原任务".into(), description: "整理报告".into(), result: String::new(),
            state: State::Running, progress: "正在处理".into(), started: Instant::now(), pid: None, handle: None,
        });
        assert!(manager.start("另一个任务").1);
        assert!(manager.resume(Some(1), "修改第二项").1);
        let full_result = "完整报告内容".repeat(30);
        manager.finish(1, Ok(full_result.clone()));
        assert!(rx.recv().await.unwrap().ok);
        assert!(manager.status().contains(&full_result));
        assert!(manager.resume(Some(999), "继续").1);
        assert!(!manager.resume(Some(1), "修改第二项").1);
        {
            let tasks = manager.tasks.lock().unwrap();
            let continued = tasks.last().unwrap();
            assert!(continued.description.contains("整理报告"));
            assert!(continued.description.contains(&full_result));
            assert!(continued.description.contains("修改第二项"));
        }
        // 未配置 key 的内置执行者应明确失败，不能报完成。
        assert!(!rx.recv().await.unwrap().ok);
    }

    #[test]
    fn parses_agent_events() {
        let codex = |s: &str| parse_codex(&serde_json::from_str(s).unwrap());
        assert!(matches!(codex(r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"好的"}}"#), Parsed::Message(t) if t == "好的"));
        assert!(matches!(codex(r#"{"type":"item.started","item":{"type":"command_execution","command":"dir"}}"#), Parsed::Progress(_)));
        assert!(matches!(codex(r#"{"type":"turn.failed","error":{"message":"x"}}"#), Parsed::Error(_)));
        let claude = |s: &str| parse_claude(&serde_json::from_str(s).unwrap());
        assert!(matches!(claude(r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#), Parsed::Error(t) if t.contains("登录")));
        assert!(matches!(claude(r#"{"type":"result","is_error":false,"result":"做完了"}"#), Parsed::Final(t) if t == "做完了"));
        assert!(matches!(claude(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#), Parsed::Progress(_)));
    }

    #[test]
    fn args_never_contain_double_quotes() {
        let s = TaskSettings {
            executor: "codex".into(),
            access: "full".into(),
            workdir: PathBuf::from("."),
            model: String::new(),
            builtin: None,
            full_tools: None,
        };
        for claude in [false, true] {
            let args = external_args(claude, &s).unwrap();
            assert!(args.iter().all(|a| !a.contains('"')), "{args:?}");
        }
    }
}
