//! 常用命令直达：打开/关闭应用、搜索、问时间、调音量、切歌，不经过大模型。
//! 只有在很确定的时候才处理，否则返回 None 交给大模型。

use std::sync::LazyLock;

use regex::Regex;

use crate::sys::{self, MediaKey};
use crate::tools::apps::{self, AppIndex, Match};
use crate::tools::process::{self, Closed};

static OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:打开|启动|运行|开一下|开)(?:一下)?(.+?)(?:一下|吧)?$").unwrap());
// “打开抖音搜索无畏契约直播”“在B站上搜一下原神”“淘宝搜机械键盘”
static SITE_SEARCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:打开|在|去|用|到)?(.{1,8}?)(?:上面|里面|上|里|中)?[，, ]*(?:然后)?(?:搜索|搜一下|搜一搜|搜|查找|找一下|找)(.+)$").unwrap()
});
// “关闭微信”“关掉 GPT 的后台”“把网易云音乐关了”
static CLOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:关闭|关掉|关了|退出|结束)(?:一下)?(.+?)(?:的?(?:后台|进程|程序))?吧?$").unwrap());
static CLOSE_BA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^把(.+?)(?:的?(?:后台|进程|程序))?给?(?:关闭|关掉|关了|退出|结束掉?)吧?$").unwrap()
});
static SEARCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:搜索|搜一下|搜一搜|百度一下|百度|必应一下)(?:一下)?(.+)$").unwrap());
static TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:现在)?(?:几点了?|什么时间|几号|今天几号|今天星期几|星期几)$").unwrap());

fn normalize(text: &str) -> String {
    let mut t = text.trim().trim_matches(|c: char| "。.！!？?，, 、~".contains(c)).to_string();
    for prefix in ["请你", "请", "帮我", "麻烦你", "麻烦", "给我"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            t = rest.to_string();
        }
    }
    t.trim().to_string()
}

pub fn try_handle(text: &str, apps: &AppIndex) -> Option<String> {
    let t = normalize(text);
    if t.is_empty() || t.chars().count() > 20 {
        return None;
    }

    if TIME.is_match(&t) {
        return Some(format!("现在是{}。", sys::now_text()));
    }

    let media = match t.as_str() {
        "大声点" | "大声一点" | "音量调大" | "音量调大一点" | "声音大一点" | "调大音量" => Some((MediaKey::VolumeUp, 5, "音量已调大。")),
        "小声点" | "小声一点" | "音量调小" | "音量调小一点" | "声音小一点" | "调小音量" => Some((MediaKey::VolumeDown, 5, "音量已调小。")),
        "静音" | "取消静音" => Some((MediaKey::Mute, 1, "好的。")),
        "暂停" | "暂停播放" | "继续播放" | "播放" => Some((MediaKey::PlayPause, 1, "好的。")),
        "下一首" | "下一曲" | "切歌" => Some((MediaKey::Next, 1, "下一首。")),
        "上一首" | "上一曲" => Some((MediaKey::Previous, 1, "上一首。")),
        _ => None,
    };
    if let Some((key, times, reply)) = media {
        sys::press_media_key(key, times);
        return Some(reply.into());
    }

    // 要在“打开 XX”之前判断，否则“打开抖音搜索 XX”会被当成打开一个叫“抖音搜索 XX”的程序
    if let Some(c) = SITE_SEARCH.captures(&t) {
        if let Some(engine) = apps::site_engine(&c[1]) {
            let query = c[2].trim().trim_start_matches(['一', '下']).trim();
            if !query.is_empty() && sys::shell_open(&apps::search_url(query, engine)) {
                return Some(format!("已在{}搜索{query}。", c[1].trim()));
            }
        }
    }

    if let Some(c) = CLOSE.captures(&t).or_else(|| CLOSE_BA.captures(&t)) {
        let name = c[1].trim();
        // 只处理和进程名或开始菜单应用名完全一致的，其余（关闭蓝牙、关掉这个网页…）交给大模型
        if !name.contains('的') {
            if let Closed::Done(text) = process::close(name, apps, true) {
                return Some(if text.starts_with("已关闭") { format!("已关闭{name}。") } else { format!("{text}。") });
            }
        }
        return None;
    }

    if let Some(c) = OPEN.captures(&t) {
        let name = c[1].trim();
        // “打开XX的YY”“打开网页搜索”之类复杂请求交给大模型
        if !name.contains('的') && !name.contains("网站") && !name.contains("网页") {
            if let Match::One(app) = apps.best(name) {
                if apps::launch(&app) {
                    return Some(format!("已打开{}。", app.name));
                }
            }
        }
        return None;
    }

    if let Some(c) = SEARCH.captures(&t) {
        let query = c[1].trim();
        if !query.is_empty() && sys::shell_open(&apps::search_url(query, "bing")) {
            return Some(format!("已为你搜索{query}。"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_phrases() {
        let name = |text: &str| {
            let t = normalize(text);
            CLOSE.captures(&t).or_else(|| CLOSE_BA.captures(&t)).map(|c| c[1].to_string())
        };
        assert_eq!(name("关闭GPT的后台。").as_deref(), Some("GPT"));
        assert_eq!(name("帮我关掉微信吧").as_deref(), Some("微信"));
        assert_eq!(name("把网易云音乐关了").as_deref(), Some("网易云音乐"));
        assert_eq!(name("结束ChatGPT进程").as_deref(), Some("ChatGPT"));
        assert_eq!(name("打开微信"), None);
    }

    #[test]
    fn site_search_phrases() {
        let parse = |text: &str| {
            let t = normalize(text);
            SITE_SEARCH
                .captures(&t)
                .and_then(|c| apps::site_engine(&c[1]).map(|e| (e, c[2].trim().trim_start_matches(['一', '下']).trim().to_string())))
        };
        assert_eq!(parse("打开抖音搜索无畏契约直播"), Some(("douyin", "无畏契约直播".into())));
        assert_eq!(parse("帮我在抖音里面搜一下无畏契约直播。"), Some(("douyin", "无畏契约直播".into())));
        assert_eq!(parse("打开抖音，然后搜索无畏契约直播"), Some(("douyin", "无畏契约直播".into())));
        assert_eq!(parse("在B站上搜原神"), Some(("bilibili", "原神".into())));
        assert_eq!(parse("淘宝搜机械键盘"), Some(("taobao", "机械键盘".into())));
        assert_eq!(parse("搜索明天的天气"), None); // 没说网站，交给普通搜索
        assert_eq!(parse("打开记事本"), None);
    }
}
