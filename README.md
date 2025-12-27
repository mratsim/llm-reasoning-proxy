# LLM Reasoning Proxy

A lightweight, high-performance Rust proxy that improve compatibility between reasoning models (like DeepSeek R1) and standard OpenAI-compatible clients.

## Motivation

Most LLM serving frameworks like vLLM, SGLang or providers like OpenRouter are serving reasoning content in a specific `reasoning_content` field that was not part of OpenAI Chat Completions standard.

This proxy intercepts chat completion requests from such providers and wraps reasoning content into `<think>` tags. This ensures compatibility with UI clients that do not natively support reasoning fields.

## Features

- 🚀 **High Performance:** Built with Rust, Axum, and Tokio.
- ⚡ **Streaming Support:** Full Server-Sent Events (SSE) streaming support with real-time transformation.
- 🐳 **Container Ready:** Docker, Podman and DockerCompose support with prebuilt images.

## Installation

### 📦 Option 1: Prebuilt Binaries

1. **Quick Install** - Platform-specific one-liners:

<details>
   <summary>Windows (x86-64 i.e. AMD or Intel)</summary>

   ```powershell
   Invoke-WebRequest -Uri "https://github.com/mratsim/llm-reasoning-proxy/releases/latest/download/llm-reasoning-proxy-windows-x86_64.zip" -OutFile "proxy.zip"; Expand-Archive -Path "proxy.zip" -DestinationPath "."; Remove-Item "proxy.zip"
   ```
   </details>

   <details>
   <summary>Windows ARM64</summary>

   ```powershell
   Invoke-WebRequest -Uri "https://github.com/mratsim/llm-reasoning-proxy/releases/latest/download/llm-reasoning-proxy-windows-arm64.zip" -OutFile "proxy.zip"; Expand-Archive -Path "proxy.zip" -DestinationPath "."; Remove-Item "proxy.zip"
   ```
   </details>

   <details>
   <summary>Linux (x86-64 i.e. AMD or Intel)</summary>

   ```bash
   curl -sL "https://github.com/mratsim/llm-reasoning-proxy/releases/latest/download/llm-reasoning-proxy-linux-x86_64.tar.gz" | tar -xz
   ```
   </details>

   <details>
   <summary>Linux (Arm64 like Raspberry Pi)</summary>

   ```bash
   curl -sL "https://github.com/mratsim/llm-reasoning-proxy/releases/latest/download/llm-reasoning-proxy-linux-arm64.tar.gz" | tar -xz
   ```
   </details>

   <details>
   <summary>macOS Apple Silicon</summary>

   ```bash
   curl -sL "https://github.com/mratsim/llm-reasoning-proxy/releases/latest/download/llm-reasoning-proxy-macos-arm64.tar.gz" | tar -xz
   ```
   </details>

2. **Manual Download** - Visit the [GitHub Releases page](https://github.com/mratsim/llm-reasoning-proxy/releases) and download the appropriate file for your platform and architecture then **extract** the binary:
   - **Windows:** Use 7-Zip or built-in extraction
   - **macOS/Linux:** `tar -xzf llm-reasoning-proxy-platform-arch-v<version>.tar.gz`

3. **Run** the proxy:
   ```bash
   llm-reasoning-proxy --port 5001 --upstream http://127.0.0.1:5000
   ```

### 🦀 Option 2: Cargo (Rust Users)

If you have the Rust toolchain installed, you can install directly from source:

```bash
# From the local directory
git clone https://github.com/mratsim/llm-reasoning-proxy
cd llm-reasoning-proxy
cargo build --release
./target/release/llm-reasoning-proxy

# Or install globally
cargo install --git https://github.com/mratsim/llm-reasoning-proxy
```

Then point the proxy to your inference engine

```
llm-reasoning-proxy --port 5001 --upstream http://127.0.0.1:5000
```

### 🐳 Option 3: Docker or Podman

Adjust upstream address and local port accordingly

```bash
docker run -p 5001:5001 \
  --env RUST_LOG=info \
  --add-host=host.docker.internal:host-gateway \
  ghcr.io/mratsim/llm-reasoning-proxy:latest \
  --port 5001 \
  --upstream http://host.docker.internal:5000
```

```bash
podman run -p 5001:5001 \
  --env RUST_LOG=info \
  ghcr.io/mratsim/llm-reasoning-proxy:latest \
  --port 5001 \
  --upstream http://host.containers.internal:5000
```

### 🐳 Option 4: Docker-compose

```
git clone https://github.com/mratsim/llm-reasoning-proxy
cd llm-reasoning-proxy
docker-compose up -d
```

## Usage

Start the proxy pointing to your upstream inference engine.

```bash
llm-reasoning-proxy --port 5001 --upstream http://127.0.0.1:5000
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