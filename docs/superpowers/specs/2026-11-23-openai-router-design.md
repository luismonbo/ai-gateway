# ai-gateway: OpenAI-compatible router and hardening

Status: draft for owner approval · 2026-11-23

## Why

ai-gateway is public and pinned on the owner's GitHub profile. The goal is "a proper AI gateway someone might want to use": a small self-hosted service that puts API keys, per-key rate limits and usage stats in front of any backend that speaks the OpenAI API (Ollama, OpenAI, vLLM, LM Studio, Groq…), with a fallback when a backend fails. Every claim in the README must be true, and the repo needs tests, CI and a Docker image before it is re-pinned.

The owner is learning Rust. Claude implements this round and explains each step: what changed, how, the implications, and the Rust concepts behind it. Clear code beats clever code.

## Decisions

- Owner: approach B below (router for OpenAI-compatible backends).
- Owner: the example fallback model is `gpt-6-terra` (replacing `gpt-4.1-nano`). It lives in configuration, never in Rust code.
- Everything else marked in this document is Claude's call, open to correction.

## Goals

1. Clients choose a backend with a model prefix (`ollama/gemma4:e2b`, `openai/gpt-6-terra`). Requests and responses pass through almost untouched, so tools, images, JSON mode and sampling parameters work on every backend.
2. A per-backend fallback that the client can see.
3. No secret ever reaches logs, the stats file, URLs or error bodies.
4. All configuration comes from environment variables and is checked at startup; bad config stops the process with a clear message.
5. OpenAI-compatible surface: `POST /v1/chat/completions` (streaming and not), `GET /v1/models`, OpenAI's error format, `Retry-After` on 429.
6. Unit and integration tests; CI running fmt, clippy (`-D warnings`), tests and a dependency audit; `Cargo.lock` committed.
7. A Dockerfile, and a README documenting every endpoint, every setting and the auth flow.

## Non-goals (this round)

- Sharing rate limits or stats between several gateway processes. Both stay in memory per process; the README says so.
- Managing keys at runtime. Keys come from env vars; changing them needs a restart.
- Endpoints other than chat completions and models (`/v1/embeddings` is the obvious next one).
- Azure OpenAI (different URL and auth scheme). It stays on the roadmap.
- More than one fallback hop, retries, response caching, Prometheus metrics, CORS, benchmarks.
- Ollama's native API. Ollama is reached through its OpenAI-compatible `/v1` endpoint; the NDJSON→SSE translation is deleted (it stays in git history).

## Anti-goals (failure even if everything "works")

- A raw gateway key appearing anywhere outside the environment: logs, `stats.json`, `/v1/stats`, URLs.
- Silent model substitution: a client must be able to tell that a fallback answered.
- Leaking upstream account details (fragments of the operator's upstream key, organisation IDs) to gateway clients.
- Breaking stock OpenAI SDKs: the official Python/JS clients must work with only `base_url`, `api_key` and the model name changed.

## Constraints and facts

- Rust stable (local toolchain 1.100.0), edition 2021, actix-web 4.12, reqwest 0.12.28 with rustls. Keep the dependency list short.
- Kept from today: Bearer auth with user and admin roles, a token bucket per key, per-key stats persisted to a JSON file, text or JSON logs.
- Checked against the owner's Ollama 0.40.17 on 2026-11-23:
  - `/v1/models` and `/v1/chat/completions` work.
  - With `stream_options.include_usage` it sends a usage chunk with `choices: []` before `data: [DONE]`.
  - The last content chunk has `delta: {}`.
  - Model IDs can contain `/` (`hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL`).
- actix-web's raw-body extractor defaults to a 256 KiB limit. That's too small for long chats or images, so the gateway sets its own limit.
- The repo is public.
  - Commits happen on a branch after this design is approved.
  - Pushing, merging and GitHub settings (description, topics) happen only when the owner says so.
  - Unpinning and re-pinning are the owner's actions on their profile.

## Approaches considered

| | Approach | Verdict |
|---|---|---|
| A | Local-first, hardened: keep the Ollama-native translation; make the primary, the fallback and the fallback model configurable; map OpenAI parameters to Ollama by hand. | Rejected: every OpenAI feature (tools, images, JSON mode) needs mapping code, and it only ever supports two backends. |
| **B** | **Router for OpenAI-compatible backends: each upstream is a base URL plus an optional key, clients pick one with a prefix, and bodies pass through.** | **Chosen by the owner.** Costs: deletes the translation layer and loses Ollama-only options such as per-request `num_ctx`. |

Within B, env vars were chosen over a TOML config file (Claude's call). Env vars keep the Docker setup simple and cover any number of upstreams with one fallback each. A file becomes worth it if model aliases or fallback chains are wanted later.

## Design

### Configuration

The gateway loads `.env` with `dotenvy`, which replaces the unmaintained `dotenv`, then reads the configuration once at startup. `.env` is loaded before logging is set up, so `LOG_FORMAT` and `RUST_LOG` set there take effect.

| Variable | Default | Meaning |
|---|---|---|
| `HOST` | `127.0.0.1` | Bind address. The Docker image sets `0.0.0.0`. |
| `PORT` | `8080` | |
| `LOG_FORMAT` | `text` | `text` or `json`. |
| `RUST_LOG` | `info` | Standard tracing filter. |
| `GATEWAY_API_KEYS` | none | Comma-separated user keys. |
| `ADMIN_API_KEYS` | none | Comma-separated admin keys. |
| `RATE_LIMIT_RPM` | `60` | Requests per minute per key; also the burst size. |
| `MAX_REQUEST_BYTES` | `10485760` (10 MiB) | Largest accepted request body. |
| `UPSTREAMS` | required | Comma-separated upstream names, e.g. `ollama,openai`. |
| `DEFAULT_UPSTREAM` | none | Upstream for model names without a known prefix. |
| `<NAME>_BASE_URL` | required per upstream | OpenAI-style base URL including the version path. |
| `<NAME>_API_KEY` | none | Sent upstream as `Authorization: Bearer …`. |
| `<NAME>_FALLBACK` | none | `upstream/model`, e.g. `OLLAMA_FALLBACK=openai/gpt-6-terra`. |
| `UPSTREAM_CONNECT_TIMEOUT_SECS` | `10` | |
| `UPSTREAM_TIMEOUT_SECS` | `300` | Longest silence allowed while waiting for upstream data. It resets on every chunk, so long streams are fine. |
| `STATS_PATH` | `stats.json` | The Docker image uses `/data/stats.json`. |
| `STATS_FLUSH_SECS` | `60` | How often stats are saved. |

Breaking any of these rules stops startup with a message naming the variable:

- **Upstream names** match `[a-z0-9_]+`, and their variables use the upper-cased name (`ollama` → `OLLAMA_BASE_URL`). Duplicates are rejected.
- **Base URLs:**
  - Every name in `UPSTREAMS` needs a `<NAME>_BASE_URL` that parses as an http(s) URL.
  - Base URLs follow the OpenAI SDK convention and include the version path: `https://api.openai.com/v1`, `http://localhost:11434/v1`, `https://api.groq.com/openai/v1`.
  - The gateway trims a trailing `/` and appends `/chat/completions` or `/models`.
  - A URL with no path gets a startup warning (Claude's call), because old `.env` files used `https://api.openai.com`.
- **References:** `DEFAULT_UPSTREAM` and every fallback target must name a configured upstream.
- **Gateway keys:**
  - At least one gateway key must be configured; there is no built-in default key.
  - A key listed as both user and admin is an error.
  - Keys shorter than 32 characters get a warning.
- **Legacy:** if `AI_PROVIDER` (no longer used) is set, it gets a warning.
- **Numbers** must parse and be greater than 0, and `PORT` must fit in a u16.

### Routing

1. Take `model` from the request body (400 if missing or not a string).
2. If the text before the first `/` is a configured upstream name, route there with the rest as the model (400 if the rest is empty).
3. Otherwise, route to `DEFAULT_UPSTREAM` with the full string. If there's no default, return 400 listing the valid prefixes.

| `model` sent | `DEFAULT_UPSTREAM` | Goes to | Model the upstream sees |
|---|---|---|---|
| `ollama/gemma4:e2b` | any | ollama | `gemma4:e2b` |
| `openai/gpt-6-terra` | any | openai | `gpt-6-terra` |
| `ollama/hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL` | any | ollama | `hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL` |
| `hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL` | ollama | ollama | the full string (`hf.co` isn't an upstream) |
| `gemma4:e2b` | ollama | ollama | `gemma4:e2b` |
| `gemma4:e2b` | none | — | 400 |

Prefix matching is exact and case-sensitive. Routing is a pure function and is unit-tested with this table.

### Forwarding

- **Parsing:** the handler reads the raw body (up to `MAX_REQUEST_BYTES`, otherwise 413). It parses the body into a struct holding `model`, `stream` (default false), and every other field via `#[serde(flatten)]`. Only `model` is rewritten; everything else goes upstream unchanged.
- **Upstream request:** `POST {base_url}/chat/completions`, with the upstream's key if it has one. No client header is forwarded; in particular, the client's gateway key never leaves the gateway.
- **HTTP client:** one `reqwest::Client`, built at startup with the timeouts above and shared by all requests, so connections are reused.
- **Non-streaming success:** return the upstream's body bytes unchanged (no re-serialisation) with its content type. Read `model` and `usage` from the same bytes for stats; if they're missing, record the request without tokens.
- **Streaming success:** respond with `text/event-stream` and pass the bytes through an SSE reader that:
  - buffers bytes until a complete event (events end with a blank line), so an event or a multi-byte character split across network chunks is never broken;
  - records tokens from any event whose JSON has `usage`;
  - forwards every event byte-for-byte, including `data: [DONE]`.
- **Usage in streams (Claude's call):**
  - If the client didn't ask for usage (`stream_options.include_usage` is anything other than `true`), the gateway sets it to `true` so tokens can be counted. It then drops the resulting usage-only chunk (`choices: []` plus `usage`) before it reaches the client.
  - Clients that asked for usage receive the chunk as normal.
  - If an upstream ignores the option, that request's tokens aren't counted. The README documents this.
- **Response headers:**
  - `x-gateway-upstream: <name>` is always set.
  - `x-gateway-fallback: <upstream>/<model>` is set when the fallback answered.
  - The body's `model` field, written by the upstream, also shows which model answered.

### Errors

Every error the gateway itself produces uses OpenAI's format:

```json
{"error": {"message": "…", "type": "…", "param": null, "code": "…"}}
```

| Situation | Client gets | Falls back? |
|---|---|---|
| Missing or invalid gateway key | 401 `invalid_api_key` | — |
| Gateway rate limit | 429 `rate_limit_exceeded` + `Retry-After` | — |
| Bad JSON, missing `model`, unknown prefix with no default | 400 `invalid_request_error` | — |
| Body over `MAX_REQUEST_BYTES` | 413 | — |
| Can't connect to the upstream / timed out | 502 `upstream_unavailable` / 504 `upstream_timeout` | yes |
| Upstream 5xx | 502 `upstream_error` | yes |
| Upstream 429 | 429 `upstream_rate_limited` (the upstream's `Retry-After` is kept if present) | yes |
| Upstream 401/403 | 502 `upstream_auth_failed`, with a generic message | no |
| Any other upstream 4xx (400, 404, 413, 422…) | the same status, with the upstream's body passed through | no |

- **Hidden upstream bodies (Claude's call):** bodies from upstream 401, 403, 429 and 5xx responses are logged (truncated) but never returned, because they can contain fragments of the operator's upstream key or organisation ID.
- **Passed-through upstream bodies:** other 4xx bodies reach the client because they explain the client's mistake, such as an unknown model or a context that's too long.
- **Both fail:** if the fallback also fails, the client gets the fallback's error, and the log has both.
- **Mid-stream failure:** a failure in the middle of a stream is logged and ends the stream, since the status was already sent.

### Fallback

- **Configuration:** per upstream, with `<NAME>_FALLBACK=upstream/model`.
- **Triggers (Claude's call):** only connection errors, timeouts, upstream 5xx and upstream 429, and only while nothing has been sent to the client yet. Other 4xx responses never trigger it: they're the client's or the operator's mistake, and falling back would hide them and could cost money.
- **One hop:** a fallback's own fallback is never followed.
- **Streaming:** streaming requests fall back too, as long as the failure happens before the first byte reaches the client.
- **The fallback request** is the same body, with `model` replaced by the fallback model.
- **Visibility:** the `x-gateway-fallback` header, the response's `model` field, and a warning log with the key ID, the primary's error and the fallback target.
- **Tests:** "does this error trigger a fallback?" is a pure function, unit-tested as a table.

### `GET /v1/models`

- **Fetching:** the gateway fetches `{base_url}/models` from every upstream concurrently.
- **Result:** one OpenAI-style list with IDs prefixed by the upstream name (`ollama/gemma4:e2b`) and `owned_by` set to the upstream name.
- **Failures:** an upstream that fails is skipped and logged, and the response is still 200.
- **Access:** it needs a gateway key and counts against the rate limit like every `/v1` route.

### Auth and keys (Claude's call)

- **Key store:** at startup, each gateway key is hashed (SHA-256) into a key store that maps the hash to a key ID and a role. Raw gateway keys aren't kept after startup.
- **Key ID:** `key_` plus the first 12 hex characters of the hash. It's stable across restarts and is what logs, stats and `/v1/stats` show.
- **Lookup:** to authenticate, the gateway hashes the presented Bearer token and looks the hash up. Response time then doesn't depend on how many characters of a guess are right, as it does with today's `Vec::contains` string comparison.
- **Roles:** both roles can call every endpoint. Admin keys see everyone's stats; user keys see only their own.
- **Request extensions** carry `{ key_id, role }`, never the raw key.
- **Failed attempts** are logged without the header or the token.

### Middleware layout

```
App
├── Logger                    outermost: access-logs everything, including 401 and 429
├── GET /health               public, no key: returns 200 "ok"
└── scope /v1                 runs Auth → RateLimit → Tracking → handler
    ├── POST /chat/completions
    ├── GET  /models
    └── GET  /stats
```

`/v1/health` is replaced by `/health` (Claude's call).

### Rate limiting

- **Bucket:** a token bucket per key ID, as today. Capacity is `RATE_LIMIT_RPM`, and it refills at RPM/60 tokens per second.
- **Scope:** it applies to every `/v1` route, admin keys included.
- **Rejection:** a rejected request gets 429 with `Retry-After` set to the whole seconds until one token is available (rounded up, at least 1).
- **Testing:** the bucket takes `now: Instant` as an argument, so tests move time forward without sleeping.

### Stats

- **What's recorded:** stats are keyed by key ID and record:
  - requests and errors (5xx)
  - latency to the response headers
  - prompt and completion tokens
  - which models answered, as `upstream/model`, so a fallback shows as `openai/gpt-6-terra`

  Requests rejected by auth or the rate limiter aren't counted; they appear in the access log.
- **Saving:** stats are saved every `STATS_FLUSH_SECS` and on shutdown. Saves are atomic: write `<path>.tmp`, then rename it over the file.
- **Missing or corrupt files:** a missing file means starting empty. A corrupt or unreadable file is logged as an error and renamed to `<path>.corrupt-<unix-time>`, then the gateway starts empty, so nothing is overwritten silently.
- **Old files:** files keyed by raw keys are converted on load. Entries that aren't key IDs are re-keyed by hashing them, so the raw keys leave the file at the next save.
- **`/v1/stats`:** users get their own stats. Admins get everyone's, or one key's with `?key_id=`. The masked `api_key` field becomes `key_id`.

### Logging

- **Library:** logs go through `tracing` only (the `log` crate is dropped), with `key_id` as a structured field.
- **Startup log:** the bind address, upstream names with their base URLs, the default upstream, the fallbacks and the rate limit. It never logs keys.

### What gets removed

- **Code:**
  - `providers/ollama.rs` and `providers/fallback.rs`
  - the `LLMProvider` trait
  - the typed OpenAI/Ollama request and response models, replaced by pass-through
- **Dependencies:** whichever of `log`, `dotenv`, `uuid`, `async-trait` and `futures-util` end up unused.
- **Added:** `dotenvy` and `sha2`; for tests, `wiremock` and `tempfile`.

## Testing

Tests are written first for each piece.

**Unit**
- The routing table above.
- The fallback-trigger table.
- Config validation: every rule above.
- Key store: lookup, roles, a stable ID format, and `Debug` output that never shows a key.
- Bucket: draining, refilling with injected time, the capacity cap, and the `Retry-After` value.
- SSE reader:
  - an event split across chunks
  - a multi-byte character split across chunks
  - usage extraction
  - the injected usage chunk dropped versus kept
  - `[DONE]` passed through
- Stats store: save/load round trip, atomic write, a corrupt file renamed, raw keys re-keyed.
- The error format.

**Integration** (`tests/`, the whole app in-process, upstreams mocked with `wiremock`)
- **Non-streaming:**
  - extra fields such as `temperature` and `tools` reach the upstream
  - the prefix is stripped from `model`
  - the response body is byte-identical
  - usage is recorded
- **Streaming:** events pass through unchanged; the usage chunk is injected and stripped.
- **Fallback:**
  - happens on 500, 429, a refused connection and a timeout
  - doesn't happen on 400 or 404
  - works for streaming before the first byte
  - stops after one hop
- **Upstream 401:** the client gets 502 without the upstream body.
- **Auth:**
  - 401 uses the error format
  - admin and user stats views differ
  - `/health` works without a key
- **Rate limit:** 429 with `Retry-After`.
- **`/v1/models`:** aggregates results while one upstream is down.
- **No leaks:** after requests with a known key, neither the captured logs nor the stats file contain it.

**Coverage and smoke test:** coverage is measured with `cargo llvm-cov`, aiming for 80% or more; it's reported, not enforced in CI. Before the work is called done, a manual smoke test runs against the owner's local Ollama.

## CI

- **Workflow:** GitHub Actions on pushes and pull requests to `main`.
- **Steps:**
  - `cargo fmt --all --check`
  - `cargo clippy --all-targets --locked -- -D warnings`
  - `cargo test --locked`
  - `cargo audit`, which reports advisories such as the one for `dotenv`
- **Setup:** the stable toolchain and a cargo cache. Third-party actions are pinned to commit SHAs.
- **Repo changes:** `Cargo.lock` is removed from `.gitignore` and committed, and `.gitignore` gains `stats.json*`.

## Docker

- **Build:** a multi-stage build using the official Rust image. The runtime stage is a minimal non-root image (distroless `cc` or slim Debian).
  - The build and runtime stages must use the same Debian release, so the binary's glibc matches.
  - Image tags are verified when building.
- **Environment:** the image sets `HOST=0.0.0.0`, `PORT=8080` and `STATS_PATH=/data/stats.json`. `/data` is a volume, and port 8080 is exposed.
- **Health check:** there's no `HEALTHCHECK` instruction, because distroless has no curl. The README shows how to probe `/health` from Compose or Kubernetes.
- **`.dockerignore`:** excludes `target/`, `.env`, `stats.json*` and `.git`.
- **Host Ollama:** the README shows how to reach an Ollama running on the host (`host.docker.internal`).

## README and metadata

- **README:** rewritten to match the code. It covers:
  - a one-paragraph description
  - a quick start with cargo and with Docker
  - the configuration table
  - routing and fallback rules with examples
  - each endpoint, with curl examples that include `Authorization`
  - the auth flow: user and admin keys, key IDs, rate limits, `Retry-After`
  - the error format
  - stats persistence
  - limitations
  - development commands
  - the roadmap
- **Claims (Claude's call):** "high-performance", "production-ready" and "low latency" are removed; there's no benchmark this round.
- **Fixes:** the `yourusername` placeholder, the middleware order and the typos. A CI badge is added.
- **`.env.example`:** rewritten for the new variables, including `OLLAMA_FALLBACK=openai/gpt-6-terra`. No model name is hard-coded in Rust.
- **`Cargo.toml`:** gains `description`, `license = "Apache-2.0"` (matching `LICENSE`) and `repository`.
- **Untracked files:** `CONTEXT.md`, `CLAUDE.md` and `stats.json` are left alone.

## Delivery order

The work happens on a branch from `main`, which includes the unpushed README commit `cfdb482`. Each step is one commit, passes fmt, clippy and the tests, and is followed by an explanation for the owner.

1. **Safety and foundations:**
   - `lib.rs` plus a thin `main.rs`
   - config with startup checks
   - `.env` loaded before logging
   - the key store, with no keys in logs
   - `/health` and the middleware order
   - `.gitignore` and `Cargo.lock`
   - fmt and clippy clean
   - CI
2. **Router:**
   - routing and forwarding
   - the error format
   - fallback, including for streams
   - the SSE reader and usage
   - `/v1/models`
   - removing the old providers
3. **Stats and limits:** stats by key ID with atomic periodic saves, and a configurable rate limit with `Retry-After`.
4. **Ship:** the Dockerfile, the README, `.env.example` and `Cargo.toml` metadata.

Then, only when the owner says so: push, open a pull request, and set the description and topics with `gh repo edit`. The owner re-pins the repo.

## Left to the builder

- Exact module layout, type names and crate versions.
- Whether integration tests use actix's test server or a real server on port 0.
- The Docker base images, and the build-cache approach in Docker and CI.
- How much of an upstream error body is logged, and the wording of startup warnings.
- The `created` value in merged `/v1/models` entries.
