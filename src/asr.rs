//! 阿里云百炼 fun-asr-realtime 流式识别（WebSocket）。
//! 流程：run-task → 等 task-started → 持续发 16k PCM → 收到整句结束 → finish-task → task-finished。

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub struct AsrSettings {
    pub url: String,
    pub model: String,
    pub api_key: String,
    pub sentence_silence_ms: u32,
    /// 多久没开口就放弃
    pub speech_timeout: Duration,
}

const MAX_SESSION: Duration = Duration::from_secs(30);
const CHUNK_SAMPLES: usize = 1600; // 每 100ms 发一次

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

/// 测试 key 和网络：建立连接、启动任务、立即结束
pub async fn check(s: &AsrSettings) -> anyhow::Result<()> {
    let (mut tx, mut rx, finish_task) = open_task(s).await?;
    tx.send(finish_task).await?;
    let _ = tokio::time::timeout(Duration::from_secs(3), rx.next()).await;
    let _ = tx.close().await;
    Ok(())
}

/// 连接服务并发送 run-task，等到 task-started 后返回（发送端，接收端，finish-task 消息）
async fn open_task(s: &AsrSettings) -> anyhow::Result<(WsSink, WsStream, Message)> {
    let mut request = s.url.as_str().into_client_request()?;
    request.headers_mut().insert("Authorization", format!("Bearer {}", s.api_key).parse()?);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(8), connect_async(request))
        .await
        .context("连接语音识别服务超时")?
        .context("连接语音识别服务失败（检查 API Key、服务地址和网络）")?;
    let (mut tx, mut rx) = ws.split();

    let task_id = uuid::Uuid::new_v4().simple().to_string();
    let run_task = json!({
        "header": {"action": "run-task", "task_id": task_id, "streaming": "duplex"},
        "payload": {
            "task_group": "audio", "task": "asr", "function": "recognition", "model": s.model,
            "parameters": {
                "format": "pcm", "sample_rate": 16000, "language_hints": ["zh"],
                "max_sentence_silence": s.sentence_silence_ms,
            },
            "input": {}
        }
    });
    tx.send(Message::text(run_task.to_string())).await?;
    let finish_task = Message::text(
        json!({"header": {"action": "finish-task", "task_id": task_id, "streaming": "duplex"}, "payload": {"input": {}}})
            .to_string(),
    );

    // 等 task-started；这期间麦克风音频在通道里排队，不会丢
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.next()).await.context("语音识别服务无响应")?;
        let event = parse_event(msg)?;
        match event["header"]["event"].as_str() {
            Some("task-started") => break,
            Some("task-failed") => bail!("语音识别失败：{}", event["header"]["error_message"].as_str().unwrap_or("未知错误")),
            _ => {}
        }
    }
    Ok((tx, rx, finish_task))
}

/// 识别一句话。没人说话或没识别出内容时返回 Ok(None)。
pub async fn recognize(
    s: &AsrSettings,
    audio: &mut UnboundedReceiver<Vec<f32>>,
    mut on_partial: impl FnMut(&str),
) -> anyhow::Result<Option<String>> {
    let (mut tx, mut rx, finish_task) = open_task(s).await?;
    let started = Instant::now();
    let mut pcm: Vec<u8> = Vec::with_capacity(CHUNK_SAMPLES * 2);
    let mut heard = false;
    let mut finished_at: Option<Instant> = None;
    let mut text = String::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            chunk = audio.recv(), if finished_at.is_none() => {
                let Some(chunk) = chunk else { break };
                for s in chunk {
                    pcm.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
                }
                if pcm.len() >= CHUNK_SAMPLES * 2 {
                    tx.send(Message::binary(std::mem::take(&mut pcm))).await?;
                }
            }
            msg = rx.next() => {
                let Some(msg) = msg else { break };
                let event = parse_event(Some(msg))?;
                match event["header"]["event"].as_str() {
                    Some("result-generated") => {
                        let sentence = &event["payload"]["output"]["sentence"];
                        if sentence["heartbeat"].as_bool() == Some(true) {
                            continue;
                        }
                        let partial = sentence["text"].as_str().unwrap_or("");
                        if !partial.is_empty() {
                            heard = true;
                            on_partial(partial);
                        }
                        if sentence["sentence_end"].as_bool() == Some(true) {
                            text.push_str(partial);
                            if finished_at.is_none() {
                                tx.send(finish_task.clone()).await?;
                                finished_at = Some(Instant::now());
                            }
                        }
                    }
                    Some("task-finished") => break,
                    Some("task-failed") => bail!("语音识别失败：{}", event["header"]["error_message"].as_str().unwrap_or("未知错误")),
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                let timed_out = (!heard && started.elapsed() > s.speech_timeout) || started.elapsed() > MAX_SESSION;
                if finished_at.is_none() && timed_out {
                    if !pcm.is_empty() {
                        tx.send(Message::binary(std::mem::take(&mut pcm))).await?;
                    }
                    tx.send(finish_task.clone()).await?;
                    finished_at = Some(Instant::now());
                }
                if finished_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
                    break;
                }
            }
        }
    }
    let _ = tx.close().await;
    let text = text.trim().to_string();
    Ok((!text.is_empty()).then_some(text))
}

fn parse_event(msg: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) -> anyhow::Result<Value> {
    match msg {
        Some(Ok(Message::Text(t))) => Ok(serde_json::from_str(t.as_str())?),
        Some(Ok(Message::Close(frame))) => bail!("语音识别连接被关闭：{}", frame.map(|f| f.reason.to_string()).unwrap_or_default()),
        Some(Ok(_)) => Ok(Value::Null),
        Some(Err(e)) => Err(e.into()),
        None => bail!("语音识别连接已断开"),
    }
}
