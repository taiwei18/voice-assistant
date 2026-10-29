//! 工具注册表。两套：
//! - 前台：一步就能完成的本地工具 + 后台任务（开始 / 进度 / 取消），给对话用，要快；
//! - 全套：本地工具 + 桌面控制 + 浏览器控制，给内置后台助手用，也通过 MCP 提供给 Codex / Claude Code。

pub mod apps;
pub mod browser;
pub mod desktop;
pub mod process;

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::Config;
use crate::tasks::TaskManager;

pub struct Tools {
    pub apps: Arc<apps::AppIndex>,
    desktop: Option<desktop::Desktop>,
    browser: Option<Arc<browser::Browser>>,
    tasks: Option<Arc<TaskManager>>,
}

impl Tools {
    /// 全套工具
    pub fn new(cfg: &Config, apps: Arc<apps::AppIndex>) -> Self {
        Self {
            apps,
            desktop: cfg.desktop_control.then(desktop::Desktop::new),
            browser: cfg.browser_control.then(|| browser::Browser::new(&cfg.playwright_mcp, cfg.browser_idle_minutes)),
            tasks: None,
        }
    }

    /// 前台工具：快速本地工具 + 后台任务
    pub fn front(apps: Arc<apps::AppIndex>, tasks: Arc<TaskManager>) -> Self {
        Self { apps, desktop: None, browser: None, tasks: Some(tasks) }
    }

    /// 只要桌面相关的工具（MCP 模式给外部智能体用，浏览器它们自己有）
    pub fn desktop_only(apps: Arc<apps::AppIndex>) -> Self {
        Self { apps, desktop: Some(desktop::Desktop::new()), browser: None, tasks: None }
    }

    pub fn definitions(&self) -> Vec<Value> {
        let mut defs = apps::definitions();
        defs.extend(process::definitions());
        if self.tasks.is_some() {
            defs.extend(task_definitions());
        }
        if self.desktop.is_some() {
            defs.extend(desktop::definitions());
        }
        if self.browser.is_some() {
            defs.extend(browser::definitions());
        }
        defs
    }

    /// 执行工具，返回（结果文字，是否出错）
    pub async fn call(&self, name: &str, input: &Value) -> (String, bool) {
        if let Some(tasks) = &self.tasks {
            match name {
                "start_task" => return tasks.start(input["task"].as_str().unwrap_or("")),
                "task_status" => return (tasks.status(), false),
                "cancel_task" => return tasks.cancel(input["id"].as_u64().map(|i| i as u32)),
                _ => {}
            }
        }
        if let Some(out) = apps::call(&self.apps, name, input) {
            return out;
        }
        if matches!(name, "close_app" | "list_processes") {
            // 关闭程序要等它退出（最多几秒），放到阻塞线程里
            let (apps, name, input) = (self.apps.clone(), name.to_string(), input.clone());
            return tokio::task::spawn_blocking(move || process::call(&apps, &name, &input))
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| ("进程工具执行失败".into(), true));
        }
        if name.starts_with("desktop_") {
            if let Some(d) = &self.desktop {
                return d.call(name, input).await;
            }
        }
        if name.starts_with("browser_") {
            if let Some(b) = &self.browser {
                return b.call(name, input).await;
            }
        }
        (format!("未知或未启用的工具：{name}"), true)
    }
}

fn task_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "start_task",
            "description": "把需要多步操作的任务交给后台智能体去做（操作软件或网页、查资料并总结、读写文件、整理东西等），立即返回，不用等它做完。task 要写成完整、独立能看懂的说明，包含用户的原话要点。",
            "input_schema": {"type": "object", "properties": {"task": {"type": "string"}}, "required": ["task"]}
        }),
        json!({
            "name": "task_status",
            "description": "查看后台任务的进度。",
            "input_schema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "cancel_task",
            "description": "取消后台任务。不给 id 时取消最近一个正在进行的任务。",
            "input_schema": {"type": "object", "properties": {"id": {"type": "integer"}}}
        }),
    ]
}
