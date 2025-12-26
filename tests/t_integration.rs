//! t_integration.rs
//! Copyright (c) 2025-Present Mamy André-Ratsimbazafy
//! Licensed and distributed under either of
//!   * MIT license (license terms in the root directory or at http://opensource.org/licenses/MIT).
//!   * Apache v2 license (license terms in the root directory or at http://www.apache.org/licenses/LICENSE-2.0).
//! at your option. This file may not be copied, modified, or distributed except according to those terms.

// Imports
// ------------------------------------------------------------
use std::net::SocketAddr;
use std::process::{Child, Command};
use std::time::Duration;

use axum::http::header;
use axum::http::HeaderValue;
use axum::response::Response;
use axum::Router;
use serial_test::serial;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::sleep;

// Mockingjay
// ------------------------------------------------------------
struct MockServer {
    _addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl MockServer {
    async fn start(port: u16) -> Self {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = TcpListener::bind(addr)
            .await
            .expect("Failed to bind mock upstream port");
        let addr = listener.local_addr().unwrap();

        let app = Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(mock_upstream_handler),
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    shutdown_rx.await.ok();
                })
                .await
                .unwrap();
        });

        Self {
            _addr: addr,
            shutdown_tx: Some(shutdown_tx),
            handle,
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            tx.send(()).ok();
        }
        self.handle.abort();
    }
}

struct ProxyProcess {
    child: Child,
}

impl ProxyProcess {
    fn spawn(port: u16, upstream: String) -> Self {
        // Ensure the binary exists or provide a helpful error
        let path = "./target/release/llm-reasoning-proxy";
        if !std::path::Path::new(path).exists() {
            panic!("Binary not found at {}. Please run `cargo build --release` before running integration tests.", path);
        }

        let child = Command::new(path)
            .arg("--port")
            .arg(port.to_string())
            .arg("--upstream")
            .arg(upstream)
            // Silence stdout during tests unless debugging
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("Failed to spawn proxy process.");
        Self { child }
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        if let Ok(_) = self.child.kill() {
            let _ = self.child.wait();
        }
    }
}

// Constants
// ------------------------------------------------------------
const PROXY_PORT: u16 = 5555;
const MOCK_UPSTREAM_PORT: u16 = 5556;

// Mock Response Builders
// ------------------------------------------------------------

macro_rules! sse_msg {
    ($msg:expr) => {
        Ok::<_, axum::Error>(bytes::Bytes::from(format!("data: {}\n\n", $msg)))
    };
}

fn with_connection_close(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// Scenario: Standard Reasoning Stream
fn build_stream_response() -> Response {
    let stream = async_stream::stream! {
        yield sse_msg!("{\"choices\":[{\"delta\":{\"reasoning_content\":\"Thinking...\"}}]}");
        sleep(Duration::from_millis(10)).await;
        yield sse_msg!("{\"choices\":[{\"delta\":{\"content\":\"Answer\"}}]}");
        sleep(Duration::from_millis(10)).await;
        yield sse_msg!("[DONE]");
    };

    let response = Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("x-custom-upstream", "valid") // Header to test propagation
        .body(axum::body::Body::from_stream(stream))
        .unwrap();

    with_connection_close(response)
}

/// Scenario: Standard Reasoning JSON
fn build_json_response() -> Response {
    let json = serde_json::json!({
        "choices": [{
            "message": {
                "content": "Final Answer",
                "reasoning_content": "Hidden Thought"
            }
        }]
    });

    let response = Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("x-custom-upstream", "valid")
        .body(axum::body::Body::from(json.to_string()))
        .unwrap();

    with_connection_close(response)
}

/// Scenario: JSON with NO reasoning (Pass-through check)
fn build_json_no_reasoning_response() -> Response {
    let json = serde_json::json!({
        "choices": [{
            "message": { "content": "Just Answer" }
        }]
    });

    let response = Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(json.to_string()))
        .unwrap();

    with_connection_close(response)
}

/// Scenario: Streaming with NO reasoning (Pass-through check)
fn build_stream_no_reasoning_response() -> Response {
    let stream = async_stream::stream! {
        // Chunk 1
        yield sse_msg!("{\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}");
        sleep(Duration::from_millis(10)).await;
        // Chunk 2
        yield sse_msg!("{\"choices\":[{\"delta\":{\"content\":\" World\"}}]}");
        sleep(Duration::from_millis(10)).await;
        // Done
        yield sse_msg!("[DONE]");
    };

    let response = Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap();

    with_connection_close(response)
}

async fn mock_upstream_handler(
    axum::extract::Json(payload): axum::extract::Json<serde_json::Value>,
) -> Response {
    let is_streaming = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    match model {
        "error-500" => Response::builder()
            .status(500)
            .body(axum::body::Body::from("Internal Server Error"))
            .unwrap(),
        "no-reasoning" => build_json_no_reasoning_response(),
        "stream-no-reasoning" => build_stream_no_reasoning_response(), // <--- NEW CASE
        _ => {
            if is_streaming {
                build_stream_response()
            } else {
                build_json_response()
            }
        }
    }
}

// Test Utilities
// ------------------------------------------------------------

async fn wait_for_port(port: u16) {
    let addr = format!("127.0.0.1:{}", port);
    for _ in 0..50 {
        // 5 seconds max
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("Timeout waiting for port {}", port);
}

// Tests
// ------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_non_streaming_reasoning() {
    let _mock_server = MockServer::start(MOCK_UPSTREAM_PORT).await;
    let _proxy = ProxyProcess::spawn(
        PROXY_PORT,
        format!("http://127.0.0.1:{}", MOCK_UPSTREAM_PORT),
    );
    wait_for_port(PROXY_PORT).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({"stream": false, "model": "default"}))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status(), 200);

    // Verify Header Propagation
    assert_eq!(
        resp.headers()
            .get("x-custom-upstream")
            .unwrap()
            .to_str()
            .unwrap(),
        "valid"
    );

    let body = resp.text().await.expect("Failed to read body");
    let json: serde_json::Value = serde_json::from_str(&body).expect("Invalid JSON");

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .expect("Missing content");

    assert_eq!(content, "<think>Hidden Thought</think>Final Answer");
}

#[tokio::test]
#[serial]
async fn test_reasoning_streaming() {
    let _mock_server = MockServer::start(MOCK_UPSTREAM_PORT).await;
    let _proxy = ProxyProcess::spawn(
        PROXY_PORT,
        format!("http://127.0.0.1:{}", MOCK_UPSTREAM_PORT),
    );
    wait_for_port(PROXY_PORT).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({"stream": true, "model": "default"}))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status(), 200);

    // Verify Header Propagation in stream
    assert_eq!(
        resp.headers()
            .get("x-custom-upstream")
            .unwrap()
            .to_str()
            .unwrap(),
        "valid"
    );

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    use futures::StreamExt;
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        let s = String::from_utf8_lossy(&chunk);
        buffer.push_str(&s);
    }

    // Verify Transformation Logic
    // We expect the proxy to inject <think> on the first reasoning chunk
    assert!(
        buffer.contains("data: {\"choices\":[{\"delta\":{\"content\":\"<think>Thinking...\"}}]}")
    );
    // We expect the proxy to close </think> when content starts
    assert!(buffer.contains("</think>Answer"));
    assert!(buffer.contains("[DONE]"));
}

#[tokio::test]
#[serial]
async fn test_upstream_error_propagation() {
    let _mock_server = MockServer::start(MOCK_UPSTREAM_PORT).await;
    let _proxy = ProxyProcess::spawn(
        PROXY_PORT,
        format!("http://127.0.0.1:{}", MOCK_UPSTREAM_PORT),
    );
    wait_for_port(PROXY_PORT).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({"stream": false, "model": "error-500"}))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status(), 500);
}

#[tokio::test]
#[serial]
async fn test_passthrough_no_reasoning_json() {
    let _mock_server = MockServer::start(MOCK_UPSTREAM_PORT).await;
    let _proxy = ProxyProcess::spawn(
        PROXY_PORT,
        format!("http://127.0.0.1:{}", MOCK_UPSTREAM_PORT),
    );
    wait_for_port(PROXY_PORT).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({"stream": false, "model": "no-reasoning"}))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status(), 200);

    let body = resp.text().await.expect("Failed to read body");
    let json: serde_json::Value = serde_json::from_str(&body).expect("Invalid JSON");

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .expect("Missing content");

    // Should NOT contain <think> tags, just the original content
    assert_eq!(content, "Just Answer");
}

#[tokio::test]
#[serial]
async fn test_passthrough_no_reasoning_streaming() {
    let _mock_server = MockServer::start(MOCK_UPSTREAM_PORT).await;
    let _proxy = ProxyProcess::spawn(
        PROXY_PORT,
        format!("http://127.0.0.1:{}", MOCK_UPSTREAM_PORT),
    );
    wait_for_port(PROXY_PORT).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({
            "stream": true,
            "model": "stream-no-reasoning"
        }))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status(), 200);

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    use futures::StreamExt;
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        let s = String::from_utf8_lossy(&chunk);
        buffer.push_str(&s);
    }

    // Verify:
    // 1. Data arrived
    assert!(buffer.contains("Hello"));
    assert!(buffer.contains(" World"));

    // 2. No <think> tags were injected
    assert!(!buffer.contains("<think>"));
    assert!(!buffer.contains("</think>"));

    // 3. Stream ended correctly
    assert!(buffer.contains("[DONE]"));
}
