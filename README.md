# LLM Reasoning Proxy

A lightweight, high-performance Rust proxy that improve compatibility between reasoning models (like DeepSeek R1) and standard OpenAI-compatible clients.

## Motivation

Most LLM serving frameworks like vLLM, SGLang or providers like OpenRouter are serving reasoning content in a specific `reasoning_content` field that was not part of OpenAI Chat Completions standard.

This proxy intercepts chat completion requests from such providers and wraps reasoning content into `<think>` tags. This ensures compatibility with UI clients that do not natively support reasoning fields.

## Features

- 🚀 **High Performance:** Built with Rust, Axum, and Tokio.
- ⚡ **Streaming Support:** Full Server-Sent Events (SSE) streaming support with real-time transformation.
- 🐳 **Container Ready:** Docker, Podman and DockerCompose support with nightly image build.

## Installation

### 🦀 Option 1: Cargo (Rust Users)

If you have the Rust toolchain installed, you can install directly from source:

```bash
# From the local directory
git clone https://github.com/mratsim/llm-reasoning-proxy
cargo install --path llm-reasoning-proxy

# Or via git
cargo install --git https://github.com/mratsim/llm-reasoning-proxy
```

Then point the proxy to your inference engine

```
llm-reasoning-proxy --port 5001 --upstream http://127.0.0.1:5000
```



### 🐳 Option 2: Docker or Podman

Adjust upstream address and local port accordingly

```bash
docker run -p 5001:5001 \
  --env RUST_LOG=info \
  --add-host=host.docker.internal:host-gateway \
  ghcr.io/mratsim/llm-reasoning-proxy:nightly \
  --port 5001 \
  --upstream http://host.docker.internal:5000
```

```bash
podman run -p 5001:5001 \
  --env RUST_LOG=info \
  ghcr.io/mratsim/llm-reasoning-proxy:nightly \
  --port 5001 \
  --upstream http://host.containers.internal:5000
```

### 🐳 Option 3: Docker-compose

```
git clone https://github.com/mratsim/llm-reasoning-proxy
cd llm-reasoning-proxy
docker-compose up -d
```

## Usage

Start the proxy pointing to your upstream inference engine.

```bash
llm-reasoning-proxy --port 5001 --upstream http://127.0.0.1:11434
```

### CLI Arguments

| Argument | Short | Default | Description |
|----------|-------|---------|-------------|
| `--port` | `-p` | `5001` | Port for the proxy to listen on. |
| `--upstream` | `-u` | `http://127.0.0.1:5000` | The URL of the actual LLM server. |

### Example Request

Once running, point your OpenAI-compatible client (like common UI frontends) to `http://localhost:5001/v1`.

**Curl Example:**

```bash
curl http://localhost:5001/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "GLM-4.6V",
    "stream": true,
    "messages": [
      { "role": "user", "content": "Why is the sky blue?" }
    ]
  }'
```

The response will contain `<think>` tags instead of a separate `reasoning_content` field.

## License

Licensed and distributed under either of

* MIT license: [LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT

or

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option. This file may not be copied, modified, or distributed except according to those terms.