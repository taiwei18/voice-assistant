//! 调度中心：前台负责对话，复杂任务交给后台智能体（Codex / Claude Code / 内置助手），做完主动播报。
//! 前台有两种：
//! - 实时语音（默认）：唤醒后麦克风音频直接发给百炼实时语音模型，边说边回，可以插话；
//! - 识别 + 大模型：fun-asr 识别一句话 → 本地直达规则 / DeepSeek → 系统播报。
//! 两种都支持播报时说唤醒词打断。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::agent::{Agent, FRONT_PROMPT};
use crate::asr::{self, AsrSettings};
use crate::audio::{self, Ear, EarSettings, MODE_BARGE, MODE_CAPTURE, MODE_IDLE, MODE_MUTED, MODE_PAUSED};
use crate::config::{self, Config};
use crate::llm::Llm;
use crate::player::Player;
use crate::realtime::{self, RealtimeSettings};
use crate::tasks::{TaskDone, TaskManager, TaskSettings};
use crate::tools::Tools;
use crate::tools::apps::AppIndex;
use crate::tts::Tts;
use crate::{fastpath, sys};

pub enum Command {
    /// 检测到唤醒词（来自耳朵线程）
    Wake(String),
    /// 界面上点了“说话”
    Talk,
    /// 界面上输入的文字命令
    Text(String),
    SetListening(bool),
    ClearHistory,
    Reload(Config),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Idle = 0,
    Listening = 1,
    Thinking = 2,
    Acting = 3,
    Speaking = 4,
    Paused = 5,
}

#[derive(Clone, Copy, Debug)]
pub enum Role {
    User = 0,
    Assistant = 1,
    Tool = 2,
    System = 3,
}

pub enum UiEvent {
    Status(Status),
    Partial(String),
    Chat(Role, String),
    Mic(String),
}

pub type UiSink = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// 实时对话里，唤醒后多久没开口就结束；回复完多久没再说话就结束
const REALTIME_FOLLOW_UP: Duration = Duration::from_secs(8);
/// 播报结束后再等一会儿才重新收音，避免扬声器余音被当成用户说话
const ECHO_TAIL: Duration = Duration::from_millis(500);

struct Assistant {
    cfg: Config,
    ui: UiSink,
    cmd_tx: UnboundedSender<Command>,
    apps: Arc<AppIndex>,
    tasks: Arc<TaskManager>,
    done_rx: UnboundedReceiver<TaskDone>,
    /// 前台工具：快速本地工具 + 后台任务
    front_tools: Arc<Tools>,
    /// 识别 + 大模型模式下的前台对话助手
    agent: Option<Agent>,
    tts: Tts,
    ear: Option<Ear>,
    audio_tx: UnboundedSender<Vec<f32>>,
    audio_rx: UnboundedReceiver<Vec<f32>>,
    listening: bool,
}

pub async fn run(cfg: Config, mut rx: UnboundedReceiver<Command>, cmd_tx: UnboundedSender<Command>, ui: UiSink) {
    let apps = Arc::new(AppIndex::load_cached());
    {
        let apps = apps.clone();
        tokio::task::spawn_blocking(move || apps.refresh());
    }
    let (audio_tx, audio_rx) = unbounded_channel();
    let (done_tx, done_rx) = unbounded_channel();
    let tasks = TaskManager::new(done_tx, ui.clone());
    let front_tools = Arc::new(Tools::front(apps.clone(), tasks.clone()));
    let mut a = Assistant {
        listening: cfg.listen_on_start,
        agent: None,
        cfg,
        ui,
        cmd_tx,
        apps,
        tasks,
        done_rx,
        front_tools,
        tts: Tts::new(),
        ear: None,
        audio_tx,
        audio_rx,
    };
    a.apply_config();
    a.restart_ear();
    a.go_idle();

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Command::Wake(word) => {
                        // 耳朵检测到唤醒词时会立刻切到录音模式；如果此后模式被改过（比如期间在处理别的命令），这个唤醒已过时
                        let fresh = a.ear.as_ref().is_some_and(|e| e.mode.load(Ordering::SeqCst) == MODE_CAPTURE);
                        if a.listening && fresh {
                            a.chat(Role::System, format!("（唤醒：{word}）"));
                            a.conversation(None).await;
                        }
                    }
                    Command::Talk => a.conversation(None).await,
                    Command::Text(text) => {
                        a.set_mode(MODE_MUTED);
                        if a.handle(&text).await {
                            a.classic_conversation(true).await; // 播报被唤醒词打断，接着听
                        }
                        a.go_idle();
                    }
                    Command::SetListening(on) => {
                        a.listening = on;
                        a.go_idle();
                    }
                    Command::ClearHistory => {
                        if let Some(agent) = a.agent.as_mut() {
                            agent.clear();
                        }
                    }
                    Command::Reload(new_cfg) => a.reload(new_cfg),
                }
            }
            done = a.done_rx.recv() => {
                if let Some(done) = done {
                    a.announce(done).await;
                }
            }
        }
    }
    a.tasks.cancel_all();
}

impl Assistant {
    fn emit(&self, ev: UiEvent) {
        (self.ui)(ev);
    }

    fn chat(&self, role: Role, text: impl Into<String>) {
        self.emit(UiEvent::Chat(role, text.into()));
    }

    fn set_mode(&self, mode: u8) {
        if let Some(ear) = &self.ear {
            ear.set_mode(mode);
        }
    }

    fn mode(&self) -> u8 {
        self.ear.as_ref().map_or(MODE_MUTED, |e| e.mode.load(Ordering::SeqCst))
    }

    fn go_idle(&mut self) {
        while self.audio_rx.try_recv().is_ok() {}
        self.set_mode(if self.listening { MODE_IDLE } else { MODE_PAUSED });
        self.emit(UiEvent::Partial(String::new()));
        self.emit(UiEvent::Status(if self.listening { Status::Idle } else { Status::Paused }));
    }

    /// 根据配置重建前台助手和后台任务设置
    fn apply_config(&mut self) {
        let full_tools = Arc::new(Tools::new(&self.cfg, self.apps.clone()));
        self.tasks.update_settings(TaskSettings::from_config(&self.cfg, full_tools));
        self.agent = self.build_agent();
    }

    fn build_agent(&self) -> Option<Agent> {
        let key = config::llm_key(&self.cfg)?;
        let c = &self.cfg;
        match Llm::new(&c.llm_base_url, key, &c.llm_model, c.llm_thinking, &c.llm_effort) {
            Ok(llm) => Some(Agent::front(llm, self.front_tools.clone(), c.new_conversation_after_secs)),
            Err(e) => {
                self.chat(Role::System, format!("大模型初始化失败：{e}"));
                None
            }
        }
    }

    fn restart_ear(&mut self) {
        self.ear = None; // 先释放旧的麦克风
        let settings = EarSettings {
            device: self.cfg.input_device.clone(),
            wake_words: self.cfg.wake_words.clone(),
            threshold: self.cfg.kws_threshold,
            score: self.cfg.kws_score,
            model_dir: config::models_dir().join("kws"),
        };
        let mode = if self.listening { MODE_IDLE } else { MODE_PAUSED };
        match audio::start_ear(settings, mode, self.cmd_tx.clone(), self.audio_tx.clone()) {
            Ok((ear, name, problems)) => {
                self.ear = Some(ear);
                self.emit(UiEvent::Mic(name));
                for p in problems {
                    self.chat(Role::System, format!("唤醒词问题：{p}"));
                }
            }
            Err(e) => {
                self.emit(UiEvent::Mic("麦克风不可用".into()));
                self.chat(Role::System, format!("语音唤醒不可用：{e}"));
            }
        }
    }

    fn reload(&mut self, new_cfg: Config) {
        let ear_changed = new_cfg.input_device != self.cfg.input_device
            || new_cfg.wake_words != self.cfg.wake_words
            || new_cfg.kws_threshold != self.cfg.kws_threshold
            || new_cfg.kws_score != self.cfg.kws_score;
        self.cfg = new_cfg;
        self.apply_config();
        if ear_changed {
            self.restart_ear();
        }
        self.go_idle();
        self.chat(Role::System, "设置已生效。");
    }

    fn realtime_enabled(&self) -> bool {
        self.cfg.voice_frontend == "realtime"
    }

    /// 一次语音对话。notice：后台任务完成时要先告诉用户的内容
    async fn conversation(&mut self, notice: Option<String>) {
        if self.realtime_enabled() {
            match self.realtime_conversation(notice.as_deref()).await {
                Ok(()) => {}
                Err(e) => {
                    self.chat(Role::System, format!("{e:#}；这次改用“识别 + 大模型”。"));
                    if let Some(text) = notice {
                        self.speak(&text).await;
                    } else {
                        self.classic_conversation(true).await;
                    }
                }
            }
        } else {
            let mut barged = false;
            if let Some(text) = notice {
                self.chat(Role::Assistant, text.clone());
                barged = self.speak(&text).await;
                if !barged {
                    self.go_idle();
                    return;
                }
            }
            self.classic_conversation(!barged).await;
        }
        self.go_idle();
    }

    /// 后台任务做完了：主动告诉用户
    async fn announce(&mut self, done: TaskDone) {
        let text = if done.ok { format!("任务{}做完了：{}", done.id, done.text) } else { format!("任务{}没做成：{}", done.id, done.text) };
        self.conversation(Some(text)).await;
    }

    // ---------- 识别 + 大模型 ----------

    /// 一轮对话；开启“继续聆听”或播报被打断时，接着听下一句
    async fn classic_conversation(&mut self, beep: bool) {
        let mut next = self.listen(beep).await;
        while let Some(text) = next.take() {
            let barged = self.handle(&text).await;
            if barged || (self.cfg.follow_up && self.listening) {
                next = self.listen(barged).await;
            }
        }
    }

    /// 听一句话（云端流式识别）。没听到内容时返回 None。
    async fn listen(&mut self, beep: bool) -> Option<String> {
        if self.ear.is_none() {
            self.chat(Role::System, "麦克风不可用，可以在下方输入文字命令。");
            return None;
        }
        let Some(api_key) = config::asr_key(&self.cfg) else {
            self.chat(Role::System, "还没有设置语音识别的 API Key：请在“设置”页填写阿里云百炼的 key，点“保存并应用”。");
            return None;
        };
        self.set_mode(MODE_CAPTURE);
        if beep {
            sys::beep(880, 120);
        }
        self.emit(UiEvent::Status(Status::Listening));
        let settings = AsrSettings {
            url: self.cfg.asr_url.clone(),
            model: self.cfg.asr_model.clone(),
            api_key,
            sentence_silence_ms: self.cfg.asr_sentence_silence_ms,
            speech_timeout: Duration::from_secs(self.cfg.asr_speech_timeout_secs),
        };
        let ui = self.ui.clone();
        let result = asr::recognize(&settings, &mut self.audio_rx, |partial| ui(UiEvent::Partial(partial.to_string()))).await;
        self.set_mode(MODE_MUTED);
        while self.audio_rx.try_recv().is_ok() {}
        self.emit(UiEvent::Partial(String::new()));
        match result {
            Ok(text) => text,
            Err(e) => {
                self.chat(Role::System, format!("{e:#}"));
                None
            }
        }
    }

    /// 处理一句话并播报回复。返回播报是否被唤醒词打断
    async fn handle(&mut self, text: &str) -> bool {
        self.chat(Role::User, text);
        self.emit(UiEvent::Status(Status::Thinking));
        // 直达规则里关闭程序要等它退出（最多几秒），不能占着异步线程干等
        let reply = if let Some(r) = tokio::task::block_in_place(|| fastpath::try_handle(text, &self.apps)) {
            r
        } else if let Some(agent) = self.agent.as_mut() {
            let ui = self.ui.clone();
            let on_tool = |name: &str, input: &Value| {
                ui(UiEvent::Status(Status::Acting));
                ui(UiEvent::Chat(Role::Tool, describe_tool(name, input)));
            };
            match agent.ask(text, on_tool).await {
                Ok(r) => r,
                Err(e) => {
                    (self.ui)(UiEvent::Chat(Role::System, format!("{e:#}")));
                    "出错了，请再试一次。".into()
                }
            }
        } else {
            self.chat(Role::System, "还没有设置大模型的 API Key：请在“设置”页填写 DeepSeek 的 key，点“保存并应用”。");
            "还没有配置大模型。".into()
        };
        self.chat(Role::Assistant, reply.clone());
        self.speak(&reply).await
    }

    /// 系统播报。开启打断时播报期间仍听唤醒词，听到就停下并返回 true（这时已经在录音）
    async fn speak(&self, text: &str) -> bool {
        self.emit(UiEvent::Status(Status::Speaking));
        let barge = self.cfg.barge_in && self.ear.is_some() && self.listening;
        self.set_mode(if barge { MODE_BARGE } else { MODE_MUTED });
        let speaking = self.tts.speak(text, self.cfg.tts_rate);
        tokio::pin!(speaking);
        let notify = self.ear.as_ref().map(|e| e.barge.clone());
        loop {
            tokio::select! {
                _ = &mut speaking => break,
                _ = async { notify.as_ref().unwrap().notified().await }, if barge => {
                    if self.mode() == MODE_CAPTURE {
                        self.tts.stop();
                        (&mut speaking).await;
                        self.chat(Role::System, "（已打断）");
                        return true;
                    }
                }
            }
        }
        self.set_mode(MODE_MUTED);
        // 扬声器还有余音，多等一会儿再恢复收音，避免播报内容误触发唤醒
        tokio::time::sleep(Duration::from_millis(600)).await;
        false
    }

    // ---------- 实时语音 ----------

    async fn realtime_conversation(&mut self, notice: Option<&str>) -> anyhow::Result<()> {
        if self.ear.is_none() && notice.is_none() {
            anyhow::bail!("麦克风不可用");
        }
        let api_key = config::asr_key(&self.cfg).ok_or_else(|| anyhow::anyhow!("实时语音需要阿里云百炼的 API Key"))?;
        let settings = RealtimeSettings {
            url: self.cfg.realtime_url.clone(),
            model: self.cfg.realtime_model.clone(),
            voice: self.cfg.realtime_voice.clone(),
            api_key,
            instructions: FRONT_PROMPT.to_string(),
            tools: self.front_tools.definitions(),
            speech_rate: (1.0 + self.cfg.tts_rate as f32 * 0.1).clamp(0.5, 2.0),
        };
        self.emit(UiEvent::Status(Status::Thinking));
        let mut player = Player::open()?;
        let mut session = realtime::Session::connect(&settings).await?;
        let result = self.realtime_loop(&mut session, &mut player, notice).await;
        session.close().await;
        result
    }

    async fn realtime_loop(&mut self, session: &mut realtime::Session, player: &mut Player, notice: Option<&str>) -> anyhow::Result<()> {
        let duplex = self.cfg.full_duplex;
        let barge = self.cfg.barge_in && self.listening;
        let notify = self.ear.as_ref().map(|e| e.barge.clone());
        let mut responding = false; // 模型正在生成回复
        let mut transcript = String::new();
        let mut calls: Vec<(String, String, String)> = Vec::new();
        let mut pending_notices: Vec<String> = Vec::new();
        let mut resume_at: Option<Instant> = None; // 播完后什么时候恢复收音
        let mut user_speaking = false;
        // 回复有时比用户那句话的转写先到；先压着，等转写到了再按顺序显示
        let mut awaiting_user_text = false;
        let mut held_reply: Option<String> = None;
        let mut last_activity = Instant::now();
        let mut wait_limit = Duration::from_secs(self.cfg.asr_speech_timeout_secs.max(3));
        let mut ticker = tokio::time::interval(Duration::from_millis(50));

        if let Some(text) = notice {
            self.chat(Role::System, format!("（后台任务通知）{text}"));
            session.say(&format!("（这是系统通知，不是用户说的话）{text}。请用一两句话口语化地告诉用户这个结果。")).await?;
            responding = true;
            self.set_mode(MODE_MUTED);
        } else {
            while self.audio_rx.try_recv().is_ok() {}
            self.set_mode(MODE_CAPTURE);
            sys::beep(880, 120);
            self.emit(UiEvent::Status(Status::Listening));
        }

        loop {
            tokio::select! {
                chunk = self.audio_rx.recv() => {
                    if let Some(chunk) = chunk {
                        if self.mode() == MODE_CAPTURE {
                            session.send_audio(&chunk).await?;
                        }
                    }
                }
                event = session.next_event() => {
                    last_activity = Instant::now();
                    match event? {
                        realtime::Event::SpeechStarted => {
                            // 用户插话：服务端已经取消回复，本地也别再播了
                            user_speaking = true;
                            player.clear();
                            self.emit(UiEvent::Status(Status::Listening));
                        }
                        realtime::Event::SpeechStopped => {
                            user_speaking = false;
                            awaiting_user_text = true;
                            responding = true;
                            self.emit(UiEvent::Status(Status::Thinking));
                        }
                        realtime::Event::UserPartial(text) => self.emit(UiEvent::Partial(text)),
                        realtime::Event::UserText(text) => {
                            self.emit(UiEvent::Partial(String::new()));
                            if !text.trim().is_empty() {
                                self.chat(Role::User, text);
                            }
                            awaiting_user_text = false;
                            if let Some(reply) = held_reply.take() {
                                self.chat(Role::Assistant, reply);
                            }
                        }
                        realtime::Event::Transcript(delta) => transcript.push_str(&delta),
                        realtime::Event::Audio(bytes) => {
                            if !player.is_playing() {
                                self.emit(UiEvent::Status(Status::Speaking));
                                if !duplex {
                                    // 外放时不把麦克风发过去，否则模型会听到自己的声音；改为只听唤醒词
                                    self.set_mode(if barge { MODE_BARGE } else { MODE_MUTED });
                                }
                            }
                            player.push_pcm16(&bytes);
                            resume_at = None;
                        }
                        realtime::Event::FunctionCall { call_id, name, arguments } => calls.push((call_id, name, arguments)),
                        realtime::Event::ResponseDone(status) => {
                            responding = false;
                            if !transcript.trim().is_empty() {
                                let reply = std::mem::take(&mut transcript);
                                if awaiting_user_text {
                                    held_reply = Some(reply);
                                } else {
                                    self.chat(Role::Assistant, reply);
                                }
                            }
                            transcript.clear();
                            if status == "failed" {
                                self.chat(Role::System, "实时语音这一轮回复失败了");
                            }
                            if !calls.is_empty() {
                                // 执行工具，把结果交回去让模型接着说
                                self.emit(UiEvent::Status(Status::Acting));
                                for (call_id, name, arguments) in std::mem::take(&mut calls) {
                                    let input: Value = serde_json::from_str(&arguments).unwrap_or_default();
                                    self.chat(Role::Tool, describe_tool(&name, &input));
                                    let (output, _) = self.front_tools.call(&name, &input).await;
                                    session.send_function_output(&call_id, &output).await?;
                                }
                                responding = true;
                            } else if let Some(text) = pending_notices.pop() {
                                session.say(&text).await?;
                                responding = true;
                            }
                        }
                        realtime::Event::Other => {}
                    }
                }
                done = self.done_rx.recv() => {
                    if let Some(done) = done {
                        let text = if done.ok { format!("任务{}做完了：{}", done.id, done.text) } else { format!("任务{}没做成：{}", done.id, done.text) };
                        self.chat(Role::System, format!("（后台任务通知）{text}"));
                        let text = format!("（这是系统通知，不是用户说的话）{text}。请用一两句话口语化地告诉用户这个结果。");
                        if responding || user_speaking {
                            pending_notices.push(text);
                        } else {
                            session.say(&text).await?;
                            responding = true;
                        }
                    }
                }
                _ = async { notify.as_ref().unwrap().notified().await }, if barge && notify.is_some() => {
                    // 播报时说了唤醒词：停止播放、取消回复，开始听
                    if self.mode() == MODE_CAPTURE {
                        player.clear();
                        if responding {
                            session.cancel_response().await;
                        }
                        self.chat(Role::System, "（已打断）");
                        sys::beep(880, 80);
                        self.emit(UiEvent::Status(Status::Listening));
                        last_activity = Instant::now();
                        wait_limit = REALTIME_FOLLOW_UP;
                    }
                }
                _ = ticker.tick() => {
                    let playing = player.is_playing();
                    if playing || responding {
                        last_activity = Instant::now();
                    }
                    // 播完了：等余音过去再恢复收音
                    if !playing && !responding && self.mode() != MODE_CAPTURE {
                        let at = *resume_at.get_or_insert_with(|| Instant::now() + ECHO_TAIL);
                        if Instant::now() >= at {
                            resume_at = None;
                            if !self.cfg.follow_up && notice.is_none() {
                                break;
                            }
                            while self.audio_rx.try_recv().is_ok() {}
                            self.set_mode(MODE_CAPTURE);
                            self.emit(UiEvent::Status(Status::Listening));
                            last_activity = Instant::now();
                            wait_limit = REALTIME_FOLLOW_UP;
                        }
                    }
                    if !playing && !responding && !user_speaking && self.mode() == MODE_CAPTURE && last_activity.elapsed() > wait_limit {
                        break; // 没人说话了，回到待机
                    }
                }
            }
        }
        if let Some(reply) = held_reply {
            self.chat(Role::Assistant, reply);
        }
        Ok(())
    }
}

pub fn describe_tool(name: &str, input: &Value) -> String {
    let args = input.to_string();
    let args: String = args.chars().take(80).collect();
    format!("{name} {args}")
}
