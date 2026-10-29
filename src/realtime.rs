//! 阿里云百炼实时语音模型（Qwen-Audio Realtime，WebSocket）：麦克风音频直接发过去，
//! 服务端判断一句话说完后边想边说，返回 24kHz 语音；支持工具调用和打断。
//! 协议：session.update → input_audio_buffer.append（16k PCM，Base64）→ response.audio.delta / response.done，
//! 工具调用：response.function_call_arguments.done → conversation.item.create(function_call_output) → response.create。

use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub struct RealtimeSettings {
    pub url: String,
    pub model: String,
    pub voice: String,
    pub api_key: String,
    pub instructions: String,
    /// Anthropic 格式的工具定义，连接时转换成这里要的格式
    pub tools: Vec<Value>,
    pub speech_rate: f32,
}

pub enum Event {
    /// 用户开始说话（服务端会自动打断正在进行的回复）
    SpeechStarted,
    SpeechStopped,
    UserPartial(String),
    UserText(String),
    /// 助手回复的文字（随语音一起流式返回）
    Transcript(String),
    /// 24k PCM16 语音
    Audio(Vec<u8>),
    FunctionCall { call_id: String, name: String, arguments: String },
    /// 一轮回复结束：completed / cancelled / failed
    ResponseDone(String),
    Other,
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Session {
    ws: Ws,
}

impl Session {
    pub async fn connect(s: &RealtimeSettings) -> anyhow::Result<Self> {
        let url = format!("{}?model={}", s.url.trim_end_matches('/'), s.model);
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert("Authorization", format!("Bearer {}", s.api_key).parse()?);
        let (ws, _) = tokio::time::timeout(Duration::from_secs(8), connect_async(request))
            .await
            .context("连接实时语音服务超时")?
            .context("连接实时语音服务失败（检查 API Key、模型是否已开通和网络）")?;
        let mut session = Self { ws };
        let tools: Vec<Value> = s
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["input_schema"]}}))
            .collect();
        session
            .send(json!({
                "type": "session.update",
                "session": {
                    "modalities": ["audio", "text"],
                    "voice": s.voice,
                    "speech_rate": s.speech_rate,
                    "instructions": s.instructions,
                    "input_audio_format": "pcm",
                    "output_audio_format": "pcm",
                    "input_audio_transcription": {"language": "zh"},
                    "tools": tools,
                    "turn_detection": {"type": "smart_turn_v2"},
                }
            }))
            .await?;
        // 等 session.updated；参数有错时服务端回 error
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(8), session.ws.next()).await.context("实时语音服务无响应")?;
            let v = parse(msg)?;
            match v["type"].as_str() {
                Some("session.updated") => return Ok(session),
                Some("error") => bail!("实时语音配置失败：{}", v["error"]["message"].as_str().unwrap_or("未知错误")),
                _ => {}
            }
        }
    }

    async fn send(&mut self, v: Value) -> anyhow::Result<()> {
        self.ws.send(Message::text(v.to_string())).await.context("发送到实时语音服务失败")
    }

    /// 发送 16k 单声道音频
    pub async fn send_audio(&mut self, samples: &[f32]) -> anyhow::Result<()> {
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            pcm.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        self.send(json!({"type": "input_audio_buffer.append", "audio": base64_encode(&pcm)})).await
    }

    pub async fn send_function_output(&mut self, call_id: &str, output: &str) -> anyhow::Result<()> {
        // output 要求是 JSON 字符串
        let output = json!({"result": output}).to_string();
        self.send(json!({"type": "conversation.item.create", "item": {"type": "function_call_output", "call_id": call_id, "output": output}}))
            .await?;
        self.create_response().await
    }

    /// 插入一条文字消息（打字输入、后台任务完成通知）并让模型回复
    pub async fn say(&mut self, text: &str) -> anyhow::Result<()> {
        self.send(json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}}))
            .await?;
        self.create_response().await
    }

    pub async fn create_response(&mut self) -> anyhow::Result<()> {
        self.send(json!({"type": "response.create"})).await
    }

    pub async fn cancel_response(&mut self) {
        let _ = self.send(json!({"type": "response.cancel"})).await;
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    /// 下一个事件；连接断开时返回错误
    pub async fn next_event(&mut self) -> anyhow::Result<Event> {
        let v = parse(self.ws.next().await)?;
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Ok(match v["type"].as_str().unwrap_or("") {
            "input_audio_buffer.speech_started" => Event::SpeechStarted,
            "input_audio_buffer.speech_stopped" if v["reason"] != "turn_invalid" => Event::SpeechStopped,
            "conversation.item.input_audio_transcription.delta" => Event::UserPartial(format!("{}{}", s("text"), s("stash"))),
            "conversation.item.input_audio_transcription.completed" => Event::UserText(s("transcript")),
            "response.audio_transcript.delta" | "response.text.delta" => Event::Transcript(s("delta")),
            "response.audio.delta" => Event::Audio(base64_decode(v["delta"].as_str().unwrap_or(""))),
            "response.function_call_arguments.done" => {
                Event::FunctionCall { call_id: s("call_id"), name: s("name"), arguments: s("arguments") }
            }
            "response.done" => Event::ResponseDone(v["response"]["status"].as_str().unwrap_or("completed").to_string()),
            "error" => {
                let err = &v["error"];
                let message = err["message"].as_str().unwrap_or("未知错误");
                if err["type"] == "server_error" {
                    bail!("实时语音服务出错：{message}");
                }
                eprintln!("实时语音请求错误：{message}"); // 客户端错误不断开连接，比如没有回复时发了取消
                Event::Other
            }
            _ => Event::Other,
        })
    }
}

fn parse(msg: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) -> anyhow::Result<Value> {
    match msg {
        Some(Ok(Message::Text(t))) => Ok(serde_json::from_str(t.as_str())?),
        Some(Ok(Message::Close(frame))) => bail!("实时语音连接被关闭：{}", frame.map(|f| f.reason.to_string()).unwrap_or_default()),
        Some(Ok(_)) => Ok(Value::Null),
        Some(Err(e)) => Err(e.into()),
        None => bail!("实时语音连接已断开"),
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue, // '=' 和换行
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foobar", &[0, 255, 128, 7, 9]] {
            assert_eq!(base64_decode(&base64_encode(data)), data);
        }
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    }
}
