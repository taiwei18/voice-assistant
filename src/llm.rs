//! 大模型客户端：Anthropic Messages 格式（DeepSeek 提供兼容接口），直接用 HTTP 调用。

use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

pub struct Llm {
    http: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    thinking: bool,
    effort: String,
}

impl Llm {
    pub fn new(base_url: &str, api_key: String, model: &str, thinking: bool, effort: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
        Ok(Self {
            http,
            url: format!("{}/v1/messages", base_url.trim_end_matches('/')),
            api_key,
            model: model.to_string(),
            thinking,
            effort: effort.to_string(),
        })
    }

    /// 发一次请求，返回完整的响应 JSON（content 原样保留，思考块要原样回传）
    pub async fn create(&self, system: &str, tools: &[Value], messages: &[Value]) -> anyhow::Result<Value> {
        let mut body = json!({
            "model": self.model,
            "max_tokens": 8000,
            "system": system,
            "tools": tools,
            "messages": messages,
        });
        if self.thinking {
            body["thinking"] = json!({"type": "enabled"});
            body["output_config"] = json!({"effort": self.effort});
        } else {
            body["thinking"] = json!({"type": "disabled"});
        }
        self.send(&body).await
    }

    /// 测试 key、地址和模型名：发一条极短的请求
    pub async fn ping(&self) -> anyhow::Result<()> {
        let body = json!({
            "model": self.model,
            "max_tokens": 16,
            "thinking": {"type": "disabled"},
            "messages": [{"role": "user", "content": "你好"}],
        });
        self.send(&body).await.map(|_| ())
    }

    async fn send(&self, body: &Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .bearer_auth(&self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .context("请求大模型失败（检查网络）")?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("大模型返回 {status}：{}", text.chars().take(300).collect::<String>());
        }
        Ok(serde_json::from_str(&text)?)
    }
}
