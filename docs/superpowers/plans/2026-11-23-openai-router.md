# ai-gateway OpenAI-compatible router: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn ai-gateway into a tested, documented router that puts hashed API keys, per-key rate limits, usage stats and a visible fallback in front of any OpenAI-compatible backend.

**Architecture:**
- **Crate layout:** a library crate (`src/lib.rs`) holds all logic. `src/main.rs` only loads `.env`, config and logging, then runs the actix server.
- **Request path:** Logger → Auth → RateLimit → Tracking → handler.
- **Chat handler:**
  - routes by model prefix;
  - forwards the body almost untouched through one shared `reqwest::Client`;
  - relays SSE through a framing reader that counts tokens;
  - tries the configured fallback once when the primary fails before anything reached the client.

**Tech stack:**
- **Language:** Rust stable (1.100 locally), edition 2021.
- **Runtime crates:**
  - actix-web 4.12, using `middleware::from_fn`
  - reqwest 0.12, with rustls and stream support
  - serde / serde_json, tracing, thiserror
  - dotenvy 0.15, sha2 0.11
- **Tests:** actix `test`, wiremock 0.6, tempfile 3.

**Spec:** `docs/superpowers/specs/2026-11-23-openai-router-design.md`. Read it alongside this plan; the plan argues from it.

## Global constraints

**Every commit:**
- Passes `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked`.
- Stages files by explicit path. Never use `git add -A`, `git add .` or `git commit -a`: `CLAUDE.md`, `CONTEXT.md`, `stats.json` (holds raw keys) and `.env` are untracked and must stay out of git.
- Uses a message of the form `<type>: <description>`, where type is feat, fix, refactor, docs, test, chore or ci. No attribution trailer.

**Not without the owner:** no push, pull request, `gh repo edit` or other GitHub change until the owner says so.

**Secrets:**
- No raw gateway key or upstream key may appear in logs, `stats.json`, `/v1/stats`, URLs, error bodies or `Debug` output.
- Never read the owner's `.env`. Manual runs happen from the scratchpad directory with variables set on the command line. dotenvy searches the current directory and its parents, so a run from inside the repo would load `.env`.

**Code rules:**
- No model name in Rust code. `gpt-6-terra` appears only in `.env.example`, the README and tests.
- Errors the gateway produces use `{"error":{"message":…,"type":…,"param":null,"code":…}}`.
- Non-test code has no `unwrap()`. `expect()` is allowed only for truly unreachable states, with the reason as its message.
- Mutexes recover from poisoning with `lock().unwrap_or_else(PoisonError::into_inner)`, so one panicking request can't break every later one.
- A middleware rejection is returned as `Ok(ServiceResponse)`, never `Err`. actix's `Logger` passes `Err` results through without logging them (actix-web 4.12.1, `src/middleware/logger.rs:384-387`), so a 401 or 429 returned as `Err` would be missing from the access log.
- actix-web uses `http` 0.2 and reqwest uses `http` 1.x, so their `StatusCode` and `HeaderValue` are different types. Convert with `StatusCode::from_u16(s.as_u16())` and `HeaderValue::from_bytes(v.as_bytes())`.

**Dependencies:**
- The only additions are `dotenvy = "0.15"` and `sha2 = "0.11"`, plus dev-dependencies `wiremock = "0.6"` and `tempfile = "3"`.
- Remove every dependency that cargo reports as `unused dependency` (cargo 1.100 lints this).
- Tests need no extra crates: table tests are plain arrays and loops.

**Files:** each stays under 400 lines; split by responsibility when one grows past that.

**Checkpoints:** each of Steps 1–4 ends with one.
- Send the owner the explanation: what changed, how, the implications, and the Rust concepts behind it, tied to the actual lines.
- Wait for them to say continue.

## Review focus

The spec doesn't spell out these five failure modes, but each would bite a real user. Each is pinned by a test in the task that owns the code:

1. **Bodies between 256 KiB and `MAX_REQUEST_BYTES`** (long chats, base64 images) must be forwarded, not rejected by actix's default limit. Test: Task 10, `accepts_body_larger_than_actix_default`.
2. **Upgrading with the old `.env`** (`AI_PROVIDER`, no `UPSTREAMS`, `OPENAI_BASE_URL=https://api.openai.com`): startup fails and names `UPSTREAMS`. Once that is added, warnings name `AI_PROVIDER` and each base URL without a path. Test: Task 7, `old_env_file_gets_clear_messages`.
3. **An upstream stream that dies mid-way** aborts the client's connection instead of ending it cleanly. A clean end would make a truncated answer look complete. Test: Task 9, `relay_aborts_when_upstream_fails_mid_stream`.
4. **Stats in Docker:** the image runs as uid 65532, so `/data` must be owned by that user or every save fails. Test: Task 16, step "stats survive `docker stop`".
5. **The periodic flush actually runs:** a SIGKILL loses at most `STATS_FLUSH_SECS` of stats, not everything since start. Test: Task 14, `flusher_saves_on_its_interval`.

## File map (end state)

| Path | Responsibility |
|---|---|
| `src/main.rs` | `.env` → config → logging → stats load → server → final save |
| `src/lib.rs` | module declarations |
| `src/app.rs` | `AppState`, `build_app`, `/health`, the 404 fallback |
| `src/config/mod.rs` | `Config`, `LogFormat`, `ConfigError`; server, key, limit and stats variables |
| `src/config/upstreams.rs` | `Upstream`, `UpstreamKey`, `FallbackTarget`; upstream variables |
| `src/logging.rs` | `init_logging` |
| `src/error.rs` | `ApiError` (OpenAI error envelope) |
| `src/auth.rs` | `KeyId`, `Role`, `Caller`, `KeyStore`, `bearer_token`, `require_api_key` middleware |
| `src/rate_limit.rs` | `TokenBucket`, `RateLimiter`, `enforce_rate_limit` middleware |
| `src/stats/store.rs` | `KeyStats`, `StatsStore`, atomic persistence, `spawn_flusher` |
| `src/stats/http.rs` | `track_request` middleware, `GET /v1/stats` |
| `src/proxy/routing.rs` | `resolve_route` |
| `src/proxy/upstream.rs` | HTTP client, `send_chat`, `UpstreamFailure`, `should_fallback`, failure → response |
| `src/proxy/usage.rs` | `Usage`, `probe` (reads `model` and `usage` from JSON) |
| `src/proxy/sse.rs` | `SseFramer`, `StreamInspector`, `with_include_usage`, `relay_sse` |
| `src/proxy/chat.rs` | `POST /v1/chat/completions` |
| `src/proxy/models.rs` | `GET /v1/models` |
| `tests/common/mod.rs` | test keys, config builder, app init, fixtures (`#![allow(dead_code)]`) |
| `tests/{auth,chat,fallback,models,rate_limit,stats,no_leaks}.rs` | integration tests, using `actix_web::test::init_service(build_app(..))` and wiremock upstreams |

The old `handlers/`, `middleware/`, `models/`, `providers/` and `tracking/` modules keep working until their replacement task deletes them (Tasks 10 and 13–14).

---

## Step 1: Safety and foundations

### Task 1: Repo foundations (no behavior change)

**Files:**
- Create: `src/lib.rs`, `.github/workflows/ci.yml`
- Modify:
  - `.gitignore`
  - `src/main.rs:1-12`
  - `src/handlers/chat.rs`
  - `src/middleware/auth.rs:32,109`
  - `src/middleware/rate_limit.rs:82`
  - `src/providers/mod.rs:18,26`
- First commit of: `Cargo.lock`, the spec, this plan

**Interfaces:**
- Produces: crate `ai_gateway` with `pub mod handlers; pub mod middleware; pub mod models; pub mod providers; pub mod tracking;`.
- `main.rs` imports everything through `ai_gateway::…`.

- [ ] **Step 1: Create the branch and commit the docs**

```bash
git switch -c feat/openai-router
git add docs/superpowers/specs/2026-11-23-openai-router-design.md docs/superpowers/plans/2026-11-23-openai-router.md
git commit -m "docs: add OpenAI router design and implementation plan"
```

- [ ] **Step 2: Update `.gitignore`.** Delete the `# Dependencies` and `Cargo.lock` lines. Add `stats.json*` under the comment `# Gateway stats (usage per key)`. Run `git status --short`. Expected: `?? Cargo.lock` is listed and `stats.json` is not.
- [ ] **Step 3: Split the library out of `main.rs`.** Move the five `mod` lines from `main.rs` into `src/lib.rs` as `pub mod`, and change `main.rs` to import from `ai_gateway::`. Nothing else moves.
- [ ] **Step 4: Format.** Strip trailing whitespace from `src/handlers/chat.rs` first, because rustfmt aborts on lines 35 and 44. Then run `cargo fmt --all`.
- [ ] **Step 5: Fix the clippy baseline.** Run `cargo clippy --all-targets -- -D warnings`. Expected findings and fixes:
  - `auth.rs:32`: redundant field name.
  - `auth.rs:109`: replace `Box::pin(async move { fut.await })` with `Box::pin(fut)`.
  - `rate_limit.rs:82`: use `get_mut()` instead of `lock()`.
  - `providers/mod.rs:18`: rename the variant `ProviderError::ProviderError` to `ProviderError::Status`, including its uses in `providers/mod.rs:26` and `handlers/chat.rs`.

  The dead-code findings go away once the modules are `pub` in the library. Fix anything else with the smallest change.
- [ ] **Step 6: Write `.github/workflows/ci.yml`.**
  - Resolve each `<sha>` with `gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`, using the tag from `gh release view -R <owner>/<repo> --json tagName`.
  - For `dtolnay/rust-toolchain`, use the head of `master`.
  - Keep the tag as a trailing comment.

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
permissions:
  contents: read
env:
  CARGO_TERM_COLOR: always
jobs:
  check:
    name: fmt, clippy, test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha> # <tag>
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@<sha> # master
        with:
          toolchain: stable
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@<sha> # <tag>
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets --locked -- -D warnings
      - run: cargo test --locked
  audit:
    name: dependency audit
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha> # <tag>
        with:
          persist-credentials: false
      - uses: taiki-e/install-action@<sha> # <tag>
        with:
          tool: cargo-audit
      - run: cargo audit
```

- [ ] **Step 7: Verify.** All three global commands exit 0. The only other output is cargo's `unused dependency thiserror` warning; Task 2 starts using it.
- [ ] **Step 8: Commit**

```bash
git add .gitignore Cargo.lock src/lib.rs src/main.rs src/handlers src/middleware src/providers src/models src/tracking .github/workflows/ci.yml
git commit -m "chore: split library crate, commit Cargo.lock, add CI"
```

### Task 2: OpenAI error envelope

**Files:**
- Create: `src/error.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `ai_gateway::error::ApiError`.
- Derives: `#[derive(Debug, Clone, thiserror::Error)]` with `#[error("{message}")]`.
- Fields (private): status, type, code, message, and `retry_after: Option<HeaderValue>`.
- Implements `actix_web::ResponseError`: `status_code`, plus `error_response`, which returns a JSON envelope and sets `Retry-After` when present.

| Constructor | Status | `type` | `code` |
|---|---|---|---|
| `invalid_api_key()` | 401 | `invalid_request_error` | `invalid_api_key` |
| `rate_limited(retry_after_secs: u64)` | 429 | `rate_limit_error` | `rate_limit_exceeded`, plus `Retry-After` |
| `bad_request(message: impl Into<String>)` | 400 | `invalid_request_error` | null |
| `payload_too_large(limit_bytes: usize)` | 413 | `invalid_request_error` | `payload_too_large` |
| `not_found(message: impl Into<String>)` | 404 | `invalid_request_error` | `not_found` |
| `upstream_unavailable(upstream: &str)` | 502 | `server_error` | `upstream_unavailable` |
| `upstream_timeout(upstream: &str)` | 504 | `server_error` | `upstream_timeout` |
| `upstream_error(upstream: &str)` | 502 | `server_error` | `upstream_error` |
| `upstream_rate_limited(upstream: &str, retry_after: Option<HeaderValue>)` | 429 | `rate_limit_error` | `upstream_rate_limited`, plus the upstream's `Retry-After` |
| `upstream_auth_failed(upstream: &str)` | 502 | `server_error` | `upstream_auth_failed` |

Messages for the upstream errors name the upstream and are generic. They never include anything the upstream sent.

- [ ] **Step 1: Write the failing tests** in the `#[cfg(test)]` module of `src/error.rs`. `render` is a test helper that returns status, headers and the JSON body:

```rust
#[actix_web::test]
async fn bad_request_uses_the_openai_envelope() {
    let (status, headers, body) = render(ApiError::bad_request("`model` is required")).await;
    assert_eq!(status, 400);
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    assert_eq!(body, json!({"error": {"message": "`model` is required",
        "type": "invalid_request_error", "param": null, "code": null}}));
}

#[actix_web::test]
async fn rate_limited_sets_retry_after() {
    let (status, headers, body) = render(ApiError::rate_limited(7)).await;
    assert_eq!(status, 429);
    assert_eq!(headers.get("retry-after").unwrap(), "7");
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
}

#[actix_web::test]
async fn constructors_match_the_table() {
    // One (error, status, type, code) row per constructor in the table above.
}
```

- [ ] **Step 2: Run** `cargo test --lib error`. Expected: compile failure, `ApiError` not found.
- [ ] **Step 3: Implement `ApiError`** as specified.
- [ ] **Step 4: Run** `cargo test --lib error`. Expected: 3 passed.
- [ ] **Step 5: Commit** `src/error.rs` and `src/lib.rs` with the message `feat: add OpenAI-style error responses`.

### Task 3: Key store

**Files:**
- Create: `src/auth.rs`
- Modify: `src/lib.rs`, `Cargo.toml` (`cargo add sha2@0.11`)

**Interfaces (produces):**

```rust
pub struct KeyId(String); // Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Display, serde(transparent)
impl KeyId {
    pub fn from_raw_key(raw: &str) -> KeyId; // "key_" + first 12 lowercase hex chars of SHA-256(raw)
    pub fn parse(s: &str) -> Option<KeyId>;  // exactly key_[0-9a-f]{12}
    pub fn as_str(&self) -> &str;
}
pub enum Role { User, Admin }                             // Clone, Copy, Debug, PartialEq, Eq; serde lowercase
pub struct Caller { pub key_id: KeyId, pub role: Role }   // Clone, Debug, PartialEq
pub struct KeyStore { /* HashMap<[u8; 32], Caller> */ }   // manual Debug: key IDs and roles only
pub enum KeyStoreError { NoKeys, InBothLists(KeyId) }     // thiserror
impl KeyStore {
    pub fn new(user_keys: &[String], admin_keys: &[String]) -> Result<KeyStore, KeyStoreError>;
    pub fn authenticate(&self, token: &str) -> Option<Caller>; // hash the token, then look it up
    pub fn count(&self, role: Role) -> usize;
}
pub fn bearer_token(header: Option<&str>) -> Option<&str>; // case-insensitive "Bearer", token trimmed
```

Hash with `let hash: [u8; 32] = Sha256::digest(raw.as_bytes()).into();`. Hex-encode with `{:02x}` per byte; no `hex` crate. A key repeated within one list is accepted once.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn key_id_is_stable_and_shaped() {
    assert_eq!(KeyId::from_raw_key("abc").as_str(), "key_ba7816bf8f01"); // SHA-256("abc") = ba7816bf8f01…
}
#[test] fn parse_accepts_only_key_ids() {
    assert!(KeyId::parse("key_ba7816bf8f01").is_some());
    for bad in ["key_BA7816BF8F01", "key_ba7816bf8f0", "key_ba7816bf8f01a", "sk-123", ""] {
        assert!(KeyId::parse(bad).is_none(), "{bad}");
    }
}
#[test] fn authenticate_returns_the_callers_role() // user key → Role::User, admin key → Role::Admin; key_id = from_raw_key(key)
#[test] fn unknown_or_empty_tokens_are_rejected()  // "nope" and "" → None
#[test] fn a_key_in_both_lists_is_rejected()       // Err(KeyStoreError::InBothLists(KeyId::from_raw_key(k)))
#[test] fn no_keys_is_rejected()                   // Err(KeyStoreError::NoKeys)
#[test] fn debug_shows_ids_not_keys()              // format!("{store:?}") contains the ID, not the key
#[test] fn bearer_token_parsing() {
    assert_eq!(bearer_token(Some("Bearer abc")), Some("abc"));
    assert_eq!(bearer_token(Some("bearer abc")), Some("abc"));
    assert_eq!(bearer_token(Some("Bearer   abc  ")), Some("abc"));
    for bad in [None, Some("Basic abc"), Some("Bearer "), Some("Bearer"), Some("abc")] {
        assert_eq!(bearer_token(bad), None);
    }
}
```

- [ ] **Step 2: Run** `cargo test --lib auth`. Expected: compile failure.
- [ ] **Step 3: Implement** the interfaces above in `src/auth.rs`.
- [ ] **Step 4: Run** `cargo test --lib auth`. Expected: 8 passed.
- [ ] **Step 5: Commit** `src/auth.rs`, `src/lib.rs`, `Cargo.toml` and `Cargo.lock` with the message `feat: add hashed API key store with key IDs`.

### Task 4: Config for server and keys

**Files:**
- Create: `src/config/mod.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `KeyStore::new`, `KeyStoreError` and `KeyId::from_raw_key` (Task 3).
- Produces:

```rust
pub enum LogFormat { #[default] Text, Json }        // Debug, Clone, Copy, PartialEq, Eq, Default
pub struct Config { pub host: String, pub port: u16, pub log_format: LogFormat, pub keys: KeyStore } // Debug
pub struct ConfigError { pub variable: String, pub message: String } // Debug, PartialEq; thiserror "{variable}: {message}"
impl Config {
    pub fn from_env() -> Result<(Config, Vec<String>), ConfigError>; // from_lookup(|n| std::env::var(n).ok())
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<(Config, Vec<String>), ConfigError>; // Vec = warnings
}
```

**Rules for this task** (the spec's table, plus these):
- A blank or whitespace-only value counts as unset.
- `HOST` defaults to `127.0.0.1`. `PORT` defaults to 8080 and must be a u16 greater than 0.
- `LOG_FORMAT` is `text` or `json`, case-insensitive.
- Key lists are comma-separated; entries are trimmed and empty entries dropped. At least one key is required.
- A key in both lists is an error naming its key ID. A key shorter than 32 characters gets a warning naming the list and the key ID.
- `AI_PROVIDER` gets a warning.
- No error or warning text ever contains a key.
- Put the number parsing in a private helper here, `positive<T: FromStr + PartialOrd + From<u8>>(…, name, default)`. Tasks 7, 13 and 14 reuse it.

- [ ] **Step 1: Write the failing tests.** `lookup(&[(&str, &str)])` is a test helper that builds the closure. Use `const LONG: &str = "0123456789abcdef0123456789abcdef";` (32 chars).

```rust
#[test] fn defaults_apply_with_only_a_key()       // host "127.0.0.1", port 8080, LogFormat::Text, no warnings
#[test] fn missing_keys_is_an_error()             // err.variable == "GATEWAY_API_KEYS"
#[test] fn admin_keys_alone_are_enough()
#[test] fn key_in_both_lists_names_the_key_id_only() // message contains from_raw_key(LONG).as_str(), not LONG
#[test] fn short_key_warns_by_key_id()            // warning mentions "GATEWAY_API_KEYS" and the ID, not "short-key"
#[test] fn port_must_be_a_nonzero_u16()           // "0", "65536", "http" → variable "PORT"; "9000" → 9000
#[test] fn log_format_is_text_or_json()           // "JSON" → Json; "yaml" → Err with variable "LOG_FORMAT"
#[test] fn ai_provider_is_only_a_warning()
#[test] fn blank_values_count_as_unset()          // PORT = "  " → 8080
#[test] fn keys_are_trimmed_and_empty_entries_dropped() // " a…, , b… " → two keys
#[test] fn debug_never_shows_keys()               // format!("{config:?}") contains neither key
```

- [ ] **Step 2: Run** `cargo test --lib config`. Expected: compile failure.
- [ ] **Step 3: Implement** `Config::from_lookup` and `from_env`.
- [ ] **Step 4: Run** `cargo test --lib config`. Expected: 11 passed.
- [ ] **Step 5: Commit** `src/config/mod.rs` and `src/lib.rs` with the message `feat: load and validate configuration from the environment`.

### Task 5: Wire in config, key store, logging and `/health`

**Files:**
- Create: `src/app.rs`, `src/logging.rs`, `tests/common/mod.rs`, `tests/auth.rs`
- Modify:
  - `src/main.rs`: rewritten.
  - `src/auth.rs`: add the middleware and the extractor.
  - `src/middleware/rate_limit.rs:149-154` and `src/middleware/tracking.rs:66-70,90-95`: read `Caller.key_id`; log `key_id`.
  - `src/handlers/chat.rs`: take a `caller: Caller` parameter; log `key_id`.
  - `src/handlers/stats.rs`: query `?key_id=`, response field `key_id`, delete `mask_key`.
  - `src/middleware/mod.rs`, `src/lib.rs`.
  - `Cargo.toml`: replace `dotenv` with `dotenvy@0.15`.
- Delete: `src/middleware/auth.rs`

**Interfaces:**
- Consumes: `Config`, `LogFormat` (Task 4); `KeyStore`, `Caller`, `bearer_token` (Task 3); `ApiError` (Task 2).
- Produces:

```rust
// app.rs — temporary shape. `provider` goes in Task 10, `limiter` changes in Task 13, `stats` in Task 14.
pub struct AppState {
    pub config: Config,
    pub stats: Arc<RwLock<RequestTracker>>,
    pub limiter: Arc<RateLimiter>,
    pub provider: Arc<dyn LLMProvider>,
}
pub fn build_app(state: web::Data<AppState>) -> App<impl ServiceFactory<ServiceRequest, Config = (),
    Response = ServiceResponse<impl MessageBody>, Error = actix_web::Error, InitError = ()>>;
// auth.rs
pub async fn require_api_key<B: MessageBody>(state: web::Data<AppState>, req: ServiceRequest, next: Next<B>)
    -> Result<ServiceResponse<EitherBody<B>>, actix_web::Error>;
impl FromRequest for Caller { /* reads the extension; missing → ApiError::invalid_api_key() */ }
// logging.rs
pub fn init_logging(format: LogFormat); // RUST_LOG filter, default "info"; text or JSON
```

`build_app` order (the last `.wrap` runs first):

```rust
App::new()
    .app_data(state.clone())
    .app_data(web::Data::from(state.stats.clone()))    // legacy handlers, until Task 14
    .app_data(web::Data::from(state.provider.clone())) // legacy chat handler, until Task 10
    .wrap(Logger::default())                           // outermost: also logs 401/429
    .route("/health", web::get().to(health))           // public, returns 200 "ok"
    .service(web::scope("/v1")
        .wrap(TrackingMiddleware::new(state.stats.clone()))
        .wrap(RateLimitMiddleware::new(state.limiter.clone()))
        .wrap(from_fn(require_api_key))                // runs first
        .route("/chat/completions", web::post().to(chat_completions))
        .route("/stats", web::get().to(get_stats)))
    .default_service(web::to(not_found))               // ApiError::not_found naming method and path
```

**`require_api_key`:**
- Valid key: insert `Caller`, then return `next.call(req).await.map(ServiceResponse::map_into_left_body)`.
- Rejection: log `warn!(path = %req.path(), "rejected request without a valid API key")`, with no header or token. Return `Ok(req.into_response(ApiError::invalid_api_key().error_response()).map_into_right_body())`.

**`main.rs` order:**
1. `dotenvy::dotenv()`. Keep the result; don't act on it yet.
2. `Config::from_env()`.
3. `init_logging(` the configured format, or `LogFormat::Text` if config failed `)`.
4. Act on the dotenv result:
   - NotFound: info.
   - `dotenvy::Error::LineParse(_, _)`: log a generic ".env has a syntax error" and exit 1. Never log the line itself, because it may hold a secret.
   - Any other error: log it and exit 1.
5. Config error: `error!` and exit 1. Otherwise log each warning with `warn!`.
6. The legacy provider and stats setup, moved here unchanged.
7. Bind `(config.host, config.port)`.
8. Final save.

`main` returns `std::process::ExitCode`.

**`tests/common/mod.rs`:**
- Constants: `USER_KEY = "test-user-key-0123456789abcdef01234567"` and `ADMIN_KEY = "test-admin-key-0123456789abcdef0123456"`, both at least 32 characters.
- `config(overrides: &[(&str, &str)]) -> Config`: base variables are the two keys; overrides win; `("X", "")` unsets X.
- `state(config) -> AppState`: the provider is `OllamaProvider::new("http://127.0.0.1:9")`, which no test calls.
- `init(state)`, `bearer(key)`, `json_body(resp)`.

- [ ] **Step 1: Write the failing tests** in `tests/auth.rs`:

```rust
#[actix_web::test] async fn health_needs_no_key()            // GET /health → 200, body "ok"
#[actix_web::test] async fn missing_key_gets_openai_401()     // GET /v1/stats → 401; body["error"]["code"] == "invalid_api_key"
#[actix_web::test] async fn wrong_key_gets_401()              // Bearer wrong → 401
#[actix_web::test] async fn lowercase_bearer_scheme_is_accepted()
#[actix_web::test] async fn user_and_admin_keys_are_accepted() // 200 for each
#[actix_web::test] async fn old_v1_health_route_is_gone()      // GET /v1/health with USER_KEY → 404, code "not_found"
#[actix_web::test] async fn stats_show_key_id_not_key()        // user GET /v1/stats: body["key_id"] == KeyId::from_raw_key(USER_KEY).as_str();
                                                               // the raw body text doesn't contain USER_KEY
```

- [ ] **Step 2: Run** `cargo test --test auth`. Expected: compile failure (`app` module missing).
- [ ] **Step 3: Implement** the interfaces above. Switch the legacy handlers and middlewares from `ValidatedApiKey` to `Caller`. Remove every log of a key or Authorization header (`auth.rs:83-84,112`, `tracking.rs:91`, `chat.rs:46,79`).
- [ ] **Step 4: Run** `cargo test --test auth`. Expected: 7 passed. Then run the three global commands.
- [ ] **Step 5: Check fail-fast by hand:** `cargo build`, then `cd "$SCRATCHPAD" && env -i PATH="$PATH" "$REPO/target/debug/ai-gateway"; echo "exit=$?"`. Expected: an error line naming `GATEWAY_API_KEYS`, then `exit=1`.
- [ ] **Step 6: Commit** every file listed above, including the deletion via `git rm src/middleware/auth.rs`, with the message `feat: authenticate with hashed keys, load .env before logging, public /health`.
- [ ] **Step 7: Checkpoint for Step 1.** Explain Tasks 1–5 to the owner. Tell them their `.env` now needs `GATEWAY_API_KEYS`, because there is no built-in key any more. Wait for them to say continue.

---

## Step 2: Router

### Task 6: Routing

**Files:**
- Create: `src/proxy/mod.rs`, `src/proxy/routing.rs`
- Modify: `src/lib.rs`

**Interfaces (produces):**

```rust
pub struct Route<'a> { pub upstream: &'a str, pub model: &'a str }  // Debug, PartialEq
pub enum RouteError {                                                // Debug, PartialEq, thiserror; messages name the model
    EmptyModel,
    MissingModelAfterPrefix { upstream: String },
    UnknownPrefix { model: String, valid_prefixes: Vec<String> },   // e.g. ["ollama/", "openai/"]
}
pub fn resolve_route<'a>(model: &'a str, upstreams: &[&'a str], default_upstream: Option<&'a str>)
    -> Result<Route<'a>, RouteError>;
```

Split at the first `/`. Prefix matching is exact and case-sensitive.

- [ ] **Step 1: Write the failing tests.** The table is the spec's, copied exactly.

```rust
#[test]
fn routes_follow_the_spec_table() {
    let ups = ["ollama", "openai"];
    let hf = "hf.co/unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL";
    let prefixed_hf = format!("ollama/{hf}");
    let cases = [
        ("ollama/gemma4:e2b", Some("openai"), ("ollama", "gemma4:e2b")),
        ("openai/gpt-6-terra", None, ("openai", "gpt-6-terra")),
        (prefixed_hf.as_str(), None, ("ollama", hf)),
        (hf, Some("ollama"), ("ollama", hf)),
        ("gemma4:e2b", Some("ollama"), ("ollama", "gemma4:e2b")),
    ];
    for (model, default, (upstream, routed)) in cases {
        assert_eq!(resolve_route(model, &ups, default), Ok(Route { upstream, model: routed }), "{model}");
    }
}
#[test] fn unprefixed_model_without_default_is_an_error() // Err(UnknownPrefix { valid_prefixes: ["ollama/", "openai/"], .. })
#[test] fn prefix_match_is_case_sensitive()               // "Ollama/x" with no default → UnknownPrefix
#[test] fn prefix_without_model_is_an_error()             // "ollama/" → MissingModelAfterPrefix
#[test] fn empty_model_is_an_error()                      // "" → EmptyModel
```

- [ ] **Step 2: Run** `cargo test --lib routing`. Expected: compile failure.
- [ ] **Step 3: Implement** `resolve_route`.
- [ ] **Step 4: Run** `cargo test --lib routing`. Expected: 5 passed.
- [ ] **Step 5: Commit** with the message `feat: route requests by model prefix`.

### Task 7: Config for upstreams

**Files:**
- Create: `src/config/upstreams.rs`
- Modify:
  - `src/config/mod.rs`
  - `tests/common/mod.rs`: base variables gain `UPSTREAMS=primary,backup`, plus `PRIMARY_BASE_URL` and `BACKUP_BASE_URL`, both set to `http://127.0.0.1:9/v1`.

**Interfaces (produces):**

```rust
pub struct Upstream { pub name: String, pub base_url: String /* trailing '/' trimmed */,
                      pub api_key: Option<UpstreamKey>, pub fallback: Option<FallbackTarget> } // Debug, Clone
impl Upstream { pub fn chat_url(&self) -> String; pub fn models_url(&self) -> String } // base + "/chat/completions", "/models"
pub struct UpstreamKey(String);  // Clone; Debug prints "UpstreamKey(<redacted>)"; pub fn expose(&self) -> &str
pub struct FallbackTarget { pub upstream: String, pub model: String } // Debug, Clone, PartialEq, Eq; Display "upstream/model"
// New Config fields:
pub upstreams: Vec<Upstream>,          // in UPSTREAMS order
pub default_upstream: Option<String>,
pub connect_timeout: Duration,         // UPSTREAM_CONNECT_TIMEOUT_SECS, default 10
pub upstream_timeout: Duration,        // UPSTREAM_TIMEOUT_SECS, default 300
pub max_request_bytes: usize,          // MAX_REQUEST_BYTES, default 10485760
impl Config {
    pub fn upstream(&self, name: &str) -> Option<&Upstream>;
    pub fn upstream_names(&self) -> Vec<&str>;
    pub fn summary_lines(&self) -> Vec<String>; // bind address, each upstream with base URL and fallback,
                                                // default upstream, key counts. Never a key.
}
```

**Rules:** the spec's validation list applies. In addition, my call: a base URL with credentials, a query string or a fragment is rejected. The message points to `<NAME>_API_KEY`. Error messages never echo a base URL. Parse with `reqwest::Url::parse`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn parses_upstreams_with_fallback() // UPSTREAMS=ollama,openai, both URLs, OPENAI_API_KEY, OLLAMA_FALLBACK=openai/gpt-6-terra →
                                            // fallback == FallbackTarget { upstream: "openai", model: "gpt-6-terra" };
                                            // chat_url == "http://localhost:11434/v1/chat/completions"
#[test] fn upstreams_is_required()                  // variable "UPSTREAMS"
#[test] fn upstream_names_are_lowercase_alnum_underscore() // "Ollama", "my-vllm", "a b" → Err with variable "UPSTREAMS"
#[test] fn duplicate_upstreams_are_rejected()
#[test] fn base_url_is_required_and_http()          // missing, "ftp://x", "localhost:11434" → Err with variable "OLLAMA_BASE_URL"
#[test] fn base_url_with_credentials_query_or_fragment_is_rejected()
#[test] fn trailing_slash_is_trimmed()              // "http://h/v1/" → chat_url "http://h/v1/chat/completions"
#[test] fn default_upstream_must_exist()            // variable "DEFAULT_UPSTREAM"
#[test] fn fallback_must_name_upstream_and_model()  // "openai", "nope/x", "openai/" → Err with variable "OLLAMA_FALLBACK"
#[test] fn timeouts_and_limit_have_defaults()       // 10 s, 300 s, 10_485_760
#[test] fn zero_timeouts_are_rejected()
#[test] fn upstream_key_is_redacted_in_debug_and_summary()
#[test]
fn old_env_file_gets_clear_messages() {
    let old = [("AI_PROVIDER", "ollama"), ("OLLAMA_BASE_URL", "http://localhost:11434"),
               ("OPENAI_BASE_URL", "https://api.openai.com"), ("OPENAI_API_KEY", "sk-old"),
               ("GATEWAY_API_KEYS", LONG)];
    assert_eq!(Config::from_lookup(lookup(&old)).unwrap_err().variable, "UPSTREAMS");
    let fixed: Vec<_> = old.iter().copied().chain([("UPSTREAMS", "ollama,openai")]).collect();
    let (_, warnings) = Config::from_lookup(lookup(&fixed)).unwrap();
    for name in ["AI_PROVIDER", "OLLAMA_BASE_URL", "OPENAI_BASE_URL"] {
        assert!(warnings.iter().any(|w| w.contains(name)), "{name}");
    }
}
```

- [ ] **Step 2: Run** `cargo test --lib config`. Expected: the new tests fail to compile.
- [ ] **Step 3: Implement** `src/config/upstreams.rs` and the new `Config` fields. Update the `tests/common` base variables. `main.rs` keeps its legacy providers until Task 10.
- [ ] **Step 4: Run** the three global commands. Expected: all pass, including `tests/auth.rs`.
- [ ] **Step 5: Commit** with the message `feat: configure upstreams, fallbacks and timeouts`.

### Task 8: Upstream client and failure mapping

**Files:**
- Create: `src/proxy/upstream.rs`
- Modify: `src/proxy/mod.rs`, `Cargo.toml` (`cargo add --dev wiremock@0.6`)

**Interfaces:**
- Consumes: `Config`, `Upstream`, `UpstreamKey` (Task 7); `ApiError` (Task 2).
- Produces:

```rust
pub fn build_http_client(config: &Config) -> reqwest::Result<reqwest::Client>;
// connect_timeout and read_timeout(upstream_timeout); no total timeout; redirect::Policy::none();
// user_agent concat!("ai-gateway/", env!("CARGO_PKG_VERSION"))
pub enum UpstreamFailure {                    // Debug; Display never includes the body
    Unreachable(String),                      // from e.without_url(), including its source chain (e.g. "connection refused")
    Timeout,                                  // reqwest::Error::is_timeout
    Status { status: StatusCode, retry_after: Option<HeaderValue>, content_type: Option<HeaderValue>, body: Bytes },
}                                             // StatusCode and HeaderValue are actix_web::http types
pub async fn send_chat(client: &reqwest::Client, upstream: &Upstream, body: Vec<u8>)
    -> Result<reqwest::Response, UpstreamFailure>; // POST chat_url, JSON content type, bearer only if api_key; non-2xx → Status
pub async fn read_body(response: reqwest::Response) -> Result<Bytes, UpstreamFailure>;
pub fn should_fallback(failure: &UpstreamFailure) -> bool; // Unreachable, Timeout, 5xx and 429 only
pub fn failure_response(upstream: &str, failure: &UpstreamFailure) -> HttpResponse; // the spec's error table; other
                                                           // non-2xx/non-4xx statuses (e.g. 3xx) → upstream_error
pub fn describe_failure(upstream: &Upstream, failure: &UpstreamFailure) -> String; // for logs: first 512 bytes of the
                                                           // body (lossy UTF-8), with the upstream's key replaced by "[redacted]"
```

- [ ] **Step 1: Write the failing tests.** `status(code)` builds a `Status` failure with body `{"error":{"message":"Incorrect API key provided: sk-proj-****abcd"}}`. `closed_port_url()` binds `127.0.0.1:0`, drops the listener and returns `http://127.0.0.1:<port>/v1`.

```rust
#[test] fn fallback_triggers_match_the_spec() {
    assert!(should_fallback(&UpstreamFailure::Unreachable("refused".into())));
    assert!(should_fallback(&UpstreamFailure::Timeout));
    for s in [500, 502, 503, 429] { assert!(should_fallback(&status(s)), "{s}"); }
    for s in [400, 401, 403, 404, 413, 422, 302] { assert!(!should_fallback(&status(s)), "{s}"); }
}
#[actix_web::test] async fn failures_map_to_the_error_table()
    // Unreachable → 502 upstream_unavailable; Timeout → 504 upstream_timeout; 500 → 502 upstream_error;
    // 401 and 403 → 502 upstream_auth_failed; 302 → 502 upstream_error;
    // 429 with Retry-After "20" → 429 upstream_rate_limited, header "20".
    // In every case the client body lacks "sk-proj".
#[actix_web::test] async fn other_4xx_bodies_pass_through() // 404 with body B and JSON content type → 404, identical bytes, same content type
#[test] fn described_failures_are_truncated_and_redacted()   // body = key + 2000 × "x" → contains "[redacted]", not the key, under 700 chars
#[actix_web::test] async fn send_chat_sends_only_the_upstream_key() // wiremock: with a key, header "Bearer <key>"; without, no authorization header
#[actix_web::test] async fn send_chat_classifies_refused_connections() // closed_port_url() → Unreachable
#[actix_web::test] async fn send_chat_classifies_timeouts()            // mock delays 1 s; upstream_timeout 200 ms → Timeout
#[actix_web::test] async fn send_chat_returns_status_failures_with_body() // 503 with body "busy" → Status { 503, body "busy" }
```

- [ ] **Step 2: Run** `cargo test --lib proxy::upstream`. The module path keeps Task 7's `upstream…` config tests out of the count. Expected: compile failure.
- [ ] **Step 3: Implement** `src/proxy/upstream.rs`.
- [ ] **Step 4: Run** `cargo test --lib proxy::upstream`. Expected: 8 passed.
- [ ] **Step 5: Commit** with the message `feat: add upstream client and error mapping`.

### Task 9: SSE relay and usage

**Files:**
- Create: `src/proxy/usage.rs`, `src/proxy/sse.rs`
- Modify: `src/proxy/mod.rs`

**Interfaces (produces):**

```rust
// usage.rs
pub struct Usage { #[serde(default)] pub prompt_tokens: u64, #[serde(default)] pub completion_tokens: u64 } // Deserialize, Clone, Copy, Debug, PartialEq, Default
pub struct ResponseProbe { pub model: Option<String>, pub usage: Option<Usage>, pub choices: Option<Vec<serde::de::IgnoredAny>> }
pub fn probe(json: &[u8]) -> Option<ResponseProbe>; // None if not a JSON object
// sse.rs
pub struct SseFramer { /* Vec<u8> buffer */ }        // Default
impl SseFramer {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Bytes>; // complete events, each with its terminator: "\n\n", "\r\n\r\n" or "\r\r"
    pub fn finish(self) -> Option<Bytes>;                // leftover bytes, forwarded as-is
}
pub enum EventAction { Forward, Drop }
pub struct StreamSummary { pub usage: Option<Usage>, pub model: Option<String> }
pub struct StreamInspector { /* strip flag, last usage, first model */ }
impl StreamInspector {
    pub fn new(strip_usage_chunk: bool) -> Self;
    pub fn inspect(&mut self, event: &[u8]) -> EventAction;
    pub fn summary(&self) -> StreamSummary;
}
pub fn with_include_usage(rest: Map<String, Value>) -> (Map<String, Value>, bool); // bool = injected, so strip the chunk
pub fn relay_sse<S, E, F>(upstream: S, inspector: StreamInspector, on_finish: F)
    -> impl Stream<Item = Result<Bytes, actix_web::Error>> + 'static
where S: Stream<Item = Result<Bytes, E>> + 'static, E: std::fmt::Display + 'static, F: FnOnce(StreamSummary) + 'static;
```

**`inspect` rules:**
- Join the `data:` lines, stripping one leading space from each.
- No data, `[DONE]`, or JSON that doesn't parse → `Forward`.
- Record the first `model` seen and the last `usage` seen.
- `Drop` only when the strip flag is set and the event has `usage` and `choices: []`.

**`relay_sse`** uses `async_stream::stream!` and pins the upstream inside:
- Each upstream chunk goes through the framer, then the inspector, then is yielded.
- At the end, yield the framer's leftover and call `on_finish`.
- On an upstream error: `warn!`, call `on_finish`, then yield `Err(actix_web::error::ErrorBadGateway(..))`. That aborts the connection instead of ending the body cleanly.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn framer_joins_an_event_split_across_chunks() // b"data: {\"a\"" + b":1}\n" + b"\n" → [b"data: {\"a\":1}\n\n"]
#[test] fn framer_returns_several_events_from_one_chunk()
#[test] fn framer_handles_crlf_boundaries()            // "data: x\r\n\r\n" → one event, bytes unchanged
#[test] fn framer_keeps_a_multibyte_char_split_across_chunks() // "data: é\n\n" split between 0xC3 and 0xA9 → identical bytes
#[test] fn finish_returns_leftover_bytes()             // "data: partial" → Some(b"data: partial")
#[test] fn inspector_records_usage_and_model()         // last usage wins; first model wins
#[test] fn inspector_drops_the_injected_usage_chunk()  // strip=true, choices [] + usage → Drop; summary has usage 5/2
#[test] fn inspector_keeps_usage_when_client_asked()   // strip=false → Forward
#[test] fn inspector_keeps_content_chunks_that_carry_usage() // strip=true, non-empty choices + usage → Forward
#[test] fn inspector_forwards_done_comments_and_non_json()   // "data: [DONE]", ": keep-alive", "data: {nope" → Forward
#[test] fn include_usage_is_injected_only_when_not_requested()
    // missing stream_options → (include_usage true, true); {"include_usage": true} → unchanged, false;
    // {"include_usage": false} → true, true; {"foo": 1} → keeps foo, true; null → replaced with an object, true
#[actix_web::test] async fn relay_forwards_events_and_reports_summary() // futures::stream::iter of chunks → output bytes, then on_finish
#[actix_web::test]
async fn relay_aborts_when_upstream_fails_mid_stream() {
    let upstream = futures::stream::iter(vec![Ok(Bytes::from_static(b"data: {}\n\n")), Err("boom")]);
    let out: Vec<_> = relay_sse(upstream, StreamInspector::new(false), |_| {}).collect().await;
    assert_eq!(out.len(), 2);
    assert!(out[0].is_ok() && out[1].is_err());
}
```

- [ ] **Step 2: Run** `cargo test --lib proxy::`. Expected: compile failure.
- [ ] **Step 3: Implement** `usage.rs` and `sse.rs`.
- [ ] **Step 4: Run** `cargo test --lib proxy::`. Expected: all pass.
- [ ] **Step 5: Commit** with the message `feat: relay SSE streams and read token usage`.

### Task 10: Forward chat completions and remove the old providers

**Files:**
- Create: `src/proxy/chat.rs`, `tests/chat.rs`
- Modify:
  - `src/app.rs`:
    - `AppState` drops `provider` and gains `http`, with `AppState::new`.
    - Add `.app_data(web::PayloadConfig::new(state.config.max_request_bytes))`.
    - Route to the new handler and drop the legacy `provider` data.
  - `src/main.rs`: no more provider building; log `config.summary_lines()` at startup; `AppState::new`.
  - `src/handlers/mod.rs`, `src/lib.rs`.
  - `Cargo.toml`:
    - Remove `async-trait`, `uuid`, `futures-util` and `log`, plus anything else cargo reports as unused.
    - reqwest features become `["rustls-tls", "stream"]`.
  - `tests/common/mod.rs`:
    - Add `UPSTREAM_KEY = "sk-test-upstream-key-0123456789abcdef"`.
    - `state()` uses `AppState::new`.
    - Add `closed_port_url()`, `mock_url(&MockServer) -> String` (`{uri}/v1`) and `chat_body(model) -> Value`.
    - Add the SSE fixtures: `USAGE_EVENT` (a `choices: []` event with usage 5/2) and `sse_stream()`. `sse_stream()` is one content event, one `delta: {}` stop event, `USAGE_EVENT`, then `data: [DONE]\n\n`.
- Delete: `src/providers/`, `src/models/`, `src/handlers/chat.rs`

**Interfaces:**
- Consumes: Tasks 6–9.
- Produces:

```rust
pub struct ChatRequest { pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stream: Option<bool>,
    #[serde(flatten)] pub rest: Map<String, Value> }               // Serialize, Deserialize, Debug
pub fn upstream_body(request: &ChatRequest, model: &str) -> Vec<u8>;
    // serializes a borrowed `#[derive(Serialize)] struct Outgoing<'a> { model: &'a str, stream: Option<bool>, #[serde(flatten)] rest: &'a Map<..> }`;
    // `expect("serializing a JSON object cannot fail")`
pub async fn chat_completions(state: web::Data<AppState>, caller: Caller, body: Result<web::Bytes, actix_web::Error>)
    -> Result<HttpResponse, ApiError>;
// app.rs
pub struct AppState { pub config: Config, pub http: reqwest::Client,
    pub stats: Arc<RwLock<RequestTracker>>, pub limiter: Arc<RateLimiter> }
impl AppState { pub fn new(config: Config, stats: Arc<RwLock<RequestTracker>>) -> reqwest::Result<AppState> } // limiter: RateLimiter::new(60)
// chat.rs private helpers, which Task 11 reuses:
enum Attempt { Complete { status: StatusCode, content_type: Option<HeaderValue>, body: Bytes }, Stream(reqwest::Response) }
async fn attempt(state: &AppState, upstream: &Upstream, body: Vec<u8>, stream: bool) -> Result<Attempt, UpstreamFailure>;
fn success_response(state: &web::Data<AppState>, caller: &Caller, upstream: &Upstream, model_sent: &str,
                    attempt: Attempt, strip_usage: bool) -> HttpResponse;
```

**Handler flow:**
1. Read the body.
   - `Err(e)` where `e.as_error::<PayloadError>()` is `Overflow` → `ApiError::payload_too_large(limit)`.
   - Any other read error → `bad_request("could not read the request body")`.
2. `serde_json::from_slice::<ChatRequest>` → on failure, `bad_request(format!("invalid request body: {e}"))`.
3. `resolve_route` → on failure, `bad_request(err.to_string())`.
4. If `stream == Some(true)`, apply `with_include_usage` to `rest`.
5. Call `attempt`. For non-streaming requests it includes `read_body`.
6. On success:
   - **Non-streaming:** return the upstream's status, its content type (default `application/json`) and the body bytes unchanged. Record usage under the label `"{upstream}/{probe.model or model_sent}"`.
   - **Streaming:** return 200 with `text/event-stream` and `cache-control: no-cache`. The body is `relay_sse(resp.bytes_stream(), StreamInspector::new(strip), on_finish)`, and `on_finish` records usage.
7. On failure: `warn!(key_id = %caller.key_id, upstream, error = %describe_failure(..))`, then `failure_response`.
8. Every response after step 5 carries `x-gateway-upstream: <name>`, inserted only if `HeaderValue::from_str` succeeds.

Usage goes into the legacy tracker with `record_tokens(key_id.as_str(), …)`, only when usage is present. Task 14 replaces this.

- [ ] **Step 1: Write the failing tests** in `tests/chat.rs`. All requests use `USER_KEY`. The primary mock has `PRIMARY_API_KEY = UPSTREAM_KEY`.

```rust
#[actix_web::test] async fn forwards_extra_fields_and_strips_the_prefix()
    // model "primary/m1" plus temperature 0.2, tools […], messages […] → upstream got model "m1" and the same
    // temperature, tools and messages; no "stream" key was added
#[actix_web::test] async fn returns_the_upstream_body_byte_for_byte() // odd spacing and key order kept; header x-gateway-upstream: primary
#[actix_web::test] async fn sends_the_upstream_key_not_the_gateway_key() // upstream authorization == "Bearer " + UPSTREAM_KEY
#[actix_web::test] async fn records_usage_from_the_response()   // upstream model "m1-2026", usage 5/2 → GET /v1/stats:
                                                                // total_prompt_tokens 5, completion 2, models_used {"primary/m1-2026": 1}
#[actix_web::test] async fn streams_events_through_unchanged()  // client sets include_usage true → client body == sse_stream(); content type text/event-stream
#[actix_web::test] async fn injects_include_usage_and_strips_the_chunk() // client sends no stream_options → upstream got
                                                                // stream_options.include_usage == true; client body == sse_stream() without
                                                                // USAGE_EVENT; stats show 5/2
#[actix_web::test] async fn upstream_401_becomes_502_without_its_body() // body contains "sk-proj-****abcd" → 502, code upstream_auth_failed, no "sk-proj"
#[actix_web::test] async fn upstream_404_is_passed_through()    // same status and body
#[actix_web::test] async fn upstream_500_without_fallback_becomes_502() // code upstream_error; body hidden
#[actix_web::test] async fn oversized_body_returns_413()         // MAX_REQUEST_BYTES=1024 → 413, code payload_too_large; mock expect(0)
#[actix_web::test] async fn accepts_body_larger_than_actix_default() // a 300 KiB message → 200 and forwarded
#[actix_web::test] async fn bad_bodies_return_400()             // "not json", {"messages": []}, {"model": 5} → 400 invalid_request_error
#[actix_web::test] async fn unknown_prefix_without_default_returns_400()
#[actix_web::test] async fn unprefixed_model_goes_to_the_default()   // DEFAULT_UPSTREAM=primary, model "m1" → upstream got "m1"
```

- [ ] **Step 2: Run** `cargo test --test chat`. Expected: compile failure (`proxy::chat` missing).
- [ ] **Step 3: Implement** the handler and the `AppState` change, then delete the old modules.
- [ ] **Step 4: Run** `cargo test --test chat` (expected: 14 passed), then the three global commands. Remove every dependency cargo reports as unused.
- [ ] **Step 5: Commit** the files above, using `git rm -r` for the deleted modules, with the message `feat: forward chat completions to OpenAI-compatible upstreams`.

### Task 11: Fallback

**Files:**
- Modify: `src/proxy/chat.rs`
- Create: `tests/fallback.rs`

**Interfaces:**
- Consumes: `should_fallback` and `describe_failure` (Task 8); `attempt` and `success_response` (Task 10); `Upstream::fallback` (Task 7).
- Produces: the response headers `x-gateway-fallback: <upstream>/<model>`, set on every response produced by the fallback attempt, whether it succeeded or failed.

**After a primary failure:**
1. Fall back only if `should_fallback` holds and the upstream has a `fallback`.
2. Log `warn!(key_id, primary, error = describe_failure(..), fallback = %target, "primary upstream failed; using fallback")`.
3. Send `upstream_body(&request, &target.model)` to the target upstream. Its own `fallback` is never consulted.
4. On success, call `success_response`, which records stats under `target.upstream/…`.
5. On failure, log both errors and return `failure_response(target.upstream, ..)`.
6. Both cases set `x-gateway-upstream` to the target and also `x-gateway-fallback`.

Streaming requests follow the same path. All of this happens before any byte is sent to the client.

- [ ] **Step 1: Write the failing tests** in `tests/fallback.rs`.
  - Setup: `PRIMARY_FALLBACK=backup/fb-model`.
  - Each test asserts both the outcome and the `received_requests` on each mock.

```rust
#[actix_web::test] async fn falls_back_on_500()        // 200 from backup; x-gateway-fallback: backup/fb-model; backup got model "fb-model"
#[actix_web::test] async fn falls_back_on_429()
#[actix_web::test] async fn falls_back_when_primary_refuses_connections() // PRIMARY_BASE_URL = closed_port_url()
#[actix_web::test] async fn falls_back_on_timeout()    // primary delays 2 s; config.upstream_timeout = 300 ms
#[actix_web::test] async fn no_fallback_on_400_or_404() // client gets the primary's status; backup expect(0)
#[actix_web::test] async fn no_fallback_on_401()        // 502 upstream_auth_failed; backup expect(0)
#[actix_web::test] async fn streaming_falls_back_before_the_first_byte() // primary 500, backup streams sse_stream() → client gets the backup's events
#[actix_web::test] async fn fallback_is_one_hop()       // backup has BACKUP_FALLBACK=third/x; primary and backup both 500 → 502 from backup,
                                                        // x-gateway-fallback set, third expect(0)
#[actix_web::test] async fn stats_record_the_fallback_model() // models_used has "backup/fb-model"
```

- [ ] **Step 2: Run** `cargo test --test fallback`. Expected: the fallback assertions fail, because the primary's errors are returned directly.
- [ ] **Step 3: Implement** the fallback branch.
- [ ] **Step 4: Run** `cargo test --test fallback` (expected: 9 passed), then the global commands.
- [ ] **Step 5: Commit** with the message `feat: fall back to a configured upstream on outages`.

### Task 12: `GET /v1/models`

**Files:**
- Create: `src/proxy/models.rs`, `tests/models.rs`
- Modify: `src/app.rs` (add `.route("/models", web::get().to(list_models))` inside the `/v1` scope), `src/proxy/mod.rs`

**Interfaces (produces):**

```rust
pub struct ModelEntry { pub id: String, pub object: &'static str /* "model" */, pub created: u64, pub owned_by: String } // Serialize
pub fn prefix_models(upstream: &str, body: &[u8]) -> Result<Vec<ModelEntry>, serde_json::Error>;
    // parses {"data":[{"id", "created"?}]}; id → "{upstream}/{id}"; created = the upstream's integer, or 0; owned_by = upstream
pub async fn list_models(state: web::Data<AppState>) -> HttpResponse; // {"object":"list","data":[…]}, always 200
```

- **Fetching:** `futures::future::join_all` runs one `GET models_url` per upstream, with a bearer token only if the upstream has a key.
- **Per-upstream timeout (my call):** `.timeout(config.connect_timeout)`, so one hanging upstream can't hold the list for `UPSTREAM_TIMEOUT_SECS`. The README documents this.
- **Failures:** a failed upstream is logged with `warn!` and skipped.

- [ ] **Step 1: Write the failing tests.** Unit tests go in `models.rs`, integration tests in `tests/models.rs`.

```rust
#[test] fn prefix_models_prefixes_ids_and_sets_owner() // {"data":[{"id":"gemma4:e2b","created":1730000000}]} for "ollama" →
                                                       // id "ollama/gemma4:e2b", created 1730000000, owned_by "ollama"
#[test] fn prefix_models_defaults_created_to_zero()   // missing or non-integer created → 0
#[test] fn prefix_models_rejects_a_non_list()
#[actix_web::test] async fn lists_models_from_every_upstream() // two mocks → four ids, in UPSTREAMS order
#[actix_web::test] async fn skips_a_down_upstream_and_returns_200() // BACKUP_BASE_URL = closed_port_url()
#[actix_web::test] async fn sends_the_upstream_key()
#[actix_web::test] async fn requires_a_gateway_key()   // 401
```

- [ ] **Step 2: Run** `cargo test --lib models` and `cargo test --test models`. Expected: compile failure.
- [ ] **Step 3: Implement** `src/proxy/models.rs` and add the route.
- [ ] **Step 4: Run** both test commands (expected: 7 passed), then the global commands.
- [ ] **Step 5: Commit** with the message `feat: list models from every upstream`.
- [ ] **Step 6: Checkpoint for Step 2.** Explain Tasks 6–12, then give the owner the exact `.env` edits:
  - add `UPSTREAMS=ollama,openai`;
  - `OLLAMA_BASE_URL=http://localhost:11434/v1`;
  - `OPENAI_BASE_URL=https://api.openai.com/v1`;
  - add `OLLAMA_FALLBACK=openai/gpt-6-terra`;
  - add `DEFAULT_UPSTREAM=ollama` (optional);
  - delete `AI_PROVIDER`.

  Wait for them to say continue.

---

## Step 3: Stats and limits

### Task 13: Rate limiting with `Retry-After`

**Files:**
- Create: `src/rate_limit.rs`, `tests/rate_limit.rs`
- Modify:
  - `src/config/mod.rs`: add `pub rate_limit_rpm: u32` (`RATE_LIMIT_RPM`, default 60, greater than 0), and include it in `summary_lines`.
  - `src/app.rs`: `AppState.limiter` becomes a `RateLimiter` built in `new` from the config; `.wrap(from_fn(enforce_rate_limit))` replaces `RateLimitMiddleware`.
  - `src/middleware/mod.rs`, `src/lib.rs`.
- Delete: `src/middleware/rate_limit.rs`

**Interfaces (produces):**

```rust
pub struct TokenBucket { /* units: u128, last: Instant, rpm: u32 */ }
impl TokenBucket {
    pub fn new(rpm: u32, now: Instant) -> Self;                      // starts full: rpm tokens
    pub fn try_take(&mut self, now: Instant) -> Result<(), Duration>; // Err = wait until one token is available
}
pub struct RateLimiter { /* rpm, Mutex<HashMap<KeyId, TokenBucket>> */ }
impl RateLimiter {
    pub fn new(rpm: u32) -> Self;
    pub fn check(&self, key: &KeyId, now: Instant) -> Result<(), u64>; // Err = Retry-After seconds
}
pub fn retry_after_secs(wait: Duration) -> u64; // round up, at least 1
pub async fn enforce_rate_limit<B: MessageBody>(state: web::Data<AppState>, req: ServiceRequest, next: Next<B>)
    -> Result<ServiceResponse<EitherBody<B>>, actix_web::Error>; // over the limit → Ok(429 ApiError::rate_limited) response;
                                                                 // no Caller → 401 (fails closed)
```

**Arithmetic is integer:**
- One token = 60_000_000_000 units (u128).
- Refill: `elapsed_nanos × rpm` units, capped at `rpm × 60_000_000_000`.
- Use `now.saturating_duration_since(last)`.
- Wait: `ceil((one_token − units) / rpm)` nanoseconds.

Floats get the boundary wrong. At rpm = 2, 15 s after draining, `15 × (2/60)` is 0.49999999999999994, which gives a wait of 16 s instead of 15.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn a_full_bucket_allows_rpm_requests_then_rejects() {
    let t0 = Instant::now();
    let mut bucket = TokenBucket::new(60, t0);
    for _ in 0..60 { assert!(bucket.try_take(t0).is_ok()); }
    assert_eq!(bucket.try_take(t0), Err(Duration::from_secs(1)));
}
#[test] fn the_bucket_refills_with_injected_time() // rpm 2: two Ok; then Err(30 s); at t0+15 s Err(15 s); at t0+30 s Ok
#[test] fn refill_is_capped_at_capacity()          // rpm 3, drained, t0+1 h → three Ok, then Err
#[test] fn retry_after_rounds_up_and_is_at_least_one() // 1 ms → 1; 1.5 s → 2; 30 s → 30; ZERO → 1
#[test] fn keys_have_separate_buckets()            // rpm 1: a Ok, a Err(60), b Ok
#[test] fn concurrent_requests_never_exceed_capacity() // rpm 10; 8 threads × 5 checks at the same `now` → exactly 10 Ok
// tests/rate_limit.rs
#[actix_web::test] async fn over_the_limit_gets_429_with_retry_after() // RATE_LIMIT_RPM=2: two GET /v1/stats → 200;
                                                                       // the third → 429, retry-after "30", code rate_limit_exceeded
#[actix_web::test] async fn health_is_not_rate_limited() // RATE_LIMIT_RPM=1: five GET /health → 200
#[actix_web::test] async fn admin_keys_are_limited_too()
```

- [ ] **Step 2: Run** `cargo test --lib rate_limit` and `cargo test --test rate_limit`. Expected: compile failure.
- [ ] **Step 3: Implement** `src/rate_limit.rs`, rewire `app.rs` and delete the old module.
- [ ] **Step 4: Run** both test commands (expected: 9 passed), then the global commands.
- [ ] **Step 5: Commit** with the message `feat: configurable per-key rate limit with Retry-After`.

### Task 14: Stats store and `/v1/stats`

**Files:**
- Create: `src/stats/mod.rs`, `src/stats/store.rs`, `src/stats/http.rs`, `tests/stats.rs`
- Modify:
  - `src/config/mod.rs`: add `pub stats_path: PathBuf` (`STATS_PATH`, default `stats.json`) and `pub stats_flush_interval: Duration` (`STATS_FLUSH_SECS`, default 60, greater than 0); include both in `summary_lines`.
  - `src/app.rs`: `stats: Arc<StatsStore>`; wire in the `track_request` middleware and `get_stats`.
  - `src/main.rs`:
    1. Load stats with `StatsStore::load`; on `Err`, `error!` and exit 1.
    2. Log the `LoadOutcome`.
    3. `spawn_flusher`, then run the server.
    4. Abort the flusher.
    5. Do a final `save`.
  - `src/proxy/chat.rs`: `state.stats.record_completion`.
  - `src/lib.rs`, `tests/common/mod.rs` (`state()` uses `StatsStore::default()`).
- Delete: `src/tracking/`, `src/middleware/`, `src/handlers/`

**Interfaces (produces):**

```rust
pub struct KeyStats { pub request_count: u64, pub error_count: u64, pub total_latency_ms: u64,
    pub total_prompt_tokens: u64, pub total_completion_tokens: u64, pub models_used: BTreeMap<String, u64>,
    pub last_request_timestamp: u64 /* unix ms; same field name and unit as today's file */ }
    // Serialize, Deserialize, Clone, Debug, Default, PartialEq; #[serde(default)] on the struct
pub struct StatsStore { /* Mutex<BTreeMap<KeyId, KeyStats>>, plus save_lock: Mutex<()> */ } // Default
pub enum LoadOutcome { Missing, Loaded { keys: usize, rekeyed: usize }, Quarantined { moved_to: PathBuf, reason: String } }
pub struct StatsLoadError { /* path, reason, io::Error */ }  // thiserror: the file is unusable and could not be moved aside
impl StatsStore {
    pub fn load(path: &Path) -> Result<(StatsStore, LoadOutcome), StatsLoadError>;
    pub fn save(&self, path: &Path) -> std::io::Result<()>;   // snapshot, then write "<path>.tmp" + sync_all + rename;
                                                              // save_lock serializes saves
    pub fn record_request(&self, key: &KeyId, latency: Duration, is_error: bool);
    pub fn record_completion(&self, key: &KeyId, model_label: &str, usage: Option<Usage>); // always counts the model
    pub fn get(&self, key: &KeyId) -> Option<KeyStats>;
    pub fn all(&self) -> Vec<(KeyId, KeyStats)>;              // sorted by key ID
}
pub fn spawn_flusher(stats: Arc<StatsStore>, path: PathBuf, every: Duration) -> actix_web::rt::task::JoinHandle<()>;
    // first save after one interval; saves run in actix_web::rt::task::spawn_blocking; errors are logged
pub async fn track_request<B: MessageBody>(state: web::Data<AppState>, req: ServiceRequest, next: Next<B>)
    -> Result<ServiceResponse<B>, actix_web::Error>;          // latency to headers; error = 5xx
pub async fn get_stats(state: web::Data<AppState>, caller: Caller, query: web::Query<StatsQuery>) -> Result<HttpResponse, ApiError>;
pub struct StatsQuery { pub key_id: Option<String> }
```

**File format:** `{"stats": {"<key id>": KeyStats}}`, the same as today.

**On load:**
- Keys that aren't valid key IDs are re-keyed with `KeyId::from_raw_key` and merged: counts and maps are summed, and the larger timestamp is kept.
- An unparsable or unreadable file is renamed to `<path>.corrupt-<unix secs>`. If that rename fails, return `StatsLoadError`.

**`/v1/stats` responses:**
- A user gets their own stats, or zeros, and `?key_id=` is ignored.
- An admin gets everyone's stats as an array. With `?key_id=` they get one object.
  - A malformed ID gets a 400.
  - An unknown one gets a 404 `not_found`.
- Fields: `key_id`, `request_count`, `error_count`, `total_latency_ms`, `avg_latency_ms`, `total_prompt_tokens`, `total_completion_tokens`, `last_request_timestamp`, `models_used`.

- [ ] **Step 1: Write the failing tests.** Unit tests go in `store.rs` and use `tempfile::tempdir()`.

```rust
#[test] fn records_requests_and_completions()
#[test] fn save_then_load_round_trips()
#[test] fn save_is_atomic_and_leaves_no_tmp_file()        // afterwards, only stats.json is in the directory
#[test] fn a_missing_file_starts_empty()                 // LoadOutcome::Missing
#[test] fn a_corrupt_file_is_moved_aside() // "{not json" → Quarantined; a "stats.json.corrupt-<digits>" file exists; the store is empty
#[test] fn todays_file_format_loads()      // fixture in the current format (models_used map, millisecond timestamp)
#[test] fn raw_key_entries_are_rekeyed()   // {"stats":{"sk-raw-secret":{"request_count":2,…}}} → entry under from_raw_key("sk-raw-secret");
                                           // after save, the file text lacks "sk-raw-secret"
#[test] fn a_rekeyed_entry_merges_with_its_id()
#[actix_web::test] async fn flusher_saves_on_its_interval() // every 50 ms; poll up to 2 s until the file holds the recorded request
// tests/stats.rs
#[actix_web::test] async fn users_see_only_their_own_stats() // USER_KEY with ?key_id=<admin id> → own key_id
#[actix_web::test] async fn admins_see_everyone_and_can_filter() // array holds both IDs; ?key_id=<user id> → one object
#[actix_web::test] async fn unknown_key_id_is_404_and_malformed_is_400()
#[actix_web::test] async fn requests_and_server_errors_are_counted() // one 200 chat and one upstream 500 (→ 502) → request_count 2, error_count 1
#[actix_web::test] async fn rejected_requests_are_not_counted()    // RATE_LIMIT_RPM=1: user's 2nd call → 429; admin view: user request_count 1
```

- [ ] **Step 2: Run** `cargo test --lib stats` and `cargo test --test stats`. Expected: compile failure.
- [ ] **Step 3: Implement** the stats module, rewire `app.rs`, `main.rs` and `chat.rs`, and delete the old modules.
- [ ] **Step 4: Run** both test commands (expected: 14 passed), then the global commands.
- [ ] **Step 5: Commit** with the message `feat: stats by key ID with atomic periodic saves`.

### Task 15: No-leak test

**Files:**
- Create: `tests/no_leaks.rs`
- Modify: `tests/common/mod.rs`. Add `capture_logs() -> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<u8>>>)`: a `tracing_subscriber::fmt` subscriber at DEBUG level, writing to the shared buffer, installed with `set_default`. That's thread-local, and `actix_web::test` runs on one thread.

**Interfaces:**
- Consumes: everything above.
- Produces: a test that fails if a secret appears anywhere.

- [ ] **Step 1: Write the test** `no_key_reaches_logs_stats_or_responses`.
  1. Start capturing logs.
  2. Set up a primary upstream with `PRIMARY_API_KEY = UPSTREAM_KEY` and `PRIMARY_FALLBACK=backup/fb`.
  3. Mock the primary so it returns 500 with a body that contains `UPSTREAM_KEY`. The backup returns 401 once, then 200.
  4. Send these requests:
     - a chat with `USER_KEY`, which goes primary 500 → fallback 401;
     - a chat with `USER_KEY`, which goes primary 500 → fallback 200;
     - a request with the wrong key `"sk-wrong-key-0123456789"`;
     - a user stats request;
     - an admin stats request;
     - `/v1/models`.
  5. Call `state.stats.save(tempdir/stats.json)`.
  6. Assert that none of `USER_KEY`, `ADMIN_KEY`, `UPSTREAM_KEY` and `"sk-wrong-key-0123456789"` appear in:
     - the captured logs;
     - the saved file;
     - any response body;
     - any response header value;
     - `format!("{:?}", config)`.
- [ ] **Step 2: Run** `cargo test --test no_leaks`. Expected: pass. If it fails, the leak is a bug: fix it in the module that owns it, and keep the test strict.
- [ ] **Step 3: Commit** with the message `test: prove keys never reach logs, stats or responses`.
- [ ] **Step 4: Checkpoint for Step 3.** Explain Tasks 13–15 to the owner, then wait for them to say continue.

---

## Step 4: Ship

### Task 16: Docker image

**Files:**
- Create: `Dockerfile`, `.dockerignore`

**Interfaces:**
- Consumes: the binary's environment variables (Tasks 4, 7, 13, 14).
- Produces: the image `ai-gateway:dev` for the README.

- [ ] **Step 1: Verify the tags.** Run `docker manifest inspect rust:1.100-slim-trixie` and `docker manifest inspect gcr.io/distroless/cc-debian13:nonroot`.
  - Both must exist. Trixie is Debian 13 in both stages, so the binary's glibc matches.
  - If `1.100` is missing, use the newest `1.x-slim-trixie` tag that exists.
- [ ] **Step 2: Write the files.**

```dockerfile
# syntax=docker/dockerfile:1
FROM rust:1.100-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/ai-gateway /ai-gateway
RUN mkdir /data

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /ai-gateway /usr/local/bin/ai-gateway
# The nonroot user (65532) must own /data, or a named volume starts root-owned and every save fails
COPY --from=build --chown=65532:65532 /data /data
ENV HOST=0.0.0.0 PORT=8080 STATS_PATH=/data/stats.json
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/ai-gateway"]
```

`.dockerignore`: `target/`, `.git/`, `.env`, `.env.*`, `!.env.example`, `stats.json*`, `docs/`.

- [ ] **Step 3: Build.** Run `docker build -t ai-gateway:dev .`. Expected: success.
- [ ] **Step 4: Check that stats survive `docker stop`.**
  1. Start the container: `docker run -d --name ai-gateway-smoke -p 18080:8080 -v ai-gateway-smoke:/data -e GATEWAY_API_KEYS=$KEY -e UPSTREAMS=ollama -e OLLAMA_BASE_URL=http://host.docker.internal:11434/v1 ai-gateway:dev`. `KEY` comes from `openssl rand -hex 32`.
  2. Call `curl -fsS localhost:18080/health` and then `curl -fsS -H "Authorization: Bearer $KEY" localhost:18080/v1/stats`.
  3. Stop it: `docker stop ai-gateway-smoke`.
  4. Copy out the file: `docker cp ai-gateway-smoke:/data/stats.json "$SCRATCHPAD/docker-stats.json"`.

  Expected: the file exists, contains `"key_`, and doesn't contain `$KEY`. Then clean up with `docker rm ai-gateway-smoke && docker volume rm ai-gateway-smoke`.
- [ ] **Step 5: Commit** with the message `feat: add Docker image`.

### Task 17: README, `.env.example` and `Cargo.toml` metadata

**Files:**
- Modify: `README.md` (full rewrite), `.env.example` (full rewrite), `Cargo.toml`

**Interfaces:**
- Consumes: the behavior shipped in Tasks 1–16.
- Produces: user-facing documentation.

- [ ] **Step 1: Update `Cargo.toml`.**
  - Set `version = "0.2.0"` (my call: the configuration is not backward compatible).
  - Add a `description` that matches the README's first paragraph.
  - Add `license = "Apache-2.0"` and `repository = "https://github.com/LuisMongeB/ai-gateway"`.
- [ ] **Step 2: Write `.env.example`.** It has every variable from the spec's table, in the same order, each with a one-line comment.
  - `GATEWAY_API_KEYS=` is left empty, with the comment `# required: generate with openssl rand -hex 32`, so a copied template fails fast instead of running with a public key.
  - `UPSTREAMS=ollama,openai` and `DEFAULT_UPSTREAM=ollama`.
  - `OLLAMA_BASE_URL=http://localhost:11434/v1` and `OLLAMA_FALLBACK=openai/gpt-6-terra`.
  - `OPENAI_BASE_URL=https://api.openai.com/v1` and `OPENAI_API_KEY=sk-your-key-here`.
- [ ] **Step 3: Write `README.md`.** It covers every section the spec's "README and metadata" list names, plus these:
  - a CI badge (`https://github.com/LuisMongeB/ai-gateway/actions/workflows/ci.yml/badge.svg`);
  - the `x-gateway-upstream` and `x-gateway-fallback` headers;
  - a table of error codes, taken from Task 2's table;
  - how to compute a key ID: `printf %s "$KEY" | shasum -a 256 | cut -c1-12`, prefixed with `key_`;
  - the `/v1/models` timeout;
  - Docker usage with `host.docker.internal`;
  - a Kubernetes `httpGet` probe on `/health`, noting that the image has no shell or curl;
  - an "Upgrading from 0.1" section with the `.env` edits from the Step 2 checkpoint.

  Limitations to list:
  - stats and rate limits are per process;
  - changing keys needs a restart;
  - tokens aren't counted when an upstream ignores `stream_options`;
  - a stream the client abandons isn't counted for tokens;
  - non-streaming generations longer than `UPSTREAM_TIMEOUT_SECS` time out;
  - no retries, caching, embeddings or Azure yet.

  The roadmap marks auth, rate limits and fallback as done.
- [ ] **Step 4: Verify.**
  - `grep -niE 'high-performance|production-ready|low latency|yourusername|AI_PROVIDER=' README.md .env.example` prints nothing.
  - Every variable is documented: `for v in HOST PORT LOG_FORMAT RUST_LOG GATEWAY_API_KEYS ADMIN_API_KEYS RATE_LIMIT_RPM MAX_REQUEST_BYTES UPSTREAMS DEFAULT_UPSTREAM _BASE_URL _API_KEY _FALLBACK UPSTREAM_CONNECT_TIMEOUT_SECS UPSTREAM_TIMEOUT_SECS STATS_PATH STATS_FLUSH_SECS; do grep -q "$v" README.md || echo "missing $v"; done` prints nothing.
  - Every curl example sends `Authorization` except the one for `/health`.
- [ ] **Step 5: Commit** with the message `docs: rewrite README and .env.example for the router`.

### Task 18: Final verification

**Files:**
- Modify: only what a failed check requires.

**Interfaces:**
- Consumes: the whole branch.

- [ ] **Step 1: Run the global commands.** Expected: all three exit 0.
- [ ] **Step 2: Measure coverage.**
  1. Install the tooling, telling the owner first: `rustup component add llvm-tools-preview && cargo install cargo-llvm-cov --locked`.
  2. Run `cargo llvm-cov --summary-only` and record the total line coverage for the owner.
  3. If it's under 80%, add tests to the least-covered library module first. `main.rs` is excluded from that judgment.
- [ ] **Step 3: Audit dependencies.** Run `cargo install cargo-audit --locked && cargo audit`. Expected: no vulnerabilities. List any warnings for the owner.
- [ ] **Step 4: Smoke-test against the owner's Ollama.** Run from the scratchpad, never the repo, with `env -i PATH="$PATH"` plus explicit variables:
  - `UPSTREAMS=ollama,dead`
  - `OLLAMA_BASE_URL=http://localhost:11434/v1`
  - `DEAD_BASE_URL=http://127.0.0.1:9/v1` and `DEAD_FALLBACK=ollama/gemma4:e2b`
  - a fresh `GATEWAY_API_KEYS` and `ADMIN_API_KEYS`
  - `STATS_PATH=$SCRATCHPAD/stats.json`

  Then check each of these with curl:
  1. `/health`;
  2. `/v1/models`, which lists `ollama/…`;
  3. a non-streaming `ollama/gemma4:e2b` request with `max_tokens` 5;
  4. a streaming request, which ends in `data: [DONE]` and contains no `choices: []` usage chunk;
  5. a `dead/x` request, which answers through the fallback with an `x-gateway-fallback` header;
  6. `/v1/stats` as admin, which shows tokens and `ollama/gemma4:e2b`;
  7. Ctrl-C, after which `stats.json` is saved.
- [ ] **Step 5: Review the whole branch.** Do this as the chosen execution skill describes.
- [ ] **Step 6: Checkpoint for Step 4.**
  1. Explain Tasks 16–18 and report the coverage, audit and smoke-test results.
  2. Ask whether to push and open a pull request.
  3. Propose `gh repo edit` with:
     - description: "Self-hosted OpenAI-compatible gateway: API keys, per-key rate limits, usage stats and fallback for Ollama, OpenAI, vLLM and other OpenAI-compatible backends"
     - topics: `rust`, `actix-web`, `llm`, `openai`, `ollama`, `ai-gateway`, `llm-proxy`
  4. Run none of it until the owner says so.
  5. Remind them that re-pinning the repo is their action, and that their untracked `CONTEXT.md` now describes the old layout.
