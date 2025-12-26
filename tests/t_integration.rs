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
        // 1. Signal the graceful shutdown task
        if let Some(tx) = self.shutdown_tx.take() {
            tx.send(()).ok();
        }
        // 2. Force abort the server task if it doesn't shut down instantly
        self.handle.abort();
    }
}

struct ProxyProcess {
    child: Child,
}

impl ProxyProcess {
    fn spawn(port: u16, upstream: String) -> Self {
        let child = Command::new("./target/release/llm-reasoning-proxy")
            .arg("--port")
            .arg(port.to_string())
            .arg("--upstream")
            .arg(upstream)
            .spawn()
            .expect("Failed to spawn proxy process. Did you run `cargo build --release`?");
        Self { child }
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        // Ensure the process is killed and reaped (prevent zombies/hanging)
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

/// Macro to wrap SSE strings correctly
macro_rules! sse_msg {
    ($msg:expr) => {
        Ok::<_, axum::Error>(bytes::Bytes::from(format!("data: {}\n\n", $msg)))
    };
}

/// Helper to force the connection to close.
///
/// This is critical in tests to prevent the Proxy (client) from hanging,
/// waiting for a Keep-Alive connection that will never send more data.
fn with_connection_close(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// Builds the mock response for Streaming (SSE)
fn build_stream_response() -> Response {
    let stream = async_stream::stream! {
        // Chunk 1: Reasoning
        yield sse_msg!("{\"choices\":[{\"delta\":{\"reasoning_content\":\"Thinking...\"}}]}");
        sleep(Duration::from_millis(50)).await;
        // Chunk 2: End reasoning, Start Content
        yield sse_msg!("{\"choices\":[{\"delta\":{\"content\":\"Answer\"}}]}");
        sleep(Duration::from_millis(50)).await;
        // Chunk 3: Done
        yield sse_msg!("[DONE]");
    };

    let response = Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap();

    with_connection_close(response)
}

/// Builds the mock response for Non-Streaming (JSON)
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
        .body(axum::body::Body::from(json.to_string()))
        .unwrap();

    with_connection_close(response)
}

/// Unified Mock Handler
async fn mock_upstream_handler(
    axum::extract::Json(payload): axum::extract::Json<serde_json::Value>,
) -> Response {
    let is_streaming = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_streaming {
        build_stream_response()
    } else {
        build_json_response()
    }
}

// Test Utilities
// ------------------------------------------------------------

async fn wait_for_port(port: u16) {
    let addr = format!("127.0.0.1:{}", port);
    for _ in 0..20 {
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
async fn test_integration_non_streaming() {
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
        .json(&serde_json::json!({"stream": false, "model": "test"}))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status(), 200);

    let body = resp.text().await.expect("Failed to read body");
    let json: serde_json::Value = serde_json::from_str(&body).expect("Invalid JSON");

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .expect("Missing content");

    // The proxy should transform reasoning_content into content wrapped in <think></think>
    assert_eq!(content, "<think>Hidden Thought</think>Final Answer");
}

#[tokio::test]
#[serial]
async fn test_integration_streaming() {
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
        .json(&serde_json::json!({"stream": true, "model": "test"}))
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

    println!("{}", buffer);

    // Verify Transformation
    assert!(
        buffer.contains("data: {\"choices\":[{\"delta\":{\"content\":\"<think>Thinking...\"}}]}")
    );
    assert!(buffer.contains("</think>Answer"));
    assert!(buffer.contains("[DONE]"));
}
