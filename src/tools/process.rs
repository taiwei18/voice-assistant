//! 关闭程序：按应用名或进程名找到正在运行的程序，先正常关闭窗口，缩到托盘/后台的再结束进程。
//! 以前大模型只能去开任务管理器、在命令行里敲 taskkill，二十多步还关不掉；这里一步完成。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    WM_CLOSE,
};
use windows::core::{BOOL, PWSTR};

use super::apps::{AppIndex, Match};

/// 系统和桌面外壳的进程：结束了会黑屏、掉线甚至蓝屏，一律不碰
const PROTECTED: &[&str] = &[
    "system", "registry", "smss", "csrss", "wininit", "winlogon", "services", "lsass", "lsaiso", "svchost", "dwm",
    "explorer", "fontdrvhost", "sihost", "ctfmon", "taskhostw", "runtimebroker", "searchhost", "searchindexer",
    "startmenuexperiencehost", "shellexperiencehost", "textinputhost", "conhost", "audiodg", "spoolsv",
    "securityhealthservice", "msmpeng",
];

struct Proc {
    pid: u32,
    /// 进程文件名，如 ChatGPT.exe
    exe: String,
    /// 完整路径；系统进程读不到时为空
    path: String,
}

struct Win {
    hwnd: HWND,
    pid: u32,
    title: String,
    /// 被别的窗口拥有（对话框之类），关闭时不直接发给它
    owned: bool,
}

fn norm(s: &str) -> String {
    let s = s.trim().to_lowercase().replace(' ', "");
    s.strip_suffix(".exe").map(str::to_string).unwrap_or(s)
}

fn stem(exe: &str) -> String {
    norm(exe)
}

fn wide_str(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn processes() -> Vec<Proc> {
    let me = unsafe { GetCurrentProcessId() };
    let mut out = Vec::new();
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return out };
    let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        let pid = entry.th32ProcessID;
        if pid > 4 && pid != me {
            out.push(Proc { pid, exe: wide_str(&entry.szExeFile), path: image_path(pid) });
        }
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    unsafe {
        let _ = CloseHandle(snap);
    }
    out
}

fn image_path(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if ok { String::from_utf16_lossy(&buf[..len as usize]) } else { String::new() }
    }
}

/// 所有可见、有标题的顶层窗口
fn windows() -> Vec<Win> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<Win>) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut buf = [0u16; 256];
            let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
            if n > 0 {
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
                list.push(Win { hwnd, pid, title: String::from_utf16_lossy(&buf[..n as usize]), owned });
            }
        }
        true.into()
    }
    let mut list: Vec<Win> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut Vec<Win> as isize));
    }
    list
}

fn alive(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else { return false };
        let running = WaitForSingleObject(h, 0) != WAIT_OBJECT_0;
        let _ = CloseHandle(h);
        running
    }
}

fn protected(p: &Proc) -> bool {
    PROTECTED.contains(&stem(&p.exe).as_str())
}

/// 开始菜单里的应用 → 判断某个进程是不是它：普通程序比较 exe 文件名，商店应用比较安装包目录
fn app_matcher(apps: &AppIndex, keyword: &str, strict: bool) -> Option<Box<dyn Fn(&Proc) -> bool>> {
    let app = match apps.best(keyword) {
        Match::One(app) if !strict || norm(&app.name) == norm(keyword) => app,
        _ => return None,
    };
    let id = app.id.to_lowercase();
    if let Some((package, _)) = id.split_once('!') {
        // OpenAI.Codex_2p2nqsd0c76g0!App → 路径里有 \WindowsApps\OpenAI.Codex_…__2p2nqsd0c76g0\
        let (name, publisher) = package.rsplit_once('_')?;
        let (dir, tail) = (format!("\\windowsapps\\{name}_"), format!("__{publisher}\\"));
        return Some(Box::new(move |p| {
            let path = p.path.to_lowercase();
            path.contains(&dir) && path.contains(&tail)
        }));
    }
    if id.ends_with(".exe") {
        let file = id.rsplit(['\\', '/']).next().unwrap_or(&id).to_string();
        return Some(Box::new(move |p| p.exe.to_lowercase() == file));
    }
    None
}

/// 找到要关的进程，按 exe 分组。strict：只认完全同名（直达规则用，避免“关闭电脑”关掉“电脑管家”）
fn find(keyword: &str, apps: &AppIndex, strict: bool) -> BTreeMap<String, Vec<Proc>> {
    let k = norm(keyword);
    let mut groups: BTreeMap<String, Vec<Proc>> = BTreeMap::new();
    if k.is_empty() {
        return groups;
    }
    let by_app = app_matcher(apps, keyword, strict);
    for p in processes() {
        if protected(&p) {
            continue;
        }
        let s = stem(&p.exe);
        let hit = if strict { s == k } else { s.contains(&k) };
        if hit || by_app.as_ref().is_some_and(|m| m(&p)) {
            groups.entry(p.exe.to_lowercase()).or_default().push(p);
        }
    }
    // 有一个进程名和关键词完全一样时，只关它
    if groups.len() > 1 {
        if let Some(exact) = groups.keys().find(|e| stem(e) == k).cloned() {
            groups.retain(|e, _| *e == exact);
        }
    }
    groups
}

pub enum Closed {
    NotFound,
    Ambiguous(Vec<String>),
    Done(String),
}

/// 关闭程序：先给它的窗口发“关闭”，正常退出最好；窗口都没了但进程还在（缩到托盘/后台）就结束进程；
/// 如果还有窗口（多半在问“是否保存”），不强行结束，交给用户处理
pub fn close(keyword: &str, apps: &AppIndex, strict: bool) -> Closed {
    let groups = find(keyword, apps, strict);
    if groups.is_empty() {
        return Closed::NotFound;
    }
    if groups.len() > 1 {
        return Closed::Ambiguous(groups.values().map(|g| g[0].exe.clone()).collect());
    }
    let procs = groups.into_values().next().unwrap_or_default();
    let exe = procs[0].exe.clone();
    let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
    let mine = |w: &Win| pids.contains(&w.pid);

    let main_windows: Vec<Win> = windows().into_iter().filter(|w| mine(w) && !w.owned).collect();
    for w in &main_windows {
        unsafe {
            let _ = PostMessageW(Some(w.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    if !main_windows.is_empty() {
        // 等它自己退出；窗口没了 1.5 秒进程还在，就当是缩到了后台
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(100));
            if pids.iter().all(|&p| !alive(p)) {
                break;
            }
            if start.elapsed() > Duration::from_millis(1500) && !windows().iter().any(mine) {
                break;
            }
        }
    }

    let remaining: Vec<u32> = pids.iter().copied().filter(|&p| alive(p)).collect();
    if remaining.is_empty() {
        return Closed::Done(format!("已关闭 {exe}"));
    }
    if let Some(w) = windows().into_iter().find(|w| remaining.contains(&w.pid)) {
        return Closed::Done(format!(
            "已请求关闭 {exe}，但它还有窗口“{}”（可能在问是否保存），没有强行结束，请用户自己处理",
            w.title
        ));
    }
    let mut denied = 0;
    for &pid in &remaining {
        let ok = unsafe {
            match OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) {
                Ok(h) => {
                    // TerminateProcess 是异步的，等它真正退出再报告结果
                    let r = TerminateProcess(h, 1).is_ok() && WaitForSingleObject(h, 2000) == WAIT_OBJECT_0;
                    let _ = CloseHandle(h);
                    r
                }
                Err(_) => false,
            }
        };
        if !ok {
            denied += 1;
        }
    }
    if denied == remaining.len() {
        Closed::Done(format!("{exe} 结束失败，可能需要管理员权限"))
    } else if denied > 0 {
        Closed::Done(format!("已结束 {exe} 的后台进程，其中 {denied} 个没有权限结束"))
    } else {
        Closed::Done(format!("已关闭 {exe}（包括后台进程，共 {} 个）", pids.len()))
    }
}

/// 列出正在运行的程序（按 exe 合并），给大模型确认进程名用
fn list(keyword: &str, apps: &AppIndex) -> String {
    let wins = windows();
    let groups: BTreeMap<String, Vec<Proc>> = if keyword.trim().is_empty() {
        // 不给关键词时只列用户自己的程序：能读到路径、且不在 Windows 目录里
        let mut g: BTreeMap<String, Vec<Proc>> = BTreeMap::new();
        for p in processes() {
            let path = p.path.to_lowercase();
            if !protected(&p) && !path.is_empty() && !path.starts_with("c:\\windows\\") {
                g.entry(p.exe.to_lowercase()).or_default().push(p);
            }
        }
        g
    } else {
        find(keyword, apps, false)
    };
    if groups.is_empty() {
        return format!("没有找到正在运行的“{keyword}”");
    }
    let lines: Vec<String> = groups
        .values()
        .take(60)
        .map(|g| {
            let titles: Vec<&str> =
                wins.iter().filter(|w| g.iter().any(|p| p.pid == w.pid)).map(|w| w.title.as_str()).take(2).collect();
            let state = if titles.is_empty() { "后台运行，没有窗口".to_string() } else { format!("窗口：{}", titles.join(" / ")) };
            format!("{} ×{}（{}）", g[0].exe, g.len(), state)
        })
        .collect();
    lines.join("\n")
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "close_app",
            "description": "关闭正在运行的程序，包括缩到托盘或在后台运行的程序。name 可以是开始菜单里的应用名（如 微信、ChatGPT）或进程名（如 ChatGPT.exe）。会先正常关闭窗口，程序还在后台就结束进程；如果程序弹出“是否保存”之类的窗口，会停下来交给用户。系统进程不会被关闭。",
            "input_schema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}
        }),
        json!({
            "name": "list_processes",
            "description": "列出正在运行的程序（按进程名合并，带窗口标题；没有窗口的是后台运行）。keyword 可选，用来按应用名或进程名过滤。",
            "input_schema": {"type": "object", "properties": {"keyword": {"type": "string"}}}
        }),
    ]
}

/// 执行进程工具；不是进程工具时返回 None。会等待程序退出，最多几秒，请在阻塞线程里调用
pub fn call(apps: &AppIndex, name: &str, input: &Value) -> Option<(String, bool)> {
    let s = |k: &str| input[k].as_str().unwrap_or("").trim().to_string();
    Some(match name {
        "close_app" => match close(&s("name"), apps, false) {
            Closed::Done(text) => (text, false),
            Closed::NotFound => (format!("没有找到正在运行的“{}”，可以用 list_processes 看看进程名", s("name")), true),
            Closed::Ambiguous(list) => (format!("匹配到多个程序，请用完整进程名再调用：{}", list.join("、")), true),
        },
        "list_processes" => (list(&s("keyword"), apps), false),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_system_processes() {
        let p = |exe: &str| Proc { pid: 1, exe: exe.into(), path: String::new() };
        assert!(protected(&p("explorer.exe")));
        assert!(protected(&p("SVCHOST.EXE")));
        assert!(!protected(&p("ChatGPT.exe")));
        assert_eq!(norm(" ChatGPT.exe "), "chatgpt");
    }

    /// 起三个假程序实际关一遍：后台无窗口的、普通窗口的、关闭时“不肯关”（模拟问是否保存）的。
    /// 会创建和结束进程，默认不跑：cargo test -- --ignored
    #[test]
    #[ignore]
    fn closes_real_processes() {
        use std::os::windows::process::CommandExt;
        let dir = std::env::temp_dir().join("va_close_test");
        std::fs::create_dir_all(&dir).unwrap();
        let system32 = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32");
        let copy = |from: &str, to: &str| {
            let dst = dir.join(to);
            std::fs::copy(system32.join(from), &dst).unwrap();
            dst
        };
        let bg = copy("PING.EXE", "vaBgDummy.exe");
        let ps = system32.join(r"WindowsPowerShell\v1.0\powershell.exe");
        let win = dir.join("vaWinDummy.exe");
        let ask = dir.join("vaAskDummy.exe");
        std::fs::copy(&ps, &win).unwrap();
        std::fs::copy(&ps, &ask).unwrap();
        let form = |title: &str, refuse: bool| {
            let cancel = if refuse { "$f.add_FormClosing({ $_.Cancel = $true; $f.Text = '是否保存？' });" } else { "" };
            format!("Add-Type -A System.Windows.Forms; $f = New-Object Windows.Forms.Form; $f.Text = '{title}'; {cancel} [void]$f.ShowDialog()")
        };
        let spawn = |exe: &std::path::Path, args: &[&str]| {
            std::process::Command::new(exe)
                .args(args)
                .creation_flags(0x0800_0000) // 不弹控制台
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut children = vec![
            spawn(&bg, &["-n", "300", "127.0.0.1"]),
            spawn(&win, &["-NoProfile", "-Command", &form("vaWinDummy", false)]),
            spawn(&ask, &["-NoProfile", "-Command", &form("vaAskDummy", true)]),
        ];
        std::thread::sleep(Duration::from_secs(4)); // 等窗口出来
        let apps = AppIndex::load_cached();

        let started = Instant::now();
        let r = close("vaBgDummy", &apps, true);
        println!("后台程序：{} 用时 {:?}", done_text(&r), started.elapsed());
        assert!(done_text(&r).starts_with("已关闭"));
        assert!(!alive(children[0].id()));

        let started = Instant::now();
        let r = close("vawindummy", &apps, false);
        println!("普通窗口：{} 用时 {:?}", done_text(&r), started.elapsed());
        assert!(done_text(&r).starts_with("已关闭"));
        assert!(!alive(children[1].id()));

        let r = close("vaAskDummy.exe", &apps, false);
        println!("不肯关的：{}", done_text(&r));
        assert!(done_text(&r).contains("没有强行结束"));
        assert!(alive(children[2].id()), "有窗口在问是否保存时不能强行结束");

        assert!(matches!(close("vaNoSuchDummy", &apps, false), Closed::NotFound));
        assert!(matches!(close("explorer", &apps, false), Closed::NotFound), "系统进程不能关");
        println!("列表：{}", list("vaAskDummy", &apps));

        let _ = children[2].kill();
        for c in &mut children {
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(test)]
    fn done_text(r: &Closed) -> String {
        match r {
            Closed::Done(t) => t.clone(),
            Closed::NotFound => "没找到".into(),
            Closed::Ambiguous(l) => format!("多个：{}", l.join("、")),
        }
    }
}
