//! Windows 系统调用的小封装：单实例、内存占用、提示音、时间、媒体键、打开网址、开机自启。

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::Diagnostics::Debug::Beep;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_MEDIA_NEXT_TRACK,
    VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

/// 只允许运行一个实例（两个实例会同时抢麦克风）
pub fn single_instance() -> bool {
    unsafe {
        match CreateMutexW(None, true, w!("Local\\VoiceAssistantSingleInstance")) {
            Ok(_handle) => GetLastError() != ERROR_ALREADY_EXISTS, // 句柄不关闭，进程退出时自动释放
            Err(_) => true,
        }
    }
}

/// 本进程的私有内存（MB），即任务管理器里“内存”一列
pub fn private_memory_mb() -> f64 {
    unsafe {
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
        let ok = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        );
        if ok.is_ok() { pmc.PrivateUsage as f64 / 1024.0 / 1024.0 } else { 0.0 }
    }
}

pub fn beep(freq: u32, ms: u32) {
    std::thread::spawn(move || unsafe {
        let _ = Beep(freq, ms);
    });
}

pub fn now_text() -> String {
    let t = unsafe { GetLocalTime() };
    let weekday = ["日", "一", "二", "三", "四", "五", "六"][t.wDayOfWeek as usize % 7];
    format!("{}年{}月{}日 星期{} {:02}:{:02}", t.wYear, t.wMonth, t.wDay, weekday, t.wHour, t.wMinute)
}

#[derive(Clone, Copy, Debug)]
pub enum MediaKey {
    VolumeUp,
    VolumeDown,
    Mute,
    PlayPause,
    Next,
    Previous,
}

pub fn press_media_key(key: MediaKey, times: u32) {
    let vk: VIRTUAL_KEY = match key {
        MediaKey::VolumeUp => VK_VOLUME_UP,
        MediaKey::VolumeDown => VK_VOLUME_DOWN,
        MediaKey::Mute => VK_VOLUME_MUTE,
        MediaKey::PlayPause => VK_MEDIA_PLAY_PAUSE,
        MediaKey::Next => VK_MEDIA_NEXT_TRACK,
        MediaKey::Previous => VK_MEDIA_PREV_TRACK,
    };
    let make = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } },
    };
    for _ in 0..times {
        let inputs = [make(Default::default()), make(KEYEVENTF_KEYUP)];
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    }
}

/// 用默认程序打开网址或文件
pub fn shell_open(target: &str) -> bool {
    let r = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(target), None, None, SW_SHOWNORMAL) };
    r.0 as isize > 32
}

// ---------- 开机自启：HKCU\Software\Microsoft\Windows\CurrentVersion\Run ----------

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: windows::core::PCWSTR = w!("VoiceAssistant");

/// 开机自启时执行的命令：当前 exe 路径 + --autostart（自启时只显示悬浮球，不弹主界面）
fn autostart_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --autostart", exe.display()))
}

fn open_run_key(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut key).ok().ok()? };
    Some(key)
}

/// 注册表里登记的自启命令（没有则 None）
fn autostart_value() -> Option<String> {
    let key = open_run_key(KEY_QUERY_VALUE)?;
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegQueryValueExW(key, RUN_VALUE, None, None, Some(buf.as_mut_ptr() as *mut u8), Some(&mut len)).is_ok()
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok.then(|| String::from_utf16_lossy(&buf[..(len as usize / 2)]).trim_end_matches('\0').to_string())
}

pub fn autostart_enabled() -> bool {
    autostart_value().is_some()
}

pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    let command = autostart_command().ok_or_else(|| anyhow::anyhow!("找不到程序路径"))?;
    let key = open_run_key(KEY_SET_VALUE).ok_or_else(|| anyhow::anyhow!("打开注册表启动项失败"))?;
    let result = unsafe {
        if on {
            let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes)).ok()
        } else {
            let r = RegDeleteValueW(key, RUN_VALUE);
            if r == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND { Ok(()) } else { r.ok() }
        }
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    Ok(result?)
}

/// 程序挪了位置后，自启项里的路径会失效：启动时如果已开启自启，就更新成当前路径
pub fn refresh_autostart_path() {
    if let (Some(current), Some(wanted)) = (autostart_value(), autostart_command()) {
        if current != wanted {
            let _ = set_autostart(true);
        }
    }
}
