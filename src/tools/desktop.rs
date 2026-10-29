//! 桌面控制：直接调用 Windows UI Automation（读取控件树、点击、输入、快捷键）。
//! UIA 的 COM 对象不能跨线程，所以放在一个专用线程里，通过通道收发请求。

use std::sync::mpsc;

use serde_json::{Value, json};
use tokio::sync::oneshot;
use uiautomation::inputs::{Keyboard, Mouse};
use uiautomation::types::{ControlType, Point};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

const MAX_ELEMENTS: usize = 150;
const MAX_VISITED: usize = 3000;
const OWN_WINDOW: &str = "VOA"; // 和主窗口标题一致

type Job = (String, Value, oneshot::Sender<(String, bool)>);

pub struct Desktop {
    tx: mpsc::Sender<Job>,
}

impl Desktop {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("desktop".into())
            .spawn(move || {
                let mut state = match UIAutomation::new() {
                    Ok(automation) => Some(State { automation, elements: Vec::new() }),
                    Err(_) => None,
                };
                for (name, input, reply) in rx {
                    let result = match state.as_mut() {
                        Some(s) => s.handle(&name, &input),
                        None => Err("UI Automation 初始化失败".into()),
                    };
                    let _ = reply.send(match result {
                        Ok(text) => (text, false),
                        Err(text) => (text, true),
                    });
                }
            })
            .expect("启动桌面控制线程失败");
        Self { tx }
    }

    pub async fn call(&self, name: &str, input: &Value) -> (String, bool) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.tx.send((name.to_string(), input.clone(), reply_tx)).is_err() {
            return ("桌面控制线程已退出".into(), true);
        }
        reply_rx.await.unwrap_or(("桌面控制没有返回结果".into(), true))
    }
}

struct State {
    automation: UIAutomation,
    /// 最近一次快照里编号的控件，供 click/type 按编号操作
    elements: Vec<UIElement>,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("操作失败：{e}")
}

impl State {
    fn handle(&mut self, name: &str, input: &Value) -> Result<String, String> {
        let s = |k: &str| input[k].as_str().unwrap_or("").trim().to_string();
        match name {
            "desktop_windows" => {
                let names: Vec<String> = self.top_windows()?.iter().filter_map(|w| w.get_name().ok()).collect();
                Ok(format!("已打开的窗口：\n{}", names.join("\n")))
            }
            "desktop_focus" => {
                let win = self.find_window(&s("name"))?;
                win.set_focus().map_err(err)?;
                std::thread::sleep(std::time::Duration::from_millis(300));
                Ok(format!("已切换到：{}", win.get_name().unwrap_or_default()))
            }
            "desktop_snapshot" => self.snapshot(&s("window")),
            "desktop_click" => {
                let double = input["double"].as_bool().unwrap_or(false);
                if let Some(id) = input["id"].as_u64() {
                    let el = self.element(id)?;
                    if double {
                        let r = el.get_bounding_rectangle().map_err(err)?;
                        let p = Point::new((r.get_left() + r.get_right()) / 2, (r.get_top() + r.get_bottom()) / 2);
                        Mouse::new().double_click(&p).map_err(err)?;
                    } else {
                        el.click().map_err(err)?;
                    }
                    Ok(format!("已点击 [{id}]"))
                } else if let (Some(x), Some(y)) = (input["x"].as_i64(), input["y"].as_i64()) {
                    let p = Point::new(x as i32, y as i32);
                    if double { Mouse::new().double_click(&p) } else { Mouse::new().click(&p) }.map_err(err)?;
                    Ok(format!("已点击 ({x},{y})"))
                } else {
                    Err("需要 id 或 x/y".into())
                }
            }
            "desktop_type" => {
                if let Some(id) = input["id"].as_u64() {
                    self.element(id)?.click().map_err(err)?;
                }
                Keyboard::new().interval(5).send_text(&s("text")).map_err(err)?;
                Ok("已输入".into())
            }
            "desktop_keys" => {
                let keys = to_uia_keys(&s("keys"));
                Keyboard::new().interval(30).send_keys(&keys).map_err(err)?;
                Ok(format!("已按下 {}", s("keys")))
            }
            other => Err(format!("未知工具：{other}")),
        }
    }

    fn element(&self, id: u64) -> Result<&UIElement, String> {
        self.elements
            .get((id as usize).wrapping_sub(1))
            .ok_or_else(|| format!("没有编号 [{id}] 的控件，请先调用 desktop_snapshot"))
    }

    fn children(walker: &UITreeWalker, el: &UIElement) -> Vec<UIElement> {
        let mut out = Vec::new();
        if let Ok(mut child) = walker.get_first_child(el) {
            loop {
                out.push(child.clone());
                match walker.get_next_sibling(&child) {
                    Ok(next) => child = next,
                    Err(_) => break,
                }
            }
        }
        out
    }

    fn top_windows(&self) -> Result<Vec<UIElement>, String> {
        let root = self.automation.get_root_element().map_err(err)?;
        let walker = self.automation.get_control_view_walker().map_err(err)?;
        Ok(Self::children(&walker, &root)
            .into_iter()
            .filter(|w| w.get_name().is_ok_and(|n| !n.trim().is_empty()) && !w.is_offscreen().unwrap_or(true))
            .collect())
    }

    fn find_window(&self, name: &str) -> Result<UIElement, String> {
        let needle = name.to_lowercase();
        self.top_windows()?
            .into_iter()
            .find(|w| w.get_name().is_ok_and(|n| n.to_lowercase().contains(&needle)))
            .ok_or_else(|| format!("没有找到标题包含“{name}”的窗口，可以先用 desktop_windows 查看"))
    }

    fn foreground(&self) -> Result<UIElement, String> {
        let hwnd = unsafe { GetForegroundWindow() };
        let win = self.automation.element_from_handle(hwnd.into()).map_err(err)?;
        if win.get_name().is_ok_and(|n| n == OWN_WINDOW) {
            return Err("当前前台是助手自己的窗口，请先用 desktop_focus 或 launch_app 切到目标窗口".into());
        }
        Ok(win)
    }

    fn snapshot(&mut self, window: &str) -> Result<String, String> {
        let win = if window.is_empty() { self.foreground()? } else { self.find_window(window)? };
        let walker = self.automation.get_control_view_walker().map_err(err)?;
        self.elements.clear();
        let mut lines = vec![format!("窗口：{}", win.get_name().unwrap_or_default())];
        let mut stack = vec![(win, 0usize)];
        let mut visited = 0;
        while let Some((el, depth)) = stack.pop() {
            visited += 1;
            if visited > MAX_VISITED || self.elements.len() >= MAX_ELEMENTS {
                lines.push("……（控件太多，已截断）".into());
                break;
            }
            if depth > 0 {
                if let Some(line) = describe(&el) {
                    self.elements.push(el.clone());
                    lines.push(format!("[{}] {line}", self.elements.len()));
                }
            }
            if depth < 30 {
                for child in Self::children(&walker, &el).into_iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
        }
        Ok(lines.join("\n"))
    }
}

/// 把可交互或有文字的控件描述成一行：类型 "名称" (中心坐标)
fn describe(el: &UIElement) -> Option<String> {
    if el.is_offscreen().unwrap_or(false) {
        return None;
    }
    let ct = el.get_control_type().ok()?;
    let name = el.get_name().unwrap_or_default();
    let interactive = matches!(
        ct,
        ControlType::Button
            | ControlType::Edit
            | ControlType::CheckBox
            | ControlType::RadioButton
            | ControlType::ComboBox
            | ControlType::ListItem
            | ControlType::MenuItem
            | ControlType::TabItem
            | ControlType::Hyperlink
            | ControlType::TreeItem
            | ControlType::SplitButton
            | ControlType::Slider
            | ControlType::Document
    );
    if !interactive && !(ct == ControlType::Text && !name.trim().is_empty()) {
        return None;
    }
    let r = el.get_bounding_rectangle().ok()?;
    if r.get_width() <= 0 || r.get_height() <= 0 {
        return None;
    }
    let label: String = name.chars().take(60).collect();
    Some(format!(
        "{ct:?} \"{label}\" ({},{})",
        (r.get_left() + r.get_right()) / 2,
        (r.get_top() + r.get_bottom()) / 2
    ))
}

/// "ctrl+shift+t" -> "{ctrl}{shift}(t)"，"alt+f4" -> "{alt}{F4}"；已经是 {..} 格式的原样返回
fn to_uia_keys(keys: &str) -> String {
    if keys.contains('{') {
        return keys.to_string();
    }
    if !keys.contains('+') {
        return if is_named_key(keys) { format!("{{{keys}}}") } else { keys.to_string() };
    }
    let parts: Vec<&str> = keys.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let Some((last, mods)) = parts.split_last() else { return String::new() };
    let mut out: String = mods.iter().map(|m| format!("{{{m}}}")).collect();
    if last.chars().count() == 1 {
        out.push_str(&format!("({last})"));
    } else {
        out.push_str(&format!("{{{last}}}"));
    }
    out
}

fn is_named_key(k: &str) -> bool {
    let k = k.to_uppercase();
    matches!(k.as_str(), "ENTER" | "TAB" | "ESC" | "ESCAPE" | "SPACE" | "BACKSPACE" | "DELETE" | "DEL" | "HOME" | "END"
        | "UP" | "DOWN" | "LEFT" | "RIGHT" | "PAGEUP" | "PAGEDOWN" | "PGUP" | "PGDN")
        || (k.starts_with('F') && k[1..].parse::<u8>().is_ok())
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "desktop_windows",
            "description": "列出当前打开的窗口标题。",
            "input_schema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "desktop_focus",
            "description": "把标题包含 name 的窗口切到前台。注意窗口标题可能是英文（如记事本叫 Notepad）。",
            "input_schema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}
        }),
        json!({
            "name": "desktop_snapshot",
            "description": "读取窗口里的控件列表（编号、类型、名称、中心坐标）。不填 window 时读取前台窗口。操作前先调用它拿到编号。",
            "input_schema": {"type": "object", "properties": {"window": {"type": "string", "description": "窗口标题的一部分，可选"}}}
        }),
        json!({
            "name": "desktop_click",
            "description": "点击控件：优先用 desktop_snapshot 里的编号 id，也可以给屏幕坐标 x/y。",
            "input_schema": {"type": "object", "properties": {
                "id": {"type": "integer"}, "x": {"type": "integer"}, "y": {"type": "integer"},
                "double": {"type": "boolean", "description": "是否双击"}
            }}
        }),
        json!({
            "name": "desktop_type",
            "description": "输入文字（支持中文）。给 id 时先点击该输入框再输入，否则输入到当前焦点。",
            "input_schema": {"type": "object", "properties": {"text": {"type": "string"}, "id": {"type": "integer"}}, "required": ["text"]}
        }),
        json!({
            "name": "desktop_keys",
            "description": "按快捷键或特殊键，例如 ctrl+s、ctrl+shift+t、alt+f4、enter、esc、win+d。",
            "input_schema": {"type": "object", "properties": {"keys": {"type": "string"}}, "required": ["keys"]}
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::to_uia_keys;

    #[test]
    fn converts_shortcuts() {
        assert_eq!(to_uia_keys("ctrl+s"), "{ctrl}(s)");
        assert_eq!(to_uia_keys("ctrl+shift+t"), "{ctrl}{shift}(t)");
        assert_eq!(to_uia_keys("alt+f4"), "{alt}{f4}");
        assert_eq!(to_uia_keys("enter"), "{enter}");
        assert_eq!(to_uia_keys("{ctrl}(a)"), "{ctrl}(a)");
    }
}
