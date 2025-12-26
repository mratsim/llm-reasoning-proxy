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
use serde::{Deserialize, Serialize};
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
        let path = "./target/release/llm-reasoning-proxy";
        if !std::path::Path::new(path).exists() {
            panic!("Binary not found at {}. Please run `cargo build --release` before running integration tests.", path);
        }

        let child = Command::new(path)
            .arg("--port")
            .arg(port.to_string())
            .arg("--upstream")
            .arg(upstream)
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

// Mock Logic & Handlers
// ------------------------------------------------------------

/// Configuration injected into the request body by the test
/// to tell the Mock Server what to return.
#[derive(Debug, Deserialize)]
struct MockConfig {
    /// If present, return this JSON immediately (non-streaming).
    response_json: Option<serde_json::Value>,
    /// If present, stream these chunks (streaming).
    response_stream: Option<Vec<MockStreamChunk>>,
    /// If present, return this HTTP status code.
    response_status: Option<u16>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct MockStreamChunk {
    /// The SSE data payload (e.g. `{"choices": ...}`).
    /// If this string is just "[DONE]", the stream ends.
    body: String,
    /// Simulated delay before sending this chunk.
    #[serde(default)]
    delay_ms: u64,
}

fn with_connection_close(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

async fn mock_upstream_handler(
    axum::extract::Json(mut payload): axum::extract::Json<serde_json::Value>,
) -> Response {
    // 1. Extract the test configuration from the request body.
    // We assume the proxy passes unknown fields through.
    let config_val = payload
        .as_object_mut()
        .and_then(|obj| obj.remove("__mock_data"));

    let config: MockConfig = match config_val {
        Some(v) => serde_json::from_value(v).unwrap_or_else(|_| MockConfig {
            response_json: None,
            response_stream: None,
            response_status: Some(400), // Bad test config
        }),
        None => MockConfig {
            response_json: None,
            response_stream: None,
            response_status: Some(200), // Default safe fallback
        },
    };

    // 2. Handle Status Code overrides (e.g. testing 500 errors)
    if let Some(status) = config.response_status {
        if status != 200 {
            return Response::builder()
                .status(status)
                .body(axum::body::Body::from("Mock Error"))
                .unwrap();
        }
    }

    // 3. Handle Streaming
    if let Some(chunks) = config.response_stream {
        let stream = async_stream::stream! {
            for chunk in chunks {
                if chunk.delay_ms > 0 {
                    sleep(Duration::from_millis(chunk.delay_ms)).await;
                }

                if chunk.body == "[DONE]" {
                    yield Ok::<_, axum::Error>(bytes::Bytes::from("data: [DONE]\n\n"));
                } else {
                    yield Ok::<_, axum::Error>(bytes::Bytes::from(format!("data: {}\n\n", chunk.body)));
                }
            }
        };

        let response = Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("x-custom-upstream", "valid")
            .body(axum::body::Body::from_stream(stream))
            .unwrap();

        return with_connection_close(response);
    }

    // 4. Handle JSON (Non-streaming)
    if let Some(json_body) = config.response_json {
        let response = Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json_body.to_string()))
            .unwrap();

        return with_connection_close(response);
    }

    // Fallback
    Response::builder()
        .status(404)
        .body(axum::body::Body::from(
            "No mock data provided in __mock_data",
        ))
        .unwrap()
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

    // SCENARIO DEFINITION
    // --------------------------------------------------------
    let upstream_response = serde_json::json!({
        "choices": [{
            "message": {
                "content": "Final Answer",
                "reasoning_content": "Hidden Thought"
            }
        }]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({
            "stream": false,
            "model": "default",
            // We inject the desired behavior into the request body
            "__mock_data": {
                "response_json": upstream_response
            }
        }))
        .send()
        .await
        .expect("Request failed");

    // ASSERTIONS
    // --------------------------------------------------------
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("Failed to parse response");
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();

    // Verify re-wrapping logic
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

    // SCENARIO DEFINITION
    // --------------------------------------------------------
    // Define the specific SSE chunks the upstream should emit
    let stream_chunks = vec![
        MockStreamChunk {
            body: serde_json::json!({"choices": [{"delta": {"reasoning_content": "Thinking..."}}]})
                .to_string(),
            delay_ms: 0,
        },
        MockStreamChunk {
            body: serde_json::json!({"choices": [{"delta": {"content": "Answer"}}]}).to_string(),
            delay_ms: 10,
        },
        MockStreamChunk {
            body: "[DONE]".to_string(),
            delay_ms: 0,
        },
    ];

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({
            "stream": true,
            "model": "default",
            "__mock_data": {
                "response_stream": stream_chunks
            }
        }))
        .send()
        .await
        .expect("Request failed");

    // ASSERTIONS
    // --------------------------------------------------------
    assert_eq!(resp.status(), 200);

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    use futures::StreamExt;

    while let Some(item) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&item.unwrap()));
    }

    // Verify transformations:
    // 1. <think> tag injected on first reasoning chunk
    assert!(
        buffer.contains("data: {\"choices\":[{\"delta\":{\"content\":\"<think>Thinking...\"}}]}")
    );
    // 2. </think> tag closed when content starts
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

    // SCENARIO DEFINITION
    // --------------------------------------------------------
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({
            "stream": false,
            "model": "gpt-4",
            "__mock_data": {
                "response_status": 500
            }
        }))
        .send()
        .await
        .expect("Request failed");

    // ASSERTIONS
    // --------------------------------------------------------
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

    // SCENARIO DEFINITION
    // --------------------------------------------------------
    let upstream_response = serde_json::json!({
        "choices": [{
            "message": { "content": "Just Answer" }
        }]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({
            "stream": false,
            "model": "default",
            "__mock_data": {
                "response_json": upstream_response
            }
        }))
        .send()
        .await
        .expect("Request failed");

    // ASSERTIONS
    // --------------------------------------------------------
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();

    // Verify NO <think> tags injected
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

    // SCENARIO DEFINITION
    // --------------------------------------------------------
    let stream_chunks = vec![
        MockStreamChunk {
            body: serde_json::json!({"choices": [{"delta": {"content": "Hello"}}]}).to_string(),
            delay_ms: 0,
        },
        MockStreamChunk {
            body: serde_json::json!({"choices": [{"delta": {"content": " World"}}]}).to_string(),
            delay_ms: 10,
        },
        MockStreamChunk {
            body: "[DONE]".to_string(),
            delay_ms: 0,
        },
    ];

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            PROXY_PORT
        ))
        .json(&serde_json::json!({
            "stream": true,
            "model": "default",
            "__mock_data": {
                "response_stream": stream_chunks
            }
        }))
        .send()
        .await
        .expect("Request failed");

    // ASSERTIONS
    // --------------------------------------------------------
    assert_eq!(resp.status(), 200);

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    use futures::StreamExt;

    while let Some(item) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&item.unwrap()));
    }

    // Verify standard streaming behavior without modification
    assert!(buffer.contains("Hello"));
    assert!(buffer.contains(" World"));
    assert!(!buffer.contains("<think>"));
    assert!(buffer.contains("[DONE]"));
}
