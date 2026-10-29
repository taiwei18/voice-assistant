//! 本地工具：启动程序（开始菜单）、默认浏览器搜索/打开网址、时间、音量和播放控制。

use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{self, NoWindow};
use crate::sys::{self, MediaKey};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct App {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "AppID")]
    pub id: String,
}

pub enum Match {
    One(App),
    Many(Vec<App>),
    None,
}

/// 开始菜单应用索引：先读缓存立即可用，后台再用 Get-StartApps 刷新
pub struct AppIndex {
    apps: RwLock<Vec<App>>,
}

impl AppIndex {
    pub fn load_cached() -> Self {
        let apps = std::fs::read_to_string(config::app_dir().join("apps.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { apps: RwLock::new(apps) }
    }

    /// 调用 PowerShell 的 Get-StartApps 重新扫描（约 1~2 秒，放后台线程跑）
    pub fn refresh(&self) {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-StartApps | ConvertTo-Json -Compress",
            ])
            .creation_flags_no_window()
            .output();
        let Ok(out) = out else { return };
        let Ok(mut apps) = serde_json::from_slice::<Vec<App>>(&out.stdout) else { return };
        apps.retain(|a| !a.name.contains("卸载") && !a.name.to_lowercase().contains("uninstall"));
        let _ = std::fs::create_dir_all(config::app_dir());
        let _ = std::fs::write(config::app_dir().join("apps.json"), serde_json::to_string(&apps).unwrap_or_default());
        *self.apps.write().unwrap() = apps;
    }

    fn search(&self, keyword: &str) -> Vec<App> {
        let norm = |s: &str| s.to_lowercase().replace(' ', "");
        let k = norm(keyword);
        if k.is_empty() {
            return Vec::new();
        }
        let apps = self.apps.read().unwrap();
        let (mut exact, mut prefix, mut contains) = (Vec::new(), Vec::new(), Vec::new());
        for app in apps.iter() {
            let n = norm(&app.name);
            if n == k {
                exact.push(app.clone());
            } else if n.starts_with(&k) {
                prefix.push(app.clone());
            } else if n.contains(&k) {
                contains.push(app.clone());
            }
        }
        exact.into_iter().chain(prefix).chain(contains).collect()
    }

    /// 完全同名或只有一个候选时才算确定
    pub fn best(&self, name: &str) -> Match {
        let hits = self.search(name);
        let lower = name.to_lowercase().replace(' ', "");
        if let Some(app) = hits.iter().find(|a| a.name.to_lowercase().replace(' ', "") == lower) {
            return Match::One(app.clone());
        }
        match hits.len() {
            0 => Match::None,
            1 => Match::One(hits[0].clone()),
            _ => Match::Many(hits),
        }
    }
}

pub fn launch(app: &App) -> bool {
    std::process::Command::new("explorer.exe").arg(format!("shell:AppsFolder\\{}", app.id)).spawn().is_ok()
}

pub fn search_url(query: &str, engine: &str) -> String {
    let q: String = query
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    match engine {
        "baidu" => format!("https://www.baidu.com/s?wd={q}"),
        "google" => format!("https://www.google.com/search?q={q}"),
        "bilibili" => format!("https://search.bilibili.com/all?keyword={q}"),
        "zhihu" => format!("https://www.zhihu.com/search?q={q}"),
        "douyin" => format!("https://www.douyin.com/search/{q}"),
        "xiaohongshu" => format!("https://www.xiaohongshu.com/search_result?keyword={q}"),
        "taobao" => format!("https://s.taobao.com/search?q={q}"),
        "jd" => format!("https://search.jd.com/Search?keyword={q}"),
        "youtube" => format!("https://www.youtube.com/results?search_query={q}"),
        "github" => format!("https://github.com/search?q={q}"),
        _ => format!("https://www.bing.com/search?q={q}"),
    }
}

/// 口语里的网站名 → web_search 的 engine
pub fn site_engine(name: &str) -> Option<&'static str> {
    let n = name.trim().to_lowercase().replace(' ', "");
    Some(match n.as_str() {
        "抖音" | "douyin" => "douyin",
        "b站" | "哔哩哔哩" | "bilibili" | "b 站" => "bilibili",
        "小红书" => "xiaohongshu",
        "淘宝" => "taobao",
        "京东" => "jd",
        "知乎" => "zhihu",
        "百度" => "baidu",
        "必应" | "bing" => "bing",
        "谷歌" | "google" => "google",
        "youtube" | "油管" => "youtube",
        "github" => "github",
        _ => return None,
    })
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "launch_app",
            "description": "启动本机已安装的应用（来自开始菜单），例如 记事本、计算器、微信、Microsoft Edge。名称不确定时先用 find_apps。",
            "input_schema": {"type": "object", "properties": {"name": {"type": "string", "description": "应用名称，尽量用开始菜单里的完整名称"}}, "required": ["name"]}
        }),
        json!({
            "name": "find_apps",
            "description": "按关键词模糊查找本机已安装的应用名称。",
            "input_schema": {"type": "object", "properties": {"keyword": {"type": "string"}}, "required": ["keyword"]}
        }),
        json!({
            "name": "web_search",
            "description": "在用户自己的默认浏览器里（带着用户的登录状态）打开某个网站的搜索结果页，一步完成，读不到页面内容。用户说“在抖音/B站/小红书/淘宝/京东/知乎/YouTube/GitHub 上搜 X”或“打开抖音搜 X”时用对应的 engine；只说“搜一下 X”用 bing。",
            "input_schema": {"type": "object", "properties": {
                "query": {"type": "string"},
                "engine": {"type": "string", "enum": ["bing", "baidu", "google", "bilibili", "zhihu", "douyin", "xiaohongshu", "taobao", "jd", "youtube", "github"]}
            }, "required": ["query"]}
        }),
        json!({
            "name": "open_url",
            "description": "在用户的默认浏览器里打开网址，读不到页面内容。",
            "input_schema": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]}
        }),
        json!({
            "name": "current_time",
            "description": "获取当前本地日期、星期和时间。",
            "input_schema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "media_control",
            "description": "调节系统音量或控制媒体播放。",
            "input_schema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["volume_up", "volume_down", "mute", "play_pause", "next", "previous"]},
                "times": {"type": "integer", "description": "按几次，音量每次约 2%，默认 5"}
            }, "required": ["action"]}
        }),
    ]
}

/// 执行本地工具；不是本地工具时返回 None
pub fn call(index: &AppIndex, name: &str, input: &Value) -> Option<(String, bool)> {
    let s = |k: &str| input[k].as_str().unwrap_or("").trim().to_string();
    Some(match name {
        "launch_app" => match index.best(&s("name")) {
            Match::One(app) if launch(&app) => (format!("已启动：{}", app.name), false),
            Match::One(app) => (format!("启动失败：{}", app.name), true),
            Match::Many(apps) => (
                format!("有多个候选，请用完整名称再调用：{}", apps.iter().take(10).map(|a| a.name.as_str()).collect::<Vec<_>>().join("、")),
                true,
            ),
            Match::None => (format!("没有找到应用“{}”，可以先用 find_apps 换个关键词查找。", s("name")), true),
        },
        "find_apps" => {
            let hits = index.search(&s("keyword"));
            if hits.is_empty() {
                (format!("没有找到包含“{}”的应用", s("keyword")), false)
            } else {
                (format!("匹配到：{}", hits.iter().take(15).map(|a| a.name.as_str()).collect::<Vec<_>>().join("、")), false)
            }
        }
        "web_search" => {
            let engine = input["engine"].as_str().unwrap_or("bing");
            let ok = sys::shell_open(&search_url(&s("query"), engine));
            (if ok { format!("已在默认浏览器搜索：{}", s("query")) } else { "打开浏览器失败".into() }, !ok)
        }
        "open_url" => {
            let mut url = s("url");
            if !url.starts_with("http://") && !url.starts_with("https://") {
                url = format!("https://{url}");
            }
            let ok = sys::shell_open(&url);
            (if ok { format!("已打开：{url}") } else { "打开浏览器失败".into() }, !ok)
        }
        "current_time" => (sys::now_text(), false),
        "media_control" => {
            let key = match input["action"].as_str().unwrap_or("") {
                "volume_up" => MediaKey::VolumeUp,
                "volume_down" => MediaKey::VolumeDown,
                "mute" => MediaKey::Mute,
                "play_pause" => MediaKey::PlayPause,
                "next" => MediaKey::Next,
                "previous" => MediaKey::Previous,
                other => return Some((format!("未知操作：{other}"), true)),
            };
            let times = input["times"].as_u64().unwrap_or(if matches!(key, MediaKey::VolumeUp | MediaKey::VolumeDown) { 5 } else { 1 });
            sys::press_media_key(key, times.min(50) as u32);
            ("已完成".into(), false)
        }
        _ => return None,
    })
}
