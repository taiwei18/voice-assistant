//! 麦克风采集 + “耳朵”线程：待机时只跑唤醒词检测，唤醒后把音频转发给云端识别。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::assistant::Command;
use crate::kws::WakeDetector;
use crate::resample::Resampler;

/// 待机：检测唤醒词
pub const MODE_IDLE: u8 = 0;
/// 录音：音频转发给识别
pub const MODE_CAPTURE: u8 = 1;
/// 静音：思考/播报期间，丢弃音频
pub const MODE_MUTED: u8 = 2;
/// 用户暂停了语音唤醒
pub const MODE_PAUSED: u8 = 3;
/// 播报中：只听唤醒词，听到就打断播报并开始录音
pub const MODE_BARGE: u8 = 4;

pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devs| devs.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string())).collect())
        .unwrap_or_default()
}

fn pick_device(hint: &str) -> anyhow::Result<cpal::Device> {
    let host = cpal::default_host();
    if !hint.is_empty() {
        let hint_lower = hint.to_lowercase();
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if d.description().is_ok_and(|desc| desc.name().to_lowercase().contains(&hint_lower)) {
                    return Ok(d);
                }
            }
        }
    }
    host.default_input_device().ok_or_else(|| anyhow!("没有找到麦克风"))
}

fn device_name(device: &cpal::Device) -> String {
    device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "未知设备".into())
}

/// 正在用的麦克风是不是已经不是该用的那个了：
/// 用系统默认时，默认设备换了（插上耳机）；指定了设备但当时没插、用了默认的，现在指定的又插上了。
fn should_switch(hint: &str, current: &str) -> bool {
    let host = cpal::default_host();
    if hint.is_empty() {
        return host.default_input_device().is_some_and(|d| device_name(&d) != current);
    }
    let hint = hint.to_lowercase();
    !current.to_lowercase().contains(&hint)
        && host.input_devices().is_ok_and(|mut ds| ds.any(|d| device_name(&d).to_lowercase().contains(&hint)))
}

/// 打开麦克风，回调里转成单声道 f32 发到 tx。返回（流，采样率，设备名）。
fn open_mic(hint: &str, tx: mpsc::Sender<Vec<f32>>) -> anyhow::Result<(cpal::Stream, u32, String)> {
    let device = pick_device(hint)?;
    let name = device_name(&device);
    let supported = device.default_input_config().context("读取麦克风格式失败")?;
    let channels = supported.channels() as usize;
    let rate = supported.sample_rate();
    let config = supported.config();
    let on_error = |e| eprintln!("麦克风错误：{e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let _ = tx.send(data.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect());
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let _ = tx.send(
                    data.chunks(channels)
                        .map(|f| f.iter().map(|&s| s as f32 / 32768.0).sum::<f32>() / channels as f32)
                        .collect(),
                );
            },
            on_error,
            None,
        ),
        other => return Err(anyhow!("不支持的麦克风采样格式：{other:?}")),
    }
    .context("打开麦克风失败")?;
    stream.play().context("启动录音失败")?;
    Ok((stream, rate, name))
}

/// 能量门限：跟踪底噪，只有声音明显高于底噪时才把音频交给唤醒词模型，安静时几乎不占 CPU。
/// 触发时先补上之前缓存的 0.5 秒，唤醒词开头不会被切掉；声音消失 2 秒后重新休眠。
pub(crate) struct EnergyGate {
    noise_floor: f32,
    active_samples_left: usize,
    history: std::collections::VecDeque<Vec<f32>>,
    history_len: usize,
}

impl EnergyGate {
    const PRE_ROLL: usize = 8000; // 0.5 秒
    const HANGOVER: usize = 32000; // 2 秒

    pub(crate) fn new() -> Self {
        Self { noise_floor: 0.002, active_samples_left: 0, history: Default::default(), history_len: 0 }
    }

    /// 返回需要送进唤醒词模型的音频片段（可能为空）
    pub(crate) fn process(&mut self, samples: &[f32], detector: &mut WakeDetector) -> Vec<Vec<f32>> {
        if samples.is_empty() {
            return Vec::new();
        }
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        let loud = rms > (self.noise_floor * 3.0).max(0.0015);
        if !loud {
            // 只在安静时慢慢跟踪底噪
            self.noise_floor = self.noise_floor * 0.98 + rms * 0.02;
        }
        if loud {
            let was_sleeping = self.active_samples_left == 0;
            self.active_samples_left = Self::HANGOVER;
            if was_sleeping {
                detector.reset();
                let mut out: Vec<Vec<f32>> = self.history.drain(..).collect();
                self.history_len = 0;
                out.push(samples.to_vec());
                return out;
            }
        }
        if self.active_samples_left > 0 {
            self.active_samples_left = self.active_samples_left.saturating_sub(samples.len());
            return vec![samples.to_vec()];
        }
        self.history.push_back(samples.to_vec());
        self.history_len += samples.len();
        while self.history_len > Self::PRE_ROLL {
            if let Some(front) = self.history.pop_front() {
                self.history_len -= front.len();
            }
        }
        Vec::new()
    }
}

pub struct EarSettings {
    pub device: String,
    pub wake_words: Vec<String>,
    pub threshold: f32,
    pub score: f32,
    pub model_dir: PathBuf,
}

/// 打不开麦克风（没插、被占用、格式不支持）；和唤醒词模型的问题区分开，只有这种才值得定时重试
#[derive(Debug)]
pub struct MicUnavailable;

impl std::fmt::Display for MicUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("麦克风不可用")
    }
}

pub fn is_mic_unavailable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<MicUnavailable>().is_some()
}

/// 多久没收到音频就认为麦克风断开了
const SILENT_DEVICE_TIMEOUT: Duration = Duration::from_secs(3);
/// 多久检查一次该用的麦克风有没有变化
const DEVICE_CHECK_EVERY: Duration = Duration::from_secs(5);

/// 后台“耳朵”线程的句柄；drop 时停止线程并释放麦克风
pub struct Ear {
    pub mode: Arc<AtomicU8>,
    /// 播报中听到唤醒词时通知（这时已经切到录音模式）
    pub barge: Arc<Notify>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Ear {
    pub fn set_mode(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }
}

impl Drop for Ear {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 启动耳朵线程。成功时返回（句柄，设备名，唤醒词问题说明）。
pub fn start_ear(
    settings: EarSettings,
    initial_mode: u8,
    events: UnboundedSender<Command>,
    audio_out: UnboundedSender<Vec<f32>>,
) -> anyhow::Result<(Ear, String, Vec<String>)> {
    let mode = Arc::new(AtomicU8::new(initial_mode));
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<(String, Vec<String>)>>();
    let barge = Arc::new(Notify::new());
    let (mode2, stop2, barge2) = (mode.clone(), stop.clone(), barge.clone());

    let thread = std::thread::Builder::new()
        .name("ear".into())
        .spawn(move || {
            // 模型和麦克风都在本线程里创建（cpal 的流不能跨线程）
            let detector = WakeDetector::new(&settings.model_dir, &settings.wake_words, settings.threshold, settings.score);
            let (mut detector, problems) = match detector {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let (raw_tx, raw_rx) = mpsc::channel();
            let (_stream, rate, name) = match open_mic(&settings.device, raw_tx) {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.context(MicUnavailable)));
                    return;
                }
            };
            let _ = ready_tx.send(Ok((name.clone(), problems)));

            let mut resampler = Resampler::new(rate);
            let mut gate = EnergyGate::new();
            let mut last_mode = u8::MAX;
            // 正常的麦克风即使安静也会持续回调；几秒没有数据说明设备被拔掉或出错了
            let mut last_audio = Instant::now();
            let mut last_device_check = Instant::now();
            while !stop2.load(Ordering::SeqCst) {
                if last_device_check.elapsed() > DEVICE_CHECK_EVERY {
                    last_device_check = Instant::now();
                    if should_switch(&settings.device, &name) {
                        let _ = events.send(Command::MicChanged);
                        return;
                    }
                }
                let Ok(chunk) = raw_rx.recv_timeout(Duration::from_millis(200)) else {
                    if last_audio.elapsed() > SILENT_DEVICE_TIMEOUT {
                        let _ = events.send(Command::MicChanged);
                        return;
                    }
                    continue;
                };
                last_audio = Instant::now();
                let samples = resampler.process(&chunk);
                let current = mode2.load(Ordering::SeqCst);
                if current == MODE_IDLE || current == MODE_BARGE {
                    if last_mode != current {
                        detector.reset();
                        gate = EnergyGate::new();
                    }
                    // 安静时不跑唤醒词模型；有声音时连同之前 0.5 秒一起送进去
                    for piece in gate.process(&samples, &mut detector) {
                        if let Some(word) = detector.accept(&piece) {
                            // 立刻切到录音，唤醒词之后的话一个字都不丢
                            mode2.store(MODE_CAPTURE, Ordering::SeqCst);
                            if current == MODE_BARGE {
                                barge2.notify_one();
                            } else {
                                let _ = events.send(Command::Wake(word));
                            }
                            break;
                        }
                    }
                } else if current == MODE_CAPTURE && !samples.is_empty() {
                    let _ = audio_out.send(samples);
                }
                last_mode = current;
            }
        })?;

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok((name, problems))) => Ok((Ear { mode, barge, stop, thread: Some(thread) }, name, problems)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("启动麦克风超时").context(MicUnavailable)),
    }
}
