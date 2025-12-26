//! llm-reasoning-proxy
//! Copyright (c) 2025-Present Mamy André-Ratsimbazafy
//! Licensed and distributed under either of
//!   * MIT license (license terms in the root directory or at http://opensource.org/licenses/MIT).
//!   * Apache v2 license (license terms in the root directory or at http://www.apache.org/licenses/LICENSE-2.0).
//! at your option. This file may not be copied, modified, or distributed except according to those terms.

//! main.rs
// ------------------------------------------------------------

// Imports
// ------------------------------------------------------------

use futures::{Stream, StreamExt, TryStreamExt};
use std::pin::Pin;

use bytes::Bytes;
use serde_json::Value;
use tracing::{error, info};

use clap::Parser;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use http_body_util::BodyExt;
use reqwest::Client;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use logic::StreamState;

// Types
// ------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "5001")]
    port: u16,

    /// Upstream API URL
    #[arg(short, long, default_value = "http://127.0.0.1:5000")]
    upstream: String,
}

#[derive(Clone)]
struct AppState {
    client: Client,
    upstream_url: String,
}

type PinBoxStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

// Main
// ------------------------------------------------------------

#[tokio::main]
async fn main() {
    let args = Args::parse();
    tracing_subscriber::fmt::init();

    info!(
        "Starting Chat Completion Proxy on port {} forwarding to {}",
        args.port, args.upstream
    );

    let state = AppState {
        client: Client::new(),
        upstream_url: args.upstream,
    };

    let app = Router::new()
        .route("/v1/models", any(proxy_handler))
        .route("/v1/chat/completions", any(proxy_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", args.port))
        .await
        .expect("Failed to bind to port");
    axum::serve(listener, app).await.expect("Error running server");
}

// Networking
// ------------------------------------------------------------

async fn proxy_handler(
    State(state): State<AppState>,
    mut req: Request,
) -> Result<Response, StatusCode> {
    // 1. Read body to determine streaming state
    let body_bytes = req
        .body_mut()
        .collect()
        .await
        .map_err(|e| {
            error!("Request body read error: {}", e);
            StatusCode::BAD_REQUEST
        })?
        .to_bytes();

    let is_streaming = logic::is_streaming(&body_bytes);

    // 2. Construct Upstream Request
    let upstream_uri = format!(
        "{}{}",
        state.upstream_url,
        req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
    );

    let upstream_req = state
        .client
        .request(req.method().clone(), &upstream_uri)
        .body(body_bytes); // Body implies cloning bytes internally, which is cheap (Arc)

    // Apply headers
    let upstream_req = prepare_upstream_headers(req.headers(), upstream_req);

    info!(
        "Proxying {} -> {} (stream: {})",
        req.uri().path(),
        upstream_uri,
        is_streaming
    );

    // 3. Send Request
    let upstream_resp = upstream_req.send().await.map_err(|e| {
        error!("Upstream request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    // 4. Build Response
    let status = upstream_resp.status();
    let mut resp_builder = Response::builder().status(status);

    // Copy headers from upstream to downstream
    resp_builder = copy_response_headers(upstream_resp.headers(), resp_builder);

    if is_streaming {
        // SSE Case
        let stream = process_sse_stream(upstream_resp.bytes_stream());
        resp_builder
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .map_err(|e| {
                error!("Failed to build stream response: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
    } else {
        // Standard JSON Case
        let text = upstream_resp.text().await.map_err(|e| {
            error!("Response body read error: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

        let modified = logic::transform_non_streaming(&text);

        resp_builder
            .header("content-type", "application/json")
            // Recalculate length after modification
            .header("content-length", modified.len())
            .body(Body::from(modified))
            .map_err(|e| {
                error!("Failed to build json response: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
    }
}

// Helpers
// ------------------------------------------------------------

/// Filters and maps headers from Client -> Upstream
fn prepare_upstream_headers(
    headers: &HeaderMap,
    builder: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    let mut builder = builder;
    for (name, value) in headers {
        let n = name.as_str();
        if matches!(n, "host" | "content-length" | "connection") {
            continue;
        }
        if matches!(
            n,
            "authorization" | "content-type" | "user-agent" | "accept"
        ) || n.starts_with("x-")
        {
            builder = builder.header(name, value);
        }
    }
    builder
}

/// Filters and maps headers from Upstream -> Client
fn copy_response_headers(
    headers: &HeaderMap,
    builder: axum::http::response::Builder,
) -> axum::http::response::Builder {
    let mut builder = builder;
    for (name, value) in headers {
        // Don't copy transfer-encoding (axum handles chunks) or length (we might change it)
        if !matches!(
            name.as_str(),
            "transfer-encoding" | "content-encoding" | "content-length"
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
}

/// Processes the SSE stream
fn process_sse_stream(
    upstream_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> PinBoxStream {
    let reader = StreamReader::new(upstream_stream.map_err(std::io::Error::other));
    let lines = FramedRead::new(reader, LinesCodec::new());

    Box::pin(async_stream::try_stream! {
        let mut state = StreamState::default();
        tokio::pin!(lines);

        while let Some(line_result) = lines.next().await {
            let line = line_result.map_err(axum::Error::new)?;
            let trimmed = line.trim();

            if trimmed.is_empty() {
                continue;
            }

            // 1. Handle Done
            if trimmed == "data: [DONE]" {
                yield "data: [DONE]\n\n".into();
                break;
            }

            // 2. Handle Data
            if let Some(json_str) = trimmed.strip_prefix("data: ") {
                match serde_json::from_str::<Value>(json_str) {
                    Ok(mut val) => {
                        logic::transform_stream_chunk(&mut val, &mut state);
                        yield format!("data: {}\n\n", val).into();
                    }
                    Err(e) => {
                        // Log parsing error but keep stream alive or send error frame
                        error!("JSON Parse Error: {}. Chunk: {}", e, json_str);
                        yield format!("data: {{\"error\": \"Proxy Parse Error\"}}\n\n").into();
                    }
                }
            } else {
                // 3. Pass through comments or other events unmodified
                yield format!("{}\n", trimmed).into();
            }
        }
    })
}

// Transformers are all you need
// ------------------------------------------------------------

mod logic {
    use serde_json::Value;

    #[derive(Default)]
    pub struct StreamState {
        pub is_thinking: bool,
    }

    pub fn is_streaming(body: &[u8]) -> bool {
        serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|json| json.get("stream").and_then(|v| v.as_bool()))
            .unwrap_or(false)
    }

    pub fn transform_non_streaming(json_str: &str) -> String {
        if let Ok(mut json) = serde_json::from_str::<Value>(json_str) {
            // Navigate to choices -> [i] -> message
            if let Some(message) = json
                .get_mut("choices")
                .and_then(|v| v.as_array_mut())
                .and_then(|choices| choices.first_mut())
                .and_then(|choice| choice.get_mut("message"))
                .and_then(|v| v.as_object_mut())
            {
                transform_message(message);
            }
            return json.to_string();
        }
        json_str.to_string()
    }

    pub fn transform_message(msg: &mut serde_json::Map<String, Value>) {
        if let Some(r_val) = msg.remove("reasoning_content") {
            let wrapped = format!("<think>{}</think>", r_val.as_str().unwrap_or(""));
            let existing_content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let new_content = format!("{}{}", wrapped, existing_content);
            msg.insert("content".to_string(), serde_json::json!(new_content));
        }
    }

    pub fn transform_stream_chunk(val: &mut Value, state: &mut StreamState) {
        let Some(delta) = get_mut_delta(val) else {
            return;
        };

        // 1. Extract values
        let r_val_opt = delta.remove("reasoning_content");
        let r_str = r_val_opt.as_ref().and_then(|v| v.as_str()).unwrap_or("");

        let c_str = delta.get("content").and_then(|v| v.as_str()).unwrap_or("");

        // 2. Early return if nothing to do
        if r_str.is_empty() && c_str.is_empty() {
            return;
        }

        let mut output = String::with_capacity(r_str.len() + c_str.len() + 16);

        // 3. Handle Reasoning
        if !r_str.is_empty() {
            if !state.is_thinking {
                output.push_str("<think>");
                state.is_thinking = true;
            }
            output.push_str(r_str);
        }

        // 4. Handle Content
        if !c_str.is_empty() {
            if state.is_thinking {
                output.push_str("</think>");
                state.is_thinking = false;
            }
            output.push_str(c_str);
        }

        // 5. Save result
        delta.insert("content".to_string(), output.into());
    }

    // Helper to flatten deep navigation, handling mutable array access correctly
    fn get_mut_delta(val: &mut Value) -> Option<&mut serde_json::Map<String, Value>> {
        val.get_mut("choices")?
            .as_array_mut()?
            .get_mut(0)?
            .get_mut("delta")?
            .as_object_mut()
    }
}

// --- Tests ---
#[cfg(test)]
mod tests {
    use super::logic::*;
    use serde_json::Value;

    #[test]
    fn test_transform_non_streaming_with_reasoning() {
        let input = r#"{
            "choices": [{"message": {"content": "Hello", "reasoning_content": "Thinking"}}]
        }"#;
        let output = transform_non_streaming(input);
        assert!(output.contains("<think>Thinking</think>"));
        assert!(output.contains("Hello"));
    }

    #[test]
    fn test_transform_non_streaming_no_reasoning() {
        let input = r#"{"choices": [{"message": {"content": "Hello"}}]}"#;
        let output = transform_non_streaming(input);

        // Compare parsed values to ignore whitespace/formatting differences
        let parsed_output: Value =
            serde_json::from_str(&output).expect("Output was not valid JSON");
        let parsed_input: Value = serde_json::from_str(input).expect("Input was not valid JSON");

        assert_eq!(parsed_output, parsed_input);
    }

    #[test]
    fn test_stream_chunk_start_thinking() {
        let mut state = StreamState::default();
        let mut val = serde_json::json!({
            "choices": [{"delta": {"reasoning_content": "Hmm"}}]
        });
        transform_stream_chunk(&mut val, &mut state);

        assert!(state.is_thinking);
        let content = val["choices"][0]["delta"]["content"].as_str().unwrap();
        assert_eq!(content, "<think>Hmm");
    }

    #[test]
    fn test_stream_chunk_switch_to_content() {
        let mut state = StreamState { is_thinking: true };
        let mut val = serde_json::json!({
            "choices": [{"delta": {"content": "Answer"}}]
        });
        transform_stream_chunk(&mut val, &mut state);

        assert!(!state.is_thinking);
        let content = val["choices"][0]["delta"]["content"].as_str().unwrap();
        assert_eq!(content, "</think>Answer");
    }

    #[test]
    fn test_is_streaming() {
        let json = serde_json::json!({"stream": true}).to_string();
        assert!(is_streaming(json.as_bytes()));

        let json2 = serde_json::json!({"stream": false}).to_string();
        assert!(!is_streaming(json2.as_bytes()));
    }
}
