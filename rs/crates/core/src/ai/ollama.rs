//! Port of `src/main/ai/ollama-client.ts` — a minimal Ollama HTTP client via `reqwest`:
//! per-request timeout, `clean_line` (80-char clamp), `normalize_endpoint`, and `ping` that
//! NEVER errors (returns a bool, swallows failures). Mirror `ollama-client.test.ts`.
//! (The user's Ollama lives at 192.168.0.11 / a gemma model — endpoint + model stay
//! configurable.)
//!
//! No app-state coupling: construct with config, call methods. `summarize` returns
//! `Err` on any failure (the caller owns retry/backoff); `ping` never errors.

use serde_json::json;
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 12000;
const PING_TIMEOUT_MS: u64 = 3000;
const MAX_SUMMARY_CHARS: usize = 80;

#[derive(Debug, Clone)]
pub struct OllamaConfig {
    pub endpoint: String,
    pub model: String,
    pub timeout_ms: Option<u64>, // default 12000
}

#[derive(Debug, Clone, Default)]
pub struct SummarizeInput {
    pub system: String,
    pub prompt: String,
}

/// Partial patch for [`OllamaClient::configure`]; `None` fields are left as-is.
#[derive(Debug, Clone, Default)]
pub struct OllamaPatch {
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub timeout_ms: Option<u64>,
}

// Strip trailing slashes so `http://host:11434/` and `http://host:11434` behave
// identically.
fn normalize_endpoint(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_string()
}

// Take the first non-empty line, collapse internal whitespace, clamp length.
fn clean_line(raw: &str) -> String {
    let line = raw
        .split('\n')
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_SUMMARY_CHARS).collect()
}

// `Clone` is cheap: `reqwest::Client` is internally an `Arc` over a shared
// connection pool, so a clone shares pooling rather than re-allocating. The
// ambient-AI engine clones a client per off-loop summary job (see
// `AiService::prepare_job`) so a slow HTTP call can run without holding the
// engine's `&mut self`.
#[derive(Clone)]
pub struct OllamaClient {
    endpoint: String,
    model: String,
    timeout_ms: u64,
    client: reqwest::Client,
}

impl OllamaClient {
    pub fn new(cfg: OllamaConfig) -> Self {
        Self {
            endpoint: normalize_endpoint(&cfg.endpoint),
            model: cfg.model,
            timeout_ms: cfg.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            client: reqwest::Client::new(),
        }
    }

    /// Live-update endpoint/model/timeout. Untouched fields stay as-is.
    pub fn configure(&mut self, patch: OllamaPatch) {
        if let Some(endpoint) = patch.endpoint {
            self.endpoint = normalize_endpoint(&endpoint);
        }
        if let Some(model) = patch.model {
            self.model = model;
        }
        if let Some(timeout_ms) = patch.timeout_ms {
            self.timeout_ms = timeout_ms;
        }
    }

    /// POST `{endpoint}/api/generate` and reduce `.response` to a clean one-liner.
    /// Returns `Err` on network error, timeout, non-2xx, or missing/empty response.
    pub async fn summarize(&self, input: &SummarizeInput) -> Result<String, String> {
        let url = format!("{}/api/generate", self.endpoint);
        let body = json!({
            "model": self.model,
            "system": input.system,
            "prompt": input.prompt,
            "stream": false,
            "options": { "temperature": 0.2, "num_predict": 64, "top_p": 0.9 }
        });
        let res = self
            .client
            .post(&url)
            .json(&body)
            .timeout(Duration::from_millis(self.timeout_ms))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !res.status().is_success() {
            return Err(format!("ollama {}", res.status().as_u16()));
        }

        let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
        let raw = data.get("response").and_then(|v| v.as_str()).unwrap_or("");
        let summary = clean_line(raw);
        if summary.is_empty() {
            return Err("ollama: empty response".to_string());
        }
        Ok(summary)
    }

    /// GET `{endpoint}/api/tags` as a reachability check. Returns a bool; never errors.
    pub async fn ping(&self) -> bool {
        let url = format!("{}/api/tags", self.endpoint);
        match self
            .client
            .get(&url)
            .timeout(Duration::from_millis(PING_TIMEOUT_MS))
            .send()
            .await
        {
            Ok(res) => res.status().is_success(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    // A captured HTTP request: its start-line path and parsed JSON body (if any).
    struct CapturedReq {
        raw: String,
    }

    impl CapturedReq {
        fn path(&self) -> &str {
            // start-line: "METHOD PATH HTTP/1.1"
            self.raw.split("\r\n").next().unwrap().split(' ').nth(1).unwrap()
        }
        fn method(&self) -> &str {
            self.raw.split(' ').next().unwrap()
        }
        fn header(&self, name: &str) -> Option<String> {
            let lname = name.to_ascii_lowercase();
            let head = self.raw.split("\r\n\r\n").next().unwrap();
            head.split("\r\n")
                .skip(1)
                .find(|l| l.to_ascii_lowercase().starts_with(&format!("{lname}:")))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string())
        }
        fn body_json(&self) -> serde_json::Value {
            let body = self.raw.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
            serde_json::from_str(body).unwrap()
        }
    }

    async fn read_request(sock: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            let n = match sock.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            if let Some(hdr_end) = find_sub(&buf, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..hdr_end]).to_ascii_lowercase();
                let clen = headers
                    .split("\r\n")
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= hdr_end + 4 + clen {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    // Bind a server that handles exactly one request, replies with `status`/`body`,
    // and yields the captured request. Returns (port, JoinHandle).
    async fn serve_once(
        status: u16,
        reason: &str,
        body: String,
    ) -> (u16, tokio::task::JoinHandle<CapturedReq>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let raw = read_request(&mut sock).await;
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            CapturedReq { raw }
        });
        (port, handle)
    }

    fn client_for(port: u16, model: &str) -> OllamaClient {
        OllamaClient::new(OllamaConfig {
            endpoint: format!("http://127.0.0.1:{port}"),
            model: model.to_string(),
            timeout_ms: None,
        })
    }

    fn input() -> SummarizeInput {
        SummarizeInput {
            system: "sys".into(),
            prompt: "p".into(),
        }
    }

    #[tokio::test]
    async fn parses_response_trims_and_returns_a_clean_single_line() {
        let (port, h) = serve_once(200, "OK", r#"{"response":"  rebased onto main, 3 commits  "}"#.into()).await;
        let client = client_for(port, "gemma");
        assert_eq!(
            client.summarize(&input()).await.unwrap(),
            "rebased onto main, 3 commits"
        );
        h.await.unwrap();
    }

    #[tokio::test]
    async fn takes_first_non_empty_line_and_collapses_internal_whitespace() {
        let body = json!({"response":"\n\n   running   tests\twith   gaps\nsecond line\nthird"}).to_string();
        let (port, h) = serve_once(200, "OK", body).await;
        let client = client_for(port, "gemma");
        assert_eq!(client.summarize(&input()).await.unwrap(), "running tests with gaps");
        h.await.unwrap();
    }

    #[tokio::test]
    async fn clamps_a_long_line_to_80_chars() {
        let body = json!({"response": "x".repeat(200)}).to_string();
        let (port, h) = serve_once(200, "OK", body).await;
        let client = client_for(port, "gemma");
        let out = client.summarize(&input()).await.unwrap();
        assert!(out.chars().count() <= 80);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn posts_to_api_generate_with_the_frozen_body_and_options_shape() {
        let (port, h) = serve_once(200, "OK", r#"{"response":"ok"}"#.into()).await;
        let client = client_for(port, "gemma");
        client
            .summarize(&SummarizeInput {
                system: "you are terse".into(),
                prompt: "summarize this".into(),
            })
            .await
            .unwrap();
        let req = h.await.unwrap();
        assert_eq!(req.method(), "POST");
        assert_eq!(req.path(), "/api/generate");
        assert_eq!(req.header("content-type").as_deref(), Some("application/json"));
        assert_eq!(
            req.body_json(),
            json!({
                "model": "gemma",
                "system": "you are terse",
                "prompt": "summarize this",
                "stream": false,
                "options": { "temperature": 0.2, "num_predict": 64, "top_p": 0.9 }
            })
        );
    }

    #[tokio::test]
    async fn errors_on_a_non_2xx_response() {
        let (port, h) = serve_once(500, "Internal Server Error", r#"{"error":"boom"}"#.into()).await;
        let client = client_for(port, "gemma");
        let err = client.summarize(&input()).await.unwrap_err();
        assert!(err.contains("ollama 500"), "got: {err}");
        h.await.unwrap();
    }

    #[tokio::test]
    async fn errors_when_the_connection_fails() {
        // Port 1 is reserved and refuses; mirrors a fetch network rejection.
        let client = client_for(1, "gemma");
        assert!(client.summarize(&input()).await.is_err());
    }

    #[tokio::test]
    async fn errors_when_response_is_missing() {
        let (port, h) = serve_once(200, "OK", r#"{"done":true}"#.into()).await;
        let client = client_for(port, "gemma");
        assert!(client.summarize(&input()).await.is_err());
        h.await.unwrap();
    }

    #[tokio::test]
    async fn errors_when_response_is_empty_or_whitespace_only() {
        let body = json!({"response":"   \n\t "}).to_string();
        let (port, h) = serve_once(200, "OK", body).await;
        let client = client_for(port, "gemma");
        assert!(client.summarize(&input()).await.is_err());
        h.await.unwrap();
    }

    #[tokio::test]
    async fn normalizes_a_trailing_slash_on_the_endpoint() {
        let (port, h) = serve_once(200, "OK", r#"{"response":"ok"}"#.into()).await;
        let client = OllamaClient::new(OllamaConfig {
            endpoint: format!("http://127.0.0.1:{port}/"),
            model: "gemma".into(),
            timeout_ms: None,
        });
        client.summarize(&input()).await.unwrap();
        let req = h.await.unwrap();
        // No double slash: a non-normalized endpoint would yield "//api/generate".
        assert_eq!(req.path(), "/api/generate");
    }

    #[tokio::test]
    async fn honors_a_small_per_request_timeout() {
        // A server that accepts but never replies; a tiny timeout must abort the call.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _keep = tokio::spawn(async move {
            let _accepted = listener.accept().await; // hold the connection open, never respond
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let mut client = client_for(port, "gemma");
        client.configure(OllamaPatch {
            timeout_ms: Some(150),
            ..Default::default()
        });
        assert!(client.summarize(&input()).await.is_err());
    }

    #[tokio::test]
    async fn configure_live_updates_endpoint_and_model_used_by_the_next_summarize() {
        let (port, h) = serve_once(200, "OK", r#"{"response":"ok"}"#.into()).await;
        // Start pointed at a dead port, then configure to the live server + new model.
        let mut client = client_for(1, "gemma");
        client.configure(OllamaPatch {
            endpoint: Some(format!("http://127.0.0.1:{port}/")),
            model: Some("llama3".into()),
            ..Default::default()
        });
        client.summarize(&input()).await.unwrap();
        let req = h.await.unwrap();
        assert_eq!(req.path(), "/api/generate");
        assert_eq!(req.body_json()["model"], "llama3");
    }

    #[tokio::test]
    async fn configure_leaves_untouched_fields_intact_when_patching_partially() {
        let (port, h) = serve_once(200, "OK", r#"{"response":"ok"}"#.into()).await;
        let mut client = client_for(port, "gemma");
        client.configure(OllamaPatch {
            model: Some("llama3".into()),
            ..Default::default()
        });
        client.summarize(&input()).await.unwrap();
        let req = h.await.unwrap();
        assert_eq!(req.path(), "/api/generate"); // endpoint unchanged
        assert_eq!(req.body_json()["model"], "llama3");
    }

    #[tokio::test]
    async fn ping_gets_api_tags_and_returns_true_on_2xx() {
        let (port, h) = serve_once(200, "OK", r#"{"models":[]}"#.into()).await;
        let client = OllamaClient::new(OllamaConfig {
            endpoint: format!("http://127.0.0.1:{port}/"),
            model: "gemma".into(),
            timeout_ms: None,
        });
        assert!(client.ping().await);
        let req = h.await.unwrap();
        assert_eq!(req.path(), "/api/tags");
        assert_eq!(req.method(), "GET");
    }

    #[tokio::test]
    async fn ping_returns_false_on_non_2xx_without_erroring() {
        let (port, h) = serve_once(503, "Service Unavailable", "{}".into()).await;
        let client = client_for(port, "gemma");
        assert!(!client.ping().await);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn ping_returns_false_never_errors_when_connection_fails() {
        let client = client_for(1, "gemma");
        assert!(!client.ping().await);
    }

    #[test]
    fn normalize_endpoint_strips_trailing_slashes() {
        assert_eq!(normalize_endpoint("http://h:11434/"), "http://h:11434");
        assert_eq!(normalize_endpoint("http://h:11434"), "http://h:11434");
        assert_eq!(normalize_endpoint("http://h:11434///"), "http://h:11434");
    }

    #[test]
    fn clean_line_first_nonempty_collapse_and_clamp() {
        assert_eq!(clean_line("  hi  there  "), "hi there");
        assert_eq!(clean_line("\n\n  one\ntwo"), "one");
        assert_eq!(clean_line(""), "");
        assert_eq!(clean_line(&"y".repeat(200)).chars().count(), 80);
    }
}
