# ai-gateway

[![CI](https://github.com/LuisMongeB/ai-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/LuisMongeB/ai-gateway/actions/workflows/ci.yml)

An OpenAI-compatible gateway that routes requests to Ollama, OpenAI and other OpenAI-style backends by model prefix, with API keys, rate limits, fallback and usage stats. Clients talk to one endpoint and pick a backend with a prefix such as `ollama/gemma4:e2b` or `openai/gpt-6-terra`. Request and response bodies pass through untouched apart from the model name, so tools, images, JSON mode, sampling parameters and streaming work wherever the backend supports them. It is written in Rust with actix-web and runs as a single binary or a small Docker image.

## Quick start

### With cargo

```bash
cp .env.example .env
openssl rand -hex 32          # put the output in .env as GATEWAY_API_KEYS=…
cargo run --release
```

The gateway reads `.env` from the current directory at startup; variables already set in the environment win. The examples below send the key as `$GATEWAY_KEY`, so set that first (`export GATEWAY_KEY=…`, the same key). Check that the gateway is up, then send a request:

```bash
curl http://localhost:8080/health

curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "ollama/gemma4:e2b", "messages": [{"role": "user", "content": "Hello!"}]}'
```

Any OpenAI SDK works: point it at the gateway and use a gateway key.

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8080/v1", api_key="<your gateway key>")
reply = client.chat.completions.create(
    model="ollama/gemma4:e2b",
    messages=[{"role": "user", "content": "Hello!"}],
)
```

### With Docker

```bash
docker build -t ai-gateway .
docker run --rm -p 8080:8080 -v ai-gateway-data:/data \
  -e GATEWAY_API_KEYS="$GATEWAY_KEY" \
  -e UPSTREAMS=ollama \
  -e OLLAMA_BASE_URL=http://host.docker.internal:11434/v1 \
  ai-gateway
```

- Inside a container, `localhost` is the container itself. `host.docker.internal` reaches an Ollama running on your machine; on Linux, add `--add-host=host.docker.internal:host-gateway`.
- The image sets `HOST=0.0.0.0`, `PORT=8080` and `STATS_PATH=/data/stats.json`. If you pass `--env-file .env`, keep `HOST` and `STATS_PATH` out of that file, or the container stops listening on its network interface or can't save stats.
- The image runs as the non-root user 65532 and keeps stats in the `/data` volume, which that user owns.
- The image has no shell or curl, so a `HEALTHCHECK` command can't run inside it. Probe `/health` over HTTP instead, for example in Kubernetes:

```yaml
livenessProbe:
  httpGet:
    path: /health
    port: 8080
readinessProbe:
  httpGet:
    path: /health
    port: 8080
```

## Configuration

Everything is set with environment variables, read once at startup. A blank value counts as unset.

| Variable | Default | Meaning |
|---|---|---|
| `HOST` | `127.0.0.1` | Bind address. The Docker image sets `0.0.0.0`. |
| `PORT` | `8080` | |
| `LOG_FORMAT` | `text` | `text` or `json`. |
| `RUST_LOG` | `info` | Standard tracing filter, e.g. `info,access=warn`. |
| `GATEWAY_API_KEYS` | none | Comma-separated user keys. |
| `ADMIN_API_KEYS` | none | Comma-separated admin keys. |
| `RATE_LIMIT_RPM` | `60` | Requests per minute per key; also the burst size. |
| `MAX_REQUEST_BYTES` | `10485760` (10 MiB) | Largest accepted request body. |
| `UPSTREAMS` | required | Comma-separated upstream names, e.g. `ollama,openai`. |
| `DEFAULT_UPSTREAM` | none | Upstream for model names without a known prefix. |
| `<NAME>_BASE_URL` | required per upstream | OpenAI-style base URL including the version path. |
| `<NAME>_API_KEY` | none | Sent to that upstream as `Authorization: Bearer …`. |
| `<NAME>_FALLBACK` | none | `upstream/model`, e.g. `OLLAMA_FALLBACK=openai/gpt-6-terra`. |
| `UPSTREAM_CONNECT_TIMEOUT_SECS` | `10` | How long to wait for a connection to an upstream. |
| `UPSTREAM_TIMEOUT_SECS` | `300` | Longest silence allowed while waiting for upstream data. It resets on every chunk, so long streams are fine. |
| `STATS_PATH` | `stats.json` | Where stats are saved. The Docker image uses `/data/stats.json`. |
| `STATS_FLUSH_SECS` | `60` | How often stats are saved. |

An upstream's variables use its name in upper case: `ollama` reads `OLLAMA_BASE_URL`, `OLLAMA_API_KEY` and `OLLAMA_FALLBACK`. Base URLs follow the OpenAI SDK convention and include the version path:

| Backend | Base URL |
|---|---|
| Ollama | `http://localhost:11434/v1` |
| OpenAI | `https://api.openai.com/v1` |
| Groq | `https://api.groq.com/openai/v1` |
| vLLM | `http://localhost:8000/v1` |

The gateway checks the configuration before it starts, and the first broken rule stops startup with a message naming the variable:

- Upstream names use only lowercase letters, digits and `_`, and each appears once.
- Every upstream needs a base URL that starts with `http://` or `https://`. Credentials, query strings and fragments are rejected; put the key in `<NAME>_API_KEY`. A URL without a path (such as `https://api.openai.com`) starts with a warning.
- `DEFAULT_UPSTREAM` and every fallback target must name a configured upstream.
- At least one gateway key is required, and a key can't be both a user key and an admin key. Keys shorter than 32 characters get a warning.
- Numbers must be whole and greater than 0, and `PORT` must fit in 16 bits.

The startup log lists the bind address, each upstream with its base URL and fallback, the default upstream, the limits and the number of keys. It never shows a key.

## Routing

1. The gateway reads `model` from the request body (400 if it's missing or not a string).
2. If the text before the first `/` is a configured upstream, the request goes there with the rest as the model (400 if the rest is empty).
3. Otherwise the request goes to `DEFAULT_UPSTREAM` with the full string. With no default, the answer is a 400 that lists the valid prefixes.

With `UPSTREAMS=ollama,openai`:

| `model` sent | `DEFAULT_UPSTREAM` | Goes to | Model the upstream sees |
|---|---|---|---|
| `ollama/gemma4:e2b` | any | ollama | `gemma4:e2b` |
| `openai/gpt-6-terra` | any | openai | `gpt-6-terra` |
| `ollama/hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL` | any | ollama | `hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL` |
| `hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL` | ollama | ollama | the full string (`hf.co` isn't an upstream) |
| `gemma4:e2b` | ollama | ollama | `gemma4:e2b` |
| `gemma4:e2b` | none | — | 400 |

Prefixes match exactly and are case-sensitive.

## Fallback

`OLLAMA_FALLBACK=openai/gpt-6-terra` sends a request to OpenAI as `gpt-6-terra` when Ollama can't answer it.

- **What triggers it:** connection errors, timeouts, upstream 5xx and upstream 429. Other 4xx responses never do: they're the client's or the operator's mistake, and retrying elsewhere would hide them and could cost money.
- **One hop:** a fallback's own fallback is never followed.
- **Streaming:** streaming requests fall back too, because the switch happens before the first byte reaches the client.
- **The request:** the same body, with `model` replaced by the fallback model.
- **How you can tell:** the response carries `x-gateway-fallback: openai/gpt-6-terra`, its `model` field names the model that answered, and the log has a warning with the key ID, the primary's error and the fallback target. If the fallback fails too, the client gets the fallback's error and the log has both.

## Endpoints

### `GET /health`

Public: no key, no rate limit. Returns `200 ok`.

```bash
curl http://localhost:8080/health
```

### `POST /v1/chat/completions`

The OpenAI chat completions API. Only `model` is rewritten; every other field reaches the upstream unchanged, and only the upstream's own key is sent to it, never the client's gateway key.

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "openai/gpt-6-terra", "temperature": 0.2,
       "messages": [{"role": "user", "content": "Summarise SSE in one line."}]}'
```

A successful non-streaming answer is the upstream's body, byte for byte, with its content type.

Streaming works the same way with `"stream": true`. Events are passed on byte for byte as they complete, including `data: [DONE]`.

```bash
curl -N http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "ollama/gemma4:e2b", "stream": true,
       "messages": [{"role": "user", "content": "Count to five."}]}'
```

To count tokens in streams, the gateway asks the upstream for usage (`stream_options.include_usage: true`) when the client didn't, and then drops the usage-only chunk that results before the client sees it. Clients that asked for usage get the chunk as normal.

Chat responses that reached an upstream, successful or not, carry these headers:

| Header | Value |
|---|---|
| `x-gateway-upstream` | The upstream that answered, e.g. `ollama`. |
| `x-gateway-fallback` | `upstream/model` of the fallback, only when the fallback answered. |

### `GET /v1/models`

Every upstream's models in one OpenAI-style list, with IDs already prefixed, so an ID can be sent back as `model`:

```bash
curl http://localhost:8080/v1/models -H "Authorization: Bearer $GATEWAY_KEY"
```

```json
{"object": "list", "data": [
  {"id": "ollama/gemma4:e2b", "object": "model", "created": 1730000000, "owned_by": "ollama"},
  {"id": "openai/gpt-6-terra", "object": "model", "created": 1760000000, "owned_by": "openai"}
]}
```

Upstreams are asked at the same time, and each gets `UPSTREAM_CONNECT_TIMEOUT_SECS` (10 s by default) for its whole answer, so one hanging upstream can't hold up the list. An upstream that fails is left out and logged; the response is still 200.

### `GET /v1/stats`

Usage per key. A user key gets its own stats (zeros before its first request); `?key_id=` is ignored.

```bash
curl http://localhost:8080/v1/stats -H "Authorization: Bearer $GATEWAY_KEY"
```

```json
{"key_id": "key_6a9739db33e7", "request_count": 12, "error_count": 1,
 "total_latency_ms": 5400, "avg_latency_ms": 450.0,
 "total_prompt_tokens": 2048, "total_completion_tokens": 512,
 "last_request_timestamp": 1795485831406,
 "models_used": {"ollama/gemma4:e2b": 10, "openai/gpt-6-terra": 1}}
```

An admin key gets every key's stats as an array, or one key's with `?key_id=`. A malformed key ID gets a 400, and an unknown one a 404.

```bash
curl http://localhost:8080/v1/stats -H "Authorization: Bearer $ADMIN_KEY"
curl "http://localhost:8080/v1/stats?key_id=key_6a9739db33e7" -H "Authorization: Bearer $ADMIN_KEY"
```

## Keys, roles and rate limits

- **Sending a key:** every `/v1` route needs `Authorization: Bearer <key>` with a key from `GATEWAY_API_KEYS` or `ADMIN_API_KEYS`. The scheme is case-insensitive.
- **Roles:** both roles can call every endpoint; admins can also read everyone's stats.
- **Key IDs:** keys are hashed at startup and never logged or stored. Logs and stats name a key by its ID: `key_` plus the first 12 hex characters of the key's SHA-256. To find the ID of a key:

  ```bash
  echo "key_$(printf %s "$KEY" | shasum -a 256 | cut -c1-12)"    # Linux: sha256sum instead of shasum -a 256
  ```

- **Rate limits:** each key gets a token bucket that holds `RATE_LIMIT_RPM` requests and refills at `RATE_LIMIT_RPM / 60` per second. It applies to every `/v1` route, admin keys included. A request over the limit gets a 429 with `Retry-After`: the whole seconds until a request will be accepted again, rounded up. The OpenAI SDKs wait that long before retrying.
- **What isn't counted:** requests rejected for a missing key or the rate limit aren't counted in stats; they still appear in the access log.

## Errors

Every error the gateway produces uses OpenAI's format, so the SDKs report it like any API error:

```json
{"error": {"message": "Upstream 'ollama' could not be reached.", "type": "server_error", "param": null, "code": "upstream_unavailable"}}
```

| Situation | Status | `type` | `code` |
|---|---|---|---|
| Missing or invalid gateway key | 401 | `invalid_request_error` | `invalid_api_key` |
| Gateway rate limit | 429, with `Retry-After` | `rate_limit_error` | `rate_limit_exceeded` |
| Bad JSON, missing `model`, unknown prefix with no default | 400 | `invalid_request_error` | `null` |
| Body over `MAX_REQUEST_BYTES` | 413 | `invalid_request_error` | `payload_too_large` |
| No such route | 404 | `invalid_request_error` | `not_found` |
| Can't connect to the upstream | 502 | `server_error` | `upstream_unavailable` |
| The upstream timed out | 504 | `server_error` | `upstream_timeout` |
| Upstream 5xx | 502 | `server_error` | `upstream_error` |
| Upstream 429 | 429, with the upstream's `Retry-After` if it sent one | `rate_limit_error` | `upstream_rate_limited` |
| Upstream 401 or 403 | 502, generic message | `server_error` | `upstream_auth_failed` |
| Any other upstream 4xx (400, 404, 413, 422…) | the same status | the upstream's own body, passed through | |

Bodies of upstream 401, 403, 429 and 5xx responses are never returned, because they can contain fragments of the operator's upstream key or organisation ID. They're logged, truncated to 512 bytes, with the upstream's key replaced by `[redacted]`. Other 4xx bodies reach the client because they explain the client's mistake, such as an unknown model or a context that's too long.

If an upstream fails in the middle of a stream, the status has already been sent, so the gateway logs the failure and aborts the connection instead of ending the stream as if the answer were complete.

## Stats

For each key ID the gateway records:

- requests and errors (responses with a 5xx status);
- latency to the response headers;
- prompt and completion tokens;
- which models answered, as `upstream/model`, so a fallback shows up as `openai/gpt-6-terra`.

Stats are saved to `STATS_PATH` every `STATS_FLUSH_SECS` and on shutdown, so a crash loses at most one interval. Each save writes `<path>.tmp`, syncs it to disk and renames it over the old file, so the file is never half-written. If the final save fails, the gateway exits with status 1.

- **Missing file:** the gateway starts empty.
- **Corrupt or unreadable file:** it's logged as an error and renamed to `<path>.corrupt-<unix time>`, and the gateway starts empty. Nothing is overwritten. If the file can't be renamed either, startup fails.
- **Files from 0.1:** entries stored under raw API keys are re-keyed to their key IDs and merged when the file loads, and the raw keys leave the file at the next save.

## Logging

- **Library:** logs go through `tracing`, as text or as JSON lines (`LOG_FORMAT=json`), and are filtered with `RUST_LOG`.
- **Colours:** text logs use colour only when they go to a terminal and `NO_COLOR` isn't set.
- **Access log:** every request gets one line with target `access`, holding the method, the path, the status, the latency, the client's IP address and the user agent. The query string is never logged. Quiet the access log with `RUST_LOG=info,access=warn`.
- **Key IDs:** gateway log lines name keys by ID (`key_id` field). No raw key, gateway or upstream, appears in logs, stats or error responses.

## Upgrading from 0.1

The configuration changed and isn't backward compatible. In your `.env`:

```ini
UPSTREAMS=ollama,openai
OLLAMA_BASE_URL=http://localhost:11434/v1   # now includes /v1
OPENAI_BASE_URL=https://api.openai.com/v1   # now includes /v1
OLLAMA_FALLBACK=openai/gpt-6-terra
DEFAULT_UPSTREAM=ollama                     # optional: keeps unprefixed model names working
```

Also:

- **Delete `AI_PROVIDER`.** It's ignored now, and its presence logs a warning.
- **Keep `OPENAI_API_KEY`.** It's now the `openai` upstream's key.
- **Set `GATEWAY_API_KEYS`.** It's required; there's no built-in key any more (`openssl rand -hex 32`).

What changes for clients:

- **Model names:** they take an upstream prefix (`ollama/gemma4:e2b`). With `DEFAULT_UPSTREAM=ollama`, unprefixed names keep going to Ollama.
- **Request bodies:** they now pass through to the upstream unchanged. Ollama-only options that the old translation layer handled, such as a per-request `num_ctx`, are no longer available.
- **Health check:** `/v1/health` is now `/health`, and it needs no key.
- **Stats:** `/v1/stats` returns `key_id` instead of `api_key`, and admins filter with `?key_id=` instead of `?key=`.
- **Errors:** they use OpenAI's format, and rate-limited requests get a `Retry-After` header.
- **Old stats file:** an existing `stats.json` is converted automatically (see [Stats](#stats)).

## Limitations

- Stats and rate limits are per process: two instances count separately.
- Changing keys or any other setting needs a restart.
- Tokens aren't counted for streams from an upstream that ignores `stream_options`.
- Tokens aren't counted for a stream the client abandons before it ends.
- A non-streaming request whose generation takes longer than `UPSTREAM_TIMEOUT_SECS` times out, because the upstream sends nothing until it's done. Stream long generations, or raise the timeout.
- Request bodies aren't decompressed: a body sent with `Content-Encoding: gzip` gets a 400. Leaving decompression out means a small compressed body can't expand into a huge one.
- There are no retries, response caching, embeddings endpoint or Azure OpenAI support yet.

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo install cargo-audit --locked && cargo audit
```

CI runs the same checks on every push and pull request to `main`. The tests start mock upstreams in-process, so they need no network, Ollama or API keys.

| Path | What it holds |
|---|---|
| `src/main.rs` | Startup: `.env`, configuration, logging, stats, server, final save. |
| `src/app.rs` | Shared state, routes and middleware order. |
| `src/config/` | Environment variables and their validation. |
| `src/auth.rs` | Key hashing, key IDs, roles and the auth middleware. |
| `src/rate_limit.rs` | Per-key token buckets. |
| `src/proxy/` | Routing, the upstream client, SSE relay, fallback and `/v1/models`. |
| `src/stats/` | The stats store, its file and `/v1/stats`. |
| `src/access_log.rs` | The access log. |
| `src/error.rs` | OpenAI-style errors. |
| `tests/` | Integration tests against mock upstreams. |

## Roadmap

| Feature | Status |
|---|---|
| OpenAI-compatible routing by model prefix | Done |
| Streaming pass-through | Done |
| Gateway keys with user and admin roles | Done |
| Per-key rate limits with `Retry-After` | Done |
| Fallback, including streaming | Done |
| Usage stats per key, saved atomically | Done |
| Docker image | Done |
| Retries with backoff | Planned |
| Embeddings endpoint | Planned |
| Azure OpenAI | Planned |
| Response caching | Planned |

---

Built by Luis Monge.
