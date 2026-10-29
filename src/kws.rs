//! 唤醒词检测：sherpa-onnx 关键词模型（约 3M 参数），唤醒词可随时修改，不需要重新训练。

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, bail};
use pinyin::ToPinyin;
use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig, OnlineStream};

/// 声母（含 y/w，和 sherpa-onnx 的 ppinyin 切分方式一致）
const INITIALS: [&str; 23] =
    ["zh", "ch", "sh", "b", "p", "m", "f", "d", "t", "n", "l", "g", "k", "h", "j", "q", "x", "r", "z", "c", "s", "y", "w"];

/// 把一个中文唤醒词转成模型的 token：声母 + 带声调的韵母，如 你好 -> n ǐ h ǎo
fn word_tokens(word: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    for py in word.to_pinyin() {
        let syllable = py?.with_tone();
        match INITIALS.iter().find(|i| syllable.starts_with(**i) && syllable.len() > i.len()) {
            Some(initial) => {
                tokens.push(initial.to_string());
                tokens.push(syllable[initial.len()..].to_string());
            }
            None => tokens.push(syllable.to_string()),
        }
    }
    (!tokens.is_empty()).then_some(tokens)
}

/// 生成 keywords 文本；返回（文本，无法使用的唤醒词说明）
fn keywords_text(words: &[String], vocab: &HashSet<String>) -> (String, Vec<String>) {
    let mut lines = Vec::new();
    let mut problems = Vec::new();
    for word in words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        match word_tokens(word) {
            Some(tokens) if tokens.iter().all(|t| vocab.contains(t)) => {
                lines.push(format!("{} @{}", tokens.join(" "), word));
            }
            Some(_) => problems.push(format!("“{word}”含有模型不支持的读音")),
            None => problems.push(format!("“{word}”需要是中文")),
        }
    }
    (lines.join("\n"), problems)
}

pub struct WakeDetector {
    spotter: KeywordSpotter,
    stream: OnlineStream,
}

impl WakeDetector {
    pub fn new(model_dir: &Path, words: &[String], threshold: f32, score: f32) -> anyhow::Result<(Self, Vec<String>)> {
        let file = |name: &str| model_dir.join(name).to_string_lossy().into_owned();
        let tokens_path = model_dir.join("tokens.txt");
        let vocab: HashSet<String> = std::fs::read_to_string(&tokens_path)
            .with_context(|| format!("找不到唤醒词模型：{}", tokens_path.display()))?
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect();
        let (keywords, problems) = keywords_text(words, &vocab);
        if keywords.is_empty() {
            bail!("没有可用的唤醒词。{}", problems.join("；"));
        }

        let mut config = KeywordSpotterConfig::default();
        config.model_config.transducer.encoder = Some(file("encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx"));
        config.model_config.transducer.decoder = Some(file("decoder-epoch-13-avg-2-chunk-16-left-64.onnx"));
        config.model_config.transducer.joiner = Some(file("joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx"));
        config.model_config.tokens = Some(file("tokens.txt"));
        config.model_config.num_threads = 1;
        config.keywords_threshold = threshold;
        config.keywords_score = score;
        config.keywords_buf = Some(keywords);
        let spotter = KeywordSpotter::create(&config).context("加载唤醒词模型失败")?;
        let stream = spotter.create_stream();
        Ok((Self { spotter, stream }, problems))
    }

    /// 喂入 16kHz 音频，检测到唤醒词时返回该词
    pub fn accept(&mut self, samples: &[f32]) -> Option<String> {
        self.stream.accept_waveform(16000, samples);
        while self.spotter.is_ready(&self.stream) {
            self.spotter.decode(&self.stream);
            if let Some(result) = self.spotter.get_result(&self.stream) {
                if !result.keyword.is_empty() {
                    self.spotter.reset(&self.stream);
                    return Some(result.keyword);
                }
            }
        }
        None
    }

    /// 丢弃之前的状态（静音、识别结束后调用，避免残留音频误触发）
    pub fn reset(&mut self) {
        self.stream = self.spotter.create_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_sherpa_ppinyin() {
        assert_eq!(word_tokens("周望军").unwrap(), ["zh", "ōu", "w", "àng", "j", "ūn"]);
        assert_eq!(word_tokens("女儿").unwrap(), ["n", "ǚ", "ér"]);
        assert_eq!(word_tokens("你好电脑").unwrap(), ["n", "ǐ", "h", "ǎo", "d", "iàn", "n", "ǎo"]);
    }
}
