//! 配置：保存在 %APPDATA%\VoiceAssistant\config.json，全部可以在界面“设置”页修改。
//! API key 用 Windows DPAPI 加密后保存，只有当前 Windows 用户能解密；也兼容系统环境变量。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // 唤醒
    pub wake_words: Vec<String>,
    /// 唤醒词检测阈值，越小越灵敏（也越容易误唤醒）
    pub kws_threshold: f32,
    /// 唤醒词加权，越大越容易被检出
    pub kws_score: f32,
    /// 麦克风名称（包含即可），空字符串表示系统默认
    pub input_device: String,
    pub listen_on_start: bool,
    pub show_window_on_start: bool,
    /// 回复后继续听几秒，不用再喊唤醒词
    pub follow_up: bool,

    // 云端流式识别（阿里云百炼 fun-asr-realtime）
    pub asr_url: String,
    pub asr_model: String,
    /// DPAPI 加密后的 key（十六进制）
    pub asr_api_key_protected: String,
    /// 没在界面填 key 时，从这个环境变量读取
    pub asr_key_env: String,
    /// 说完多久算一句话结束（毫秒）
    pub asr_sentence_silence_ms: u32,
    /// 唤醒后多久没开口就放弃（秒）
    pub asr_speech_timeout_secs: u64,

    // 大模型（Anthropic Messages 格式，默认 DeepSeek）
    pub llm_base_url: String,
    pub llm_api_key_protected: String,
    pub llm_key_env: String,
    pub llm_model: String,
    pub llm_thinking: bool,
    pub llm_effort: String,
    pub new_conversation_after_secs: u64,

    // 后台智能体：复杂任务交给谁做
    /// codex / claude / builtin（内置 DeepSeek + 桌面/浏览器工具）
    pub executor: String,
    /// full（完全访问）/ workspace（只能改工作目录）/ readonly（只读）
    pub agent_access: String,
    /// 后台智能体的工作目录，空 = 用户目录
    pub agent_workdir: String,
    /// 后台智能体用的模型，空 = 它自己的默认设置
    pub agent_model: String,

    // 语音对话
    /// realtime（实时语音模型，边说边回、可打断）/ classic（识别 + 大模型 + 系统播报）
    pub voice_frontend: String,
    pub realtime_url: String,
    pub realtime_model: String,
    pub realtime_voice: String,
    /// 播报时说唤醒词可以打断
    pub barge_in: bool,
    /// 戴耳机时可以开：播报时麦克风也一直开着，直接说话就能打断（外放会把自己的声音当成用户在说话）
    pub full_duplex: bool,

    // 工具
    pub desktop_control: bool,
    pub browser_control: bool,
    pub browser_idle_minutes: u64,
    pub playwright_mcp: String,

    // 播报与外观
    pub tts_rate: i32,
    /// auto（跟随系统）/ light / dark
    pub theme: String,

    // 悬浮球
    pub float_ball: bool,
    /// 全局快捷键 Ctrl+Alt+空格：开始说话 / 停止
    pub hotkey: bool,
    /// 悬浮球上次的位置（物理像素）；i32::MIN 表示用默认位置
    pub ball_x: i32,
    pub ball_y: i32,

    /// 配置格式版本，用于一次性迁移旧配置（旧文件里没有这个字段，按 0 处理）
    #[serde(default = "legacy_version")]
    pub config_version: u32,
}

const CONFIG_VERSION: u32 = 2;

fn legacy_version() -> u32 {
    0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 四个音节的唤醒词最不容易误触发；“小助手”这类三字词会被“小猪手套”误唤醒
            wake_words: vec!["你好电脑".into()],
            kws_threshold: 0.25,
            kws_score: 1.5,
            input_device: String::new(),
            listen_on_start: true,
            // 默认启动后只显示悬浮球，点开才出现主界面
            show_window_on_start: false,
            follow_up: true,
            asr_url: "wss://dashscope.aliyuncs.com/api-ws/v1/inference".into(),
            asr_model: "fun-asr-realtime".into(),
            asr_api_key_protected: String::new(),
            asr_key_env: "DASHSCOPE_API_KEY".into(),
            asr_sentence_silence_ms: 800,
            asr_speech_timeout_secs: 6,
            llm_base_url: "https://api.deepseek.com/anthropic".into(),
            llm_api_key_protected: String::new(),
            llm_key_env: "DEEPSEEK_API_KEY".into(),
            llm_model: "deepseek-flash".into(),
            llm_thinking: true,
            llm_effort: "low".into(),
            new_conversation_after_secs: 300,
            executor: "codex".into(),
            agent_access: "full".into(),
            agent_workdir: String::new(),
            agent_model: String::new(),
            voice_frontend: "realtime".into(),
            realtime_url: "wss://dashscope.aliyuncs.com/api-ws/v1/realtime".into(),
            realtime_model: "qwen-audio-3.1-realtime-plus".into(),
            realtime_voice: "longanqian_v3.1".into(),
            barge_in: true,
            full_duplex: false,
            desktop_control: true,
            browser_control: true,
            browser_idle_minutes: 5,
            playwright_mcp: "@playwright/mcp@0.0.83".into(),
            tts_rate: 1,
            theme: "auto".into(),
            float_ball: true,
            hotkey: true,
            ball_x: i32::MIN,
            ball_y: i32::MIN,
            config_version: CONFIG_VERSION,
        }
    }
}

pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("VoiceAssistant")
}

fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

pub fn load() -> Config {
    let mut cfg: Config = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if cfg.config_version < CONFIG_VERSION {
        // v2 起默认只显示悬浮球：旧配置里“启动时显示窗口”默认是开的，迁移时关掉
        cfg.show_window_on_start = false;
        cfg.config_version = CONFIG_VERSION;
        let _ = save(&cfg);
    }
    cfg
}

impl Config {
    pub fn ball_position(&self) -> Option<(i32, i32)> {
        (self.ball_x != i32::MIN && self.ball_y != i32::MIN).then_some((self.ball_x, self.ball_y))
    }

    pub fn agent_dir(&self) -> PathBuf {
        let dir = self.agent_workdir.trim();
        if dir.is_empty() {
            std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
        } else {
            PathBuf::from(dir)
        }
    }
}

pub fn save(cfg: &Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(app_dir())?;
    std::fs::write(config_path(), serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

/// 模型目录：优先环境变量 VA_MODELS，其次 exe 旁边的 models\，最后当前目录的 models\
pub fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VA_MODELS") {
        return PathBuf::from(dir);
    }
    let beside_exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("models")));
    match beside_exe {
        Some(dir) if dir.exists() => dir,
        _ => PathBuf::from("models"),
    }
}

// ---------- API key ----------

/// DPAPI 加密/解密（绑定当前 Windows 用户）
fn dpapi(data: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        let result = if encrypt {
            CryptProtectData(&input, windows::core::w!("VoiceAssistant"), None, None, None, 0, &mut output)
        } else {
            CryptUnprotectData(&input, None, None, None, None, 0, &mut output)
        };
        result.ok()?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData as _)));
        Some(bytes)
    }
}

/// 加密 key，返回可以写进配置文件的十六进制字符串；失败返回 None
pub fn protect_key(plain: &str) -> Option<String> {
    dpapi(plain.trim().as_bytes(), true).map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
}

fn unprotect_key(stored: &str) -> Option<String> {
    if stored.is_empty() || stored.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..stored.len()).step_by(2).map(|i| u8::from_str_radix(&stored[i..i + 2], 16).ok()).collect();
    String::from_utf8(dpapi(&bytes?, false)?).ok().filter(|k| !k.is_empty())
}

/// 读取环境变量里的 key；从注册表兜底读取用户级变量（setx 设置后不用重启已打开的程序）
fn env_key(env_name: &str) -> Option<String> {
    if env_name.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(env_name) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    let out = std::process::Command::new("reg")
        .args(["query", r"HKCU\Environment", "/v", env_name])
        .creation_flags_no_window()
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| l.trim_start().starts_with(env_name))
        .and_then(|l| l.split_whitespace().last())
        .map(str::to_string)
}

/// 优先用界面里保存的 key，其次环境变量
pub fn llm_key(cfg: &Config) -> Option<String> {
    unprotect_key(&cfg.llm_api_key_protected).or_else(|| env_key(&cfg.llm_key_env))
}

pub fn asr_key(cfg: &Config) -> Option<String> {
    unprotect_key(&cfg.asr_api_key_protected).or_else(|| env_key(&cfg.asr_key_env))
}

/// 给界面显示的 key 状态（不显示 key 本身）
/// 界面里保存过 key（不算环境变量）
pub fn has_saved_key(protected: &str) -> bool {
    unprotect_key(protected).is_some()
}

pub fn key_status(protected: &str, env_name: &str) -> String {
    if unprotect_key(protected).is_some() {
        "已保存（加密存放在本机）".into()
    } else if env_key(env_name).is_some() {
        format!("正在使用系统环境变量 {env_name}")
    } else {
        "未设置".into()
    }
}

pub trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000) // CREATE_NO_WINDOW：不弹黑框
    }
}

impl NoWindow for tokio::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        self.creation_flags(0x0800_0000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        let stored = protect_key("  sk-test-123  ").unwrap();
        assert!(!stored.contains("sk-test")); // 文件里看不到明文
        assert_eq!(unprotect_key(&stored).as_deref(), Some("sk-test-123"));
        assert_eq!(unprotect_key("zz"), None);
        assert_eq!(unprotect_key(""), None);
    }
}
