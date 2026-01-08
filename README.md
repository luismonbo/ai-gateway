# Rust AI Gateway 🦀

An educational, high-performance AI Gateway written in Rust using `Actix-Web`. 

This project demonstrates how to build a production-ready API Gateway that routes requests between local open-source models (via **Ollama**) and cloud providers (**OpenAI**), complete with rate limiting, request tracking, and fault tolerance.

## ✨ Status / Roadmap

| # | Feature | Status |
|---|---|---|
| 1 | Dual Provider Support | ✅ Complete |
| 2 | Universal API | ✅ Complete |
| 3 | Streaming Support | ✅ Complete |
| 4 | Middleware (auth, rate limiting) | ✅ Complete |
| 5 | Resilience (non-streaming fallback) | ✅ Complete |
| 6 | Streaming Fallback Support | ⏳ Planned |
| 7 | Azure OpenAI support | ⏳ Planned |

## 🚀 Features

### Core Functionality
- **Dual Provider Support**: Seamlessly switches between **Ollama** (local) and **OpenAI** (cloud).
- **Universal API**: Exposes an OpenAI-compatible interface (`/v1/chat/completions`), allowing clients to swap providers without code changes.
- **Streaming Support**: Full Server-Sent Events (SSE) streaming support for real-time responses.

### Enterprise Patterns
- **🛡️ Resilience & Fallback**: Implements the **Decorator Pattern** to automatically failover to OpenAI if the local Ollama instance is down or responding with errors. Includes model rewriting logic (e.g., `llama3` -> `gpt-3.5-turbo` on fallback).
- **🚦 Rate Limiting**: Custom **Token Bucket** algorithm implementation (using `std::sync` primitives) to enforce per-key rate limits (Default: 60 RPM).
- **📊 Observability**:
    - **Structured Logging**: JSON logging via `tracing` for easy ingestion into log aggregators.
    - **Request Tracking**: In-memory tracking of request count, latency, and token usage (prompt/completion) per API key.
    - **Persisted Stats**: Automatically saves metrics to `stats.json` on shutdown.
- **🔐 Authentication**: Bearer token authentication with role-based access control (User vs. Admin keys).

## 🛠️ Tech Stack

- **Language**: Rust 2021
- **Framework**: `actix-web`
- **Concurrency**: `tokio`, `std::sync::{Arc, RwLock, Mutex}` for thread-safe state management.
- **Serialization**: `serde`, `serde_json`
- **Logging**: `tracing`, `tracing-subscriber`
- **HTTP Client**: `reqwest`

## 📦 Installation & Usage

### 1. Prerequisites
- Rust & Cargo installed.
- [Ollama](https://ollama.ai/) running locally (optional, but recommended for primary provider).

### 2. Configuration (`.env`)
Create a `.env` file in the root directory:

```ini
# Server Config
HOST=127.0.0.1
PORT=8080
LOG_FORMAT=json

# Authentication
GATEWAY_API_KEYS=sk-gw-test123,sk-gw-dev456
ADMIN_API_KEYS=admin-secret-key

# Primary Provider (Ollama)
OLLAMA_BASE_URL=http://localhost:11434

# Secondary Provider (OpenAI - Optional for Fallback)
OPENAI_API_KEY=sk-your-openai-key
OPENAI_BASE_URL=https://api.openai.com
```

### 3. Run the Server
```bash
cargo run
```

### 4. Testing

**Standard Request (Ollama):**
```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-gw-test123" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llama3",
    "messages": [{"role": "user", "content": "Hello world!"}]
  }'
```

**Trigger Fallback (Simulate Failure):**
Request a model that doesn't exist locally, or stop your Ollama service. The Gateway will automatically retry with OpenAI (rewriting the model to a default fallback like `gpt-4.1-nano` or `gpt-3.5-turbo`).

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-gw-test123" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "non-existent-model",
    "messages": [{"role": "user", "content": "This will trigger fallback!"}]
  }'
```

**Check Stats:**
```bash
curl -H "Authorization: Bearer admin-secret-key" http://localhost:8080/v1/stats
```

## 🧩 Architecture

The application uses a layered architecture:

1.  **Middleware Chain**:
    - `RateLimitMiddleware`: Checks token bucket.
    - `AuthMiddleware`: Validates Bearer token.
    - `TrackingMiddleware`: Meaure latency & errors.
2.  **Handlers**:
    - `/chat/completions`: Deserializes request, invokes provider.
3.  **Providers (Trait Object)**:
    - `FallbackProvider` (Decorator) -> wraps:
        - `OllamaProvider` (Primary)
        - `OpenAIProvider` (Backup)

---
*Built for learning purposes by Luis Monge.*
