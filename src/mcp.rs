//! Minimal MCP client for the streamable-HTTP transport: `initialize`, then
//! `tools/call`. Only what talking to Basic Memory needs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MCP server answered HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("MCP error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("unexpected MCP response: {0}")]
    Protocol(String),
}

/// Text content of a tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub is_error: bool,
    pub text: String,
}

pub struct McpClient {
    http: reqwest::Client,
    url: String,
    session: tokio::sync::Mutex<Option<String>>,
    next_id: AtomicU64,
}

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Extracts the JSON-RPC response from a plain JSON or SSE body.
pub fn parse_rpc_body(content_type: &str, body: &str) -> Result<Value, McpError> {
    let response = if content_type.starts_with("text/event-stream") {
        sse_events(body)
            .into_iter()
            .filter_map(|data| serde_json::from_str::<Value>(&data).ok())
            .find(|v| v.get("id").is_some_and(|id| !id.is_null()))
            .ok_or_else(|| McpError::Protocol("event stream carried no response".into()))?
    } else {
        serde_json::from_str(body).map_err(|e| McpError::Protocol(format!("bad JSON: {e}")))?
    };
    if let Some(err) = response.get("error") {
        return Err(McpError::Rpc {
            code: err["code"].as_i64().unwrap_or_default(),
            message: err["message"].as_str().unwrap_or_default().to_string(),
        });
    }
    Ok(response)
}

/// `data:` payloads of each server-sent event, multi-line data joined with `\n`.
fn sse_events(body: &str) -> Vec<String> {
    let mut events = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    for line in body.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty() {
                events.push(data.join("\n"));
                data.clear();
            }
        } else if let Some(d) = line.strip_prefix("data:") {
            data.push(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    events
}

impl McpClient {
    pub fn new(url: String) -> McpClient {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        McpClient {
            http,
            url,
            session: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Calls a tool, re-initialising the session once if the server forgot it.
    ///
    /// cancel-safe: yes — tool calls used by this crate are either reads or are
    /// re-verified by the caller; a dropped call leaves at most a stale session id.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<ToolResult, McpError> {
        let params = json!({ "name": name, "arguments": args });
        let session = self.session().await?;
        let response = match self.request(Some(&session), "tools/call", &params).await {
            // The server restarted or dropped the session: start a new one, once.
            Err(McpError::Status { status: 404, .. }) => {
                self.reset_session(&session).await;
                let session = self.session().await?;
                self.request(Some(&session), "tools/call", &params).await?
            }
            other => other?,
        };
        let result = response
            .and_then(|r| r.get("result").cloned())
            .ok_or_else(|| McpError::Protocol("tools/call returned no result".into()))?;
        let text = result["content"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(ToolResult {
            is_error: result["isError"].as_bool().unwrap_or(false),
            text,
        })
    }

    /// Current session id, initialising a new session if there is none.
    ///
    /// cancel-safe: yes — the id is stored only after `initialize` succeeded; a
    /// cancelled handshake leaves no session and the next call starts over.
    async fn session(&self) -> Result<String, McpError> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "memory-review", "version": env!("CARGO_PKG_VERSION") },
        });
        let (resp, id) = self.send(None, "initialize", Some(&init)).await?;
        if resp.is_none() {
            return Err(McpError::Protocol("initialize returned no result".into()));
        }
        let id = id.ok_or_else(|| McpError::Protocol("server sent no mcp-session-id".into()))?;
        self.send(Some(&id), "notifications/initialized", None)
            .await?;
        *guard = Some(id.clone());
        Ok(id)
    }

    async fn reset_session(&self, stale: &str) {
        let mut guard = self.session.lock().await;
        if guard.as_deref() == Some(stale) {
            *guard = None;
        }
    }

    async fn request(
        &self,
        session: Option<&str>,
        method: &str,
        params: &Value,
    ) -> Result<Option<Value>, McpError> {
        Ok(self.send(session, method, Some(params)).await?.0)
    }

    /// Posts one JSON-RPC message. Requests (with `params`) get an id and a
    /// parsed response; notifications (without) return `None`.
    async fn send(
        &self,
        session: Option<&str>,
        method: &str,
        params: Option<&Value>,
    ) -> Result<(Option<Value>, Option<String>), McpError> {
        let msg = match params {
            Some(p) => json!({
                "jsonrpc": "2.0",
                "id": self.next_id.fetch_add(1, Ordering::Relaxed),
                "method": method,
                "params": p,
            }),
            None => json!({ "jsonrpc": "2.0", "method": method }),
        };
        let mut req = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .json(&msg);
        if let Some(s) = session {
            req = req.header("mcp-session-id", s);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(McpError::Status {
                status: status.as_u16(),
                body: body.chars().take(500).collect(),
            });
        }
        if params.is_none() {
            return Ok((None, session_id));
        }
        Ok((Some(parse_rpc_body(&content_type, &body)?), session_id))
    }
}

#[cfg(test)]
pub mod test_server {
    //! A scripted MCP server for tests.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use serde_json::{Value, json};

    pub type Handler = dyn Fn(&str, &Value) -> Value + Send + Sync;

    #[derive(Clone)]
    pub struct Stub {
        pub inits: Arc<AtomicUsize>,
        /// Session ids that the server pretends to have forgotten.
        pub expired: Arc<Mutex<Vec<String>>>,
        pub calls: Arc<Mutex<Vec<(String, Value)>>>,
        handler: Arc<Handler>,
    }

    /// Starts a server; `handler(tool, args)` returns the tool's text payload as JSON.
    pub async fn start(
        handler: impl Fn(&str, &Value) -> Value + Send + Sync + 'static,
    ) -> (String, Stub) {
        let stub = Stub {
            inits: Arc::new(AtomicUsize::new(0)),
            expired: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
            handler: Arc::new(handler),
        };
        let st = stub.clone();
        let app = Router::new().route(
            "/mcp",
            post(move |headers: HeaderMap, body: String| {
                let st = st.clone();
                async move { st.handle(&headers, &body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}/mcp"), stub)
    }

    fn sse(v: Value) -> Response {
        (
            [("content-type", "text/event-stream")],
            format!("event: message\ndata: {v}\n\n"),
        )
            .into_response()
    }

    impl Stub {
        fn handle(&self, headers: &HeaderMap, body: &str) -> Response {
            let req: Value = serde_json::from_str(body).unwrap();
            let method = req["method"].as_str().unwrap_or_default();
            let id = req["id"].clone();
            match method {
                "initialize" => {
                    let n = self.inits.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut r = sse(json!({"jsonrpc":"2.0","id":id,"result":{
                        "protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stub","version":"0"}}}));
                    r.headers_mut()
                        .insert("mcp-session-id", format!("s{n}").parse().unwrap());
                    r
                }
                "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
                "tools/call" => {
                    let session = headers
                        .get("mcp-session-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    if session.is_empty() {
                        return (StatusCode::BAD_REQUEST, "Missing session ID").into_response();
                    }
                    if self.expired.lock().unwrap().contains(&session) {
                        return (StatusCode::NOT_FOUND, "Session not found").into_response();
                    }
                    let name = req["params"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let args = req["params"]["arguments"].clone();
                    self.calls
                        .lock()
                        .unwrap()
                        .push((name.clone(), args.clone()));
                    let payload = (self.handler)(&name, &args);
                    let is_error = payload.get("__is_error").is_some();
                    let text = if let Some(t) = payload.get("__raw_text") {
                        t.as_str().unwrap_or_default().to_string()
                    } else {
                        payload.to_string()
                    };
                    sse(json!({"jsonrpc":"2.0","id":id,"result":{
                        "content":[{"type":"text","text":text}],"isError":is_error}}))
                }
                other => panic!("unexpected method {other}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse_message() {
        let b = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\n";
        let v = parse_rpc_body("text/event-stream", b).unwrap();
        assert_eq!(v["result"]["content"][0]["text"], "hi");
    }

    #[test]
    fn sse_skips_notifications_before_response() {
        let b = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        let v = parse_rpc_body("text/event-stream; charset=utf-8", b).unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn parses_plain_json() {
        let v = parse_rpc_body(
            "application/json",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"x\":1}}",
        )
        .unwrap();
        assert_eq!(v["result"]["x"], 1);
    }

    #[test]
    fn rpc_error_is_error() {
        let e = parse_rpc_body(
            "application/json",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"bad\"}}",
        )
        .unwrap_err();
        assert!(matches!(e, McpError::Rpc { code: -32602, .. }), "{e}");
    }

    #[test]
    fn sse_without_response_is_error() {
        assert!(parse_rpc_body("text/event-stream", ": ping\n\n").is_err());
    }

    #[tokio::test]
    async fn calls_tool_and_returns_text() {
        let (url, stub) =
            test_server::start(|name, args| json!({"tool": name, "q": args["q"]})).await;
        let c = McpClient::new(url);
        let r = c
            .call_tool("search_notes", json!({"q": "x"}))
            .await
            .unwrap();
        assert!(!r.is_error);
        let v: Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(v["tool"], "search_notes");
        // Second call reuses the session.
        c.call_tool("search_notes", json!({"q": "y"}))
            .await
            .unwrap();
        assert_eq!(stub.inits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reinitializes_on_expired_session() {
        let (url, stub) = test_server::start(|_, _| json!({"ok": true})).await;
        let c = McpClient::new(url);
        c.call_tool("t", json!({})).await.unwrap();
        stub.expired.lock().unwrap().push("s1".into());
        let r = c.call_tool("t", json!({})).await.unwrap();
        assert_eq!(r.text, "{\"ok\":true}");
        assert_eq!(stub.inits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn tool_error_flag_passed_through() {
        let (url, _stub) =
            test_server::start(|_, _| json!({"__is_error": true, "__raw_text": "nope"})).await;
        let r = McpClient::new(url).call_tool("t", json!({})).await.unwrap();
        assert!(r.is_error);
        assert_eq!(r.text, "nope");
    }

    #[tokio::test]
    async fn unreachable_server_is_http_error() {
        let c = McpClient::new("http://127.0.0.1:9/mcp".into());
        assert!(matches!(
            c.call_tool("t", json!({})).await,
            Err(McpError::Http(_))
        ));
    }
}
