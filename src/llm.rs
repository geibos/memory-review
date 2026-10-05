//! OpenAI-compatible chat completions with a single tool the model should call.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// `system`, `user` or `assistant`.
    pub role: &'static str,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: "system",
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: "user",
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: "assistant",
            content: content.into(),
        }
    }
}

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LlmOutcome {
    /// The model called the tool with these (parsed) arguments.
    ToolCall(Value),
    /// The model answered with text, or with arguments that are not JSON.
    Text(String),
}

// async-trait (dyn): held as `Arc<dyn Llm>` so tests can script replies.
#[async_trait]
pub trait Llm: Send + Sync {
    async fn call_tool(
        &self,
        messages: &[ChatMessage],
        tool: &ToolSpec,
    ) -> anyhow::Result<LlmOutcome>;
}

pub struct LiteLlm {
    http: reqwest::Client,
    url: String,
    key: String,
    model: String,
    retry_delay: Duration,
}

impl LiteLlm {
    pub fn new(url: String, key: String, model: String) -> LiteLlm {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .unwrap_or_default();
        LiteLlm {
            http,
            url,
            key,
            model,
            retry_delay: Duration::from_secs(2),
        }
    }
}

#[async_trait]
impl Llm for LiteLlm {
    /// cancel-safe: yes — a chat completion has no side effects on our state.
    async fn call_tool(
        &self,
        messages: &[ChatMessage],
        tool: &ToolSpec,
    ) -> anyhow::Result<LlmOutcome> {
        // Forced tool choice is not supported for some models behind LiteLLM,
        // so the prompt asks for the call and `auto` lets the model comply.
        let body = json!({
            "model": self.model,
            "messages": messages.iter()
                .map(|m| json!({ "role": m.role, "content": m.content }))
                .collect::<Vec<_>>(),
            "tools": [{ "type": "function", "function": {
                "name": tool.name, "description": tool.description, "parameters": tool.parameters,
            }}],
            "tool_choice": "auto",
            "max_tokens": 8000,
        });
        let url = format!("{}/v1/chat/completions", self.url);
        let mut attempt = 0;
        let resp: Value = loop {
            attempt += 1;
            let sent = self
                .http
                .post(&url)
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await;
            let retryable = match sent {
                Ok(r) if r.status().is_success() => break r.json().await?,
                Ok(r) if r.status().is_server_error() => {
                    format!("LLM returned HTTP {}", r.status().as_u16())
                }
                Ok(r) => {
                    let status = r.status().as_u16();
                    let text: String = r
                        .text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(500)
                        .collect();
                    anyhow::bail!("LLM returned HTTP {status}: {text}");
                }
                Err(e) if e.is_timeout() || e.is_connect() => format!("LLM request failed: {e}"),
                Err(e) => return Err(e.into()),
            };
            if attempt >= 2 {
                anyhow::bail!(retryable);
            }
            tracing::warn!("{retryable}; retrying");
            tokio::time::sleep(self.retry_delay).await;
        };

        let message = &resp["choices"][0]["message"];
        let call = message["tool_calls"]
            .as_array()
            .and_then(|calls| calls.iter().find(|c| c["function"]["name"] == tool.name));
        if let Some(call) = call {
            let args = call["function"]["arguments"].as_str().unwrap_or_default();
            return Ok(match serde_json::from_str::<Value>(args) {
                Ok(v) if v.is_object() => LlmOutcome::ToolCall(v),
                _ => LlmOutcome::Text(args.to_string()),
            });
        }
        Ok(LlmOutcome::Text(
            message["content"].as_str().unwrap_or_default().to_string(),
        ))
    }
}

#[cfg(test)]
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// Returns scripted outcomes in order and records every conversation it saw.
    #[derive(Default)]
    pub struct FakeLlm {
        pub replies: Mutex<VecDeque<anyhow::Result<LlmOutcome>>>,
        pub seen: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl FakeLlm {
        pub fn push_tool(&self, args: Value) {
            self.replies
                .lock()
                .unwrap()
                .push_back(Ok(LlmOutcome::ToolCall(args)));
        }
        pub fn push_text(&self, text: &str) {
            self.replies
                .lock()
                .unwrap()
                .push_back(Ok(LlmOutcome::Text(text.into())));
        }
    }

    #[async_trait]
    impl Llm for FakeLlm {
        async fn call_tool(
            &self,
            messages: &[ChatMessage],
            _tool: &ToolSpec,
        ) -> anyhow::Result<LlmOutcome> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("FakeLlm: no scripted reply left")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;

    use super::*;

    type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

    /// Serves `responses` in order (status, JSON body) at /v1/chat/completions.
    async fn stub(responses: Vec<(u16, Value)>) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let queue = Arc::new(Mutex::new(
            responses
                .into_iter()
                .collect::<std::collections::VecDeque<_>>(),
        ));
        let s = seen.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, body: String| {
                let s = s.clone();
                let queue = queue.clone();
                async move {
                    s.lock()
                        .unwrap()
                        .push((headers, serde_json::from_str(&body).unwrap()));
                    let (status, body) = queue.lock().unwrap().pop_front().unwrap();
                    let r: Response =
                        (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response();
                    r
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}"), seen)
    }

    fn tool() -> ToolSpec {
        ToolSpec {
            name: "submit",
            description: "submit",
            parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        }
    }

    fn completion(message: Value) -> Value {
        json!({"choices": [{"message": message, "finish_reason": "stop"}]})
    }

    fn client(url: String) -> LiteLlm {
        let mut c = LiteLlm::new(url, "k".into(), "m".into());
        c.retry_delay = Duration::from_millis(10);
        c
    }

    #[tokio::test]
    async fn tool_call_parsed() {
        let (url, seen) = stub(vec![(200, completion(json!({"role": "assistant", "content": null,
            "tool_calls": [{"type": "function", "function": {"name": "submit", "arguments": "{\"x\":\"1\"}"}}]})))])
        .await;
        let out = client(url)
            .call_tool(&[ChatMessage::user("hi")], &tool())
            .await
            .unwrap();
        assert_eq!(out, LlmOutcome::ToolCall(json!({"x": "1"})));

        let seen = seen.lock().unwrap();
        let (headers, body) = &seen[0];
        assert_eq!(headers["authorization"], "Bearer k");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["model"], "m");
        assert_eq!(body["tools"][0]["function"]["name"], "submit");
        // Some models behind LiteLLM reject any temperature other than their default.
        assert!(body.get("temperature").is_none(), "{body}");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn text_answer_without_tool_call() {
        let (url, _) = stub(vec![(
            200,
            completion(json!({"role": "assistant", "content": "I think…"})),
        )])
        .await;
        let out = client(url)
            .call_tool(&[ChatMessage::user("hi")], &tool())
            .await
            .unwrap();
        assert_eq!(out, LlmOutcome::Text("I think…".into()));
    }

    #[tokio::test]
    async fn malformed_arguments_become_text() {
        let (url, _) = stub(vec![(200, completion(json!({"role": "assistant",
            "tool_calls": [{"type": "function", "function": {"name": "submit", "arguments": "{oops"}}]})))])
        .await;
        let out = client(url)
            .call_tool(&[ChatMessage::user("hi")], &tool())
            .await
            .unwrap();
        assert_eq!(out, LlmOutcome::Text("{oops".into()));
    }

    #[tokio::test]
    async fn retries_once_on_5xx() {
        let (url, seen) = stub(vec![
            (502, json!({"error": "bad gateway"})),
            (
                200,
                completion(json!({"role": "assistant", "content": "ok"})),
            ),
        ])
        .await;
        let out = client(url)
            .call_tool(&[ChatMessage::user("hi")], &tool())
            .await
            .unwrap();
        assert_eq!(out, LlmOutcome::Text("ok".into()));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn gives_up_after_second_5xx_and_on_4xx() {
        let (url, _) = stub(vec![(500, json!({})), (503, json!({}))]).await;
        assert!(
            client(url)
                .call_tool(&[ChatMessage::user("hi")], &tool())
                .await
                .is_err()
        );
        let (url, seen) = stub(vec![(400, json!({"error": {"message": "bad request"}}))]).await;
        let e = client(url)
            .call_tool(&[ChatMessage::user("hi")], &tool())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("400"), "{e}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
