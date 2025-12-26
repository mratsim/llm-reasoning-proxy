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
use tracing::{debug, error, info};

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
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

// Networking
// ------------------------------------------------------------

async fn proxy_handler(
    State(state): State<AppState>,
    mut req: Request,
) -> Result<Response, StatusCode> {
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
    let upstream_uri = format!(
        "{}{}",
        state.upstream_url,
        req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
    );

    let mut upstream_req = state.client.request(req.method().clone(), &upstream_uri);

    // Pass the builder and get it back
    upstream_req = filter_headers(req.headers(), upstream_req);
    upstream_req = upstream_req.body(body_bytes.clone());

    info!(
        "Proxying {} to {} (stream: {})",
        req.uri().path(),
        upstream_uri,
        is_streaming
    );

    match upstream_req.send().await {
        Ok(upstream_resp) => {
            let status = upstream_resp.status();
            info!("Upstream response status: {}", status);

            let mut resp_builder = Response::builder().status(status);

            // Proxy headers (excluding problematic ones, content-length might change)
            for (name, value) in upstream_resp.headers() {
                if !matches!(
                    name.as_str(),
                    "transfer-encoding" | "content-encoding" | "content-length"
                ) {
                    resp_builder = resp_builder.header(name, value);
                }
            }

            if is_streaming {
                let stream = process_sse_stream(upstream_resp.bytes_stream());
                Ok(resp_builder
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap())
            } else {
                let text = upstream_resp.text().await.map_err(|e| {
                    error!("Response body read error: {}", e);
                    StatusCode::BAD_GATEWAY
                })?;

                let modified = logic::transform_non_streaming(&text);
                debug!(
                    "Modified response ({} bytes): {}",
                    modified.as_bytes().len(),
                    modified
                );

                Ok(resp_builder
                    .header("content-type", "application/json")
                    // .header("content-length", modified.as_bytes().len())
                    .body(Body::from(modified))
                    .unwrap())
            }
        }
        Err(e) => {
            error!("Upstream request failed: {}", e);
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

/// Helper to filter and map headers
/// Takes ownership of builder and returns it because reqwest builders consume self
fn filter_headers(
    headers: &HeaderMap,
    req_builder: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    let mut builder = req_builder;
    for (name, value) in headers {
        match name.as_str() {
            "host" | "content-length" | "connection" => continue,
            "authorization" | "content-type" | "user-agent" | "accept" => {
                builder = builder.header(name, value);
            }
            n if n.starts_with("x-") => {
                builder = builder.header(name, value);
            }
            _ => {}
        }
    }
    builder
}

/// Processes the SSE stream using LinesCodec for non-buffered streaming
fn process_sse_stream(
    upstream_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> PinBoxStream {
    let reader = StreamReader::new(upstream_stream.map_err(std::io::Error::other));
    let lines = FramedRead::new(reader, LinesCodec::new());

    Box::pin(async_stream::try_stream! {
        let mut state = StreamState::default();

        debug!("SSE Stream processing started.");
        tokio::pin!(lines);

        let mut line_count = 0;

        while let Some(line_result) = lines.next().await {
            line_count += 1;
            let line = line_result.map_err(|e| {
                error!("Error reading line {}: {}", line_count, e);
                axum::Error::new(e)
            })?;

            // Trim whitespace but preserve content for debug
            let line = line.trim();

            // Log periodically or for specific events
            if line_count % 100 == 0 || line.contains("[DONE]") {
                debug!("Stream line {}: {} chars", line_count, line.len());
            }

            if line.is_empty() {
                continue;
            }

            if line == "data: [DONE]" {
                debug!("Received [DONE] from upstream after {} lines.", line_count);
                yield "data: [DONE]\n\n".into();
                break;
            }

            if let Some(json_str) = line.strip_prefix("data: ") {
                match serde_json::from_str::<Value>(json_str) {
                    Ok(mut val) => {
                        logic::transform_stream_chunk(&mut val, &mut state);
                        yield format!("data: {}\n\n", val).into();
                    }
                    Err(e) => {
                        error!("Failed to parse JSON at line {}: '{}'. Error: {}", line_count, json_str, e);
                        // Forward a specific error so the client knows why it stopped, if we choose to stop
                        yield format!("data: {{\"error\": \"Proxy JSON Parse Error: {}\"}}\n\n", e).into();
                    }
                }
            } else {
                // SSE Comment or other control lines
                debug!("Non-data line: {}", line);
                // Forward non-data lines as is
                yield format!("{}\n", line).into();
            }
        }
        debug!("SSE Stream processing ended after {} lines.", line_count);
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
        if let Ok(json) = serde_json::from_slice::<Value>(body) {
            return json
                .get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        }
        false
    }

    pub fn transform_non_streaming(json_str: &str) -> String {
        if let Ok(mut json) = serde_json::from_str::<Value>(json_str) {
            // Navigate to choices -> [i] -> message
            if let Some(choices) = json.get_mut("choices").and_then(|v| v.as_array_mut()) {
                if let Some(choice) = choices.first_mut() {
                    if let Some(message) = choice.get_mut("message").and_then(|v| v.as_object_mut())
                    {
                        transform_message(message);
                    }
                }
            }
            return json.to_string();
        }
        json_str.to_string()
    }

    pub fn transform_message(msg: &mut serde_json::Map<String, Value>) {
        let reasoning = msg.remove("reasoning_content");
        let content = msg.get_mut("content");

        if let Some(r_val) = reasoning {
            let wrapped = format!("<think>{}</think>", r_val.as_str().unwrap_or(""));
            match content {
                Some(c) if c.is_string() => {
                    let current = c.as_str().unwrap_or("");
                    *c = serde_json::json!(format!("{}{}", wrapped, current));
                }
                _ => {
                    msg.insert("content".to_string(), serde_json::json!(wrapped));
                }
            }
        }
    }

    pub fn transform_stream_chunk(val: &mut Value, state: &mut StreamState) {
        if let Some(delta) = get_mut_delta(val) {
            let reasoning = delta.remove("reasoning_content");
            let content = delta
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let mut output = String::new();

            if let Some(r) = reasoning {
                let r_text = r.as_str().unwrap_or("");
                if !r_text.is_empty() {
                    if !state.is_thinking {
                        output.push_str("<think>");
                        state.is_thinking = true;
                    }
                    output.push_str(r_text);
                }
            }

            if let Some(c_text) = content {
                if !c_text.is_empty() {
                    if state.is_thinking {
                        output.push_str("</think>");
                        state.is_thinking = false;
                    }
                    output.push_str(&c_text);
                }
            }

            if !output.is_empty() {
                delta.insert("content".to_string(), serde_json::json!(output));
            }
        }
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
