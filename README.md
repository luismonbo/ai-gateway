# ai-gateway

[![CI](https://github.com/LuisMongeB/ai-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/LuisMongeB/ai-gateway/actions/workflows/ci.yml)

An OpenAI-compatible gateway that routes requests to Ollama, OpenAI, Azure OpenAI and other OpenAI-style backends by model prefix, with API keys, keyless Azure sign-in, rate limits, fallback and usage stats. Clients talk to one endpoint and pick a backend with a prefix such as `ollama/gemma4:e2b` or `openai/gpt-6-terra`. Requests are forwarded with only the model name rewritten (streaming requests also get usage reporting switched on), and answers come back byte for byte, so tools, images, JSON mode, sampling parameters and streaming work wherever the backend supports them. It is written in Rust with actix-web and runs as a single binary or a small Docker image.

## Quick start

### With cargo

```bash
cp .env.example .env
openssl rand -hex 32          # put the output in .env as GATEWAY_API_KEYS=…
cargo run --release
```

At startup the gateway reads the first `.env` it finds in the current directory or one of its parents; variables already set in the environment win. The examples below send the key as `$GATEWAY_KEY`, so set that first (`export GATEWAY_KEY=…`, the same key). Check that the gateway is up, then send a request:

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
- The image runs as the non-root user 65532 and keeps stats in `/data`. A named volume starts out owned by that user. For a bind mount (`-v ./data:/data`), make the directory writable by it first (`sudo chown 65532:65532 ./data`). If `STATS_PATH` can't be written, the gateway refuses to start and says why.
- `docker stop` kills the container after 10 s, but the gateway waits up to 30 s for requests still in flight. If streams may be running when you stop it, allow more time (`docker stop -t 35`, Compose `stop_grace_period: 35s`, Kubernetes `terminationGracePeriodSeconds: 35`), or the final stats save is cut off.
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
| `<NAME>_AUTH` | none | `managed_identity` or `azure_cli`: sign in to Azure instead of sending a key (see [Azure OpenAI](#azure-openai)). Unset keeps `<NAME>_API_KEY` if set, otherwise no auth. |
| `<NAME>_CLIENT_ID` | none | Client ID of a user-assigned managed identity. Unset means the system-assigned identity. |
| `<NAME>_TOKEN_SCOPE` | `https://ai.azure.com/.default` | The scope Azure tokens are requested for. |
| `<NAME>_MODELS` | none | Comma-separated models that `GET /v1/models` lists for this upstream, such as your Azure deployment names. When set, the upstream isn't asked for its list. Works for any upstream. |
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
| Azure OpenAI | `https://<resource>.openai.azure.com/openai/v1` |
| Groq | `https://api.groq.com/openai/v1` |
| vLLM | `http://localhost:8000/v1` |

The gateway checks the configuration before it starts, and the first broken rule stops startup with a message naming the variable:

- Upstream names use only lowercase letters, digits and `_`, and each appears once.
- Every upstream needs a base URL that starts with `http://` or `https://`. Credentials, query strings and fragments are rejected; put the key in `<NAME>_API_KEY`. A URL without a path (such as `https://api.openai.com`) starts with a warning.
- `DEFAULT_UPSTREAM` and every fallback target must name a configured upstream.
- `<NAME>_AUTH` is `managed_identity` or `azure_cli`, in any letter case, and can't be set together with `<NAME>_API_KEY`.
- `<NAME>_CLIENT_ID` must be a GUID. With `azure_cli`, or without `<NAME>_AUTH`, it's ignored with a warning, because a shell may export `AZURE_CLIENT_ID` for other tools.
- `<NAME>_TOKEN_SCOPE` must be an `https://` URL. Without `<NAME>_AUTH` it's ignored with a warning.
- With `<NAME>_AUTH`, the base URL must start with `https://`, so tokens never travel in clear text; `http://` is accepted only for `localhost` and loopback addresses. A host outside `*.openai.azure.com`, `*.services.ai.azure.com` and `*.cognitiveservices.azure.com`, such as an API Management proxy, gets a warning that tokens will be sent there.
- A model listed twice in `<NAME>_MODELS` is an error. An entry that already starts with `<name>/` gets a warning, because the gateway adds the prefix itself.
- A managed identity the Azure libraries can't use here stops startup with their reason: on Azure Arc, Service Fabric, Cloud Shell or Azure ML, or AKS workload identity without `AZURE_TENANT_ID`.
- At least one gateway key is required, and a key can't be both a user key and an admin key. Keys shorter than 32 characters get a warning.
- Numbers must be whole and greater than 0, and `PORT` must fit in 16 bits.

The startup log lists the bind address, each upstream with its base URL, how it authenticates, its fallback and, with `<NAME>_MODELS`, how many models it lists, then the default upstream, the limits and the number of keys. It never shows a key.

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

- **What triggers it:** connection errors, timeouts, upstream 5xx and upstream 429. Other 4xx responses never do, and neither does a failed Azure sign-in: they're the client's or the operator's mistake, and retrying elsewhere would hide them and could cost money.
- **One hop:** a fallback's own fallback is never followed.
- **Streaming:** streaming requests fall back too, because the switch happens before the first byte reaches the client.
- **The request:** the same body, with `model` replaced by the fallback model.
- **How you can tell:** the response carries `x-gateway-fallback: openai/gpt-6-terra`, its `model` field names the model that answered, and the log has a warning with the key ID, the primary's error and the fallback target. If the fallback fails too, the client gets the fallback's error and the log has both.

## Azure OpenAI

Azure OpenAI's v1 API is OpenAI-compatible, so Azure is an upstream like any other. Its base URL is the resource's endpoint plus `/openai/v1`: `https://<resource>.openai.azure.com/openai/v1`, or `https://<resource>.services.ai.azure.com/openai/v1` for an Azure AI Foundry resource. The model is your deployment name: with `UPSTREAMS=azure`, `azure/my-gpt4o` reaches Azure as `my-gpt4o`.

The gateway can sign in to Azure itself, so no key goes into the configuration. Set `<NAME>_AUTH` to one of two methods.

**`azure_cli`** signs in as whoever ran `az login` on the machine (Azure CLI 2.54 or later). On a laptop, for example:

```ini
UPSTREAMS=azure
AZURE_BASE_URL=https://my-resource.openai.azure.com/openai/v1
AZURE_AUTH=azure_cli
AZURE_MODELS=my-gpt4o,my-gpt5-mini   # your deployment names, for /v1/models
```

If your account is in several tenants, sign in to the resource's tenant: `az login --tenant <tenant ID>`. The Docker image has no Azure CLI, so `azure_cli` works only outside it, for example with `cargo run`.

**`managed_identity`** signs in as the Azure resource the gateway runs on. The gateway works out the platform from what Azure sets in its environment:

- **VMs and scale sets, App Service, Functions and Container Apps:** the system-assigned identity, or a user-assigned one with `<NAME>_CLIENT_ID=<its client ID>`.
- **AKS with workload identity:** when the pod is set up for it (`AZURE_FEDERATED_TOKEN_FILE` is set), the gateway signs in as the pod's identity, using the `AZURE_CLIENT_ID` the webhook sets unless `<NAME>_CLIENT_ID` names another.

Before the first request:

- **The role:** give the identity (or, with `azure_cli`, your account) the **Cognitive Services OpenAI User** role on the resource. A new assignment can take up to 5 minutes to apply; until then requests get 403.

  ```bash
  az role assignment create --role "Cognitive Services OpenAI User" \
    --assignee <principal ID or user name> --scope <resource ID>
  ```

- **The custom subdomain:** the base URL must use the resource's custom subdomain (`<resource>.openai.azure.com`). Regional endpoints such as `https://eastus.api.cognitive.microsoft.com` answer 401 to tokens.

How it behaves:

- **Startup:** the gateway signs in once per Azure upstream and logs, for example, `upstream azure: signed in with azure cli; token valid for 74 min`. If it can't, it logs a warning with the reason and starts anyway; that upstream's requests fail until it can sign in.
- **Tokens:** each upstream keeps its token in memory and fetches a new one when 5 minutes or less are left, so about once per token lifetime a request waits for sign-in: about a second with `azure_cli`, milliseconds in Azure. A fetch is bounded by `UPSTREAM_CONNECT_TIMEOUT_SECS`. Tokens never appear in logs, stats or responses.
- **Failures:** a request that can't get a token gets a 502 `upstream_auth_failed` and never falls back; the reason is logged with the key ID.
- **`/v1/models`:** Azure's own list holds the models the resource could deploy, not your deployments, and most of those fail as `model`. Set `<NAME>_MODELS` to your deployment names, and `/v1/models` lists exactly those without asking Azure.
- **Stats:** they record the model Azure reports, such as `azure/gpt-4o-2024-11-20`, not the deployment name, so two deployments of the same model share a label.
- **Token scope:** tokens are requested for `https://ai.azure.com/.default`. Older setups and other clouds may need another, set with `<NAME>_TOKEN_SCOPE`, such as `https://cognitiveservices.azure.com/.default`.
- **API keys** work too: set `<NAME>_API_KEY` instead of `<NAME>_AUTH`, not both. Azure's v1 API accepts the key as a bearer token, as the OpenAI SDKs send it.

| What you see | What to check |
|---|---|
| 502 `upstream_auth_failed`, and the log says `returned HTTP 401` | The base URL must use the resource's custom subdomain. If it does, try `<NAME>_TOKEN_SCOPE=https://cognitiveservices.azure.com/.default`. |
| 502 `upstream_auth_failed`, and the log says `returned HTTP 403` | Give the identity the Cognitive Services OpenAI User role on the resource; a new assignment takes up to 5 minutes. Check that you signed in to the resource's tenant. |
| 404 from Azure | `model` must be a deployment name, not the name of the model behind it. |
| Startup warning `could not sign in with …` | The reason follows. With `azure_cli`: install the Azure CLI or run `az login`. With `managed_identity`: give the resource an identity, and check `<NAME>_CLIENT_ID`. |
| `no token within …; raise UPSTREAM_CONNECT_TIMEOUT_SECS if signing in is slow` | Signing in took longer than `UPSTREAM_CONNECT_TIMEOUT_SECS` (10 s by default). Raise it if `az` is slow on this machine. |

## Endpoints

### `GET /health`

Public: no key, no rate limit. Returns `200 ok`.

```bash
curl http://localhost:8080/health
```

### `POST /v1/chat/completions`

The OpenAI chat completions API. Only `model` is rewritten, and streaming requests may gain `stream_options` (see below). The other fields reach the upstream with the same values and in the same order (`"stream": null` is left out, which means the same). Only the upstream's own key or Azure token is sent to it, never the client's gateway key.

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
| `x-gateway-fallback` | `upstream/model` of the fallback, when the request went to the fallback, whether it succeeded or failed. |

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

Upstreams are asked at the same time, and each gets `UPSTREAM_CONNECT_TIMEOUT_SECS` (10 s by default) for its whole answer, so one hanging upstream can't hold up the list. An upstream that fails, or can't get an Azure token, is left out and logged; the response is still 200. An upstream with `<NAME>_MODELS` isn't asked at all: its configured names are listed, in the order written, with `created` set to 0.

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
| The gateway couldn't get an Azure token (no fallback) | 502, generic message | `server_error` | `upstream_auth_failed` |
| Any other upstream 4xx (400, 404, 413, 422…) | the same status | the upstream's own body, passed through | |

Bodies of upstream 401, 403, 429 and 5xx responses are never returned, because they can contain fragments of the operator's upstream key or organisation ID. They're logged, truncated to 512 bytes, with the key or token the request carried replaced by `[redacted]` and line breaks escaped so each stays on one log line. Other 4xx bodies reach the client because they explain the client's mistake, such as an unknown model or a context that's too long; if one ever contains the request's key or token, it's replaced by `[redacted]` first.

If an upstream fails in the middle of a stream, the status has already been sent, so the gateway logs the failure and aborts the connection instead of ending the stream as if the answer were complete.

## Stats

For each key ID the gateway records:

- requests and errors (responses with a 5xx status);
- latency to the response headers;
- prompt and completion tokens;
- which models answered, as `upstream/model`, so a fallback shows up as `openai/gpt-6-terra`. Labels are cut at 256 bytes, and each key keeps at most 100; answers from further models count under `upstream/(other)`.

At startup the file is loaded and saved straight away, so an unwritable `STATS_PATH` stops the gateway before it serves anything. After that, stats are saved every `STATS_FLUSH_SECS` and on shutdown, so a crash loses at most one interval. Each save writes `<path>.tmp`, syncs it to disk and renames it over the old file, so the file is never half-written. If the final save fails, the gateway exits with status 1.

- **Missing file:** the gateway starts empty.
- **Corrupt or unreadable file:** it's logged as an error and renamed to `<path>.corrupt-<unix time>`, and the gateway starts empty. Nothing is overwritten. If the file can't be renamed either, startup fails.
- **A directory:** startup fails, and the directory is left alone; point `STATS_PATH` at a file.
- **Files from 0.1:** entries stored under raw API keys are re-keyed to their key IDs and merged when the file loads, and the startup save removes the raw keys from the file.

## Logging

- **Library:** logs go through `tracing`, as text or as JSON lines (`LOG_FORMAT=json`), and are filtered with `RUST_LOG`.
- **Colours:** text logs use colour only when they go to a terminal and `NO_COLOR` isn't set.
- **Access log:** every request gets one line with target `access`, holding the method, the path, the status, the latency, the IP address of the TCP peer (your reverse proxy's, if one sits in front) and the user agent. The query string is never logged. Quiet the access log with `RUST_LOG=info,access=warn`.
- **Key IDs:** gateway log lines name keys by ID (`key_id` field). No raw key, gateway or upstream, and no Azure token is written to logs or stats, and the key or token a request carried is replaced by `[redacted]` in any upstream body the gateway logs or passes on.

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
- **Request bodies:** they now reach the upstream as sent, with only `model` rewritten. Ollama-only options that the old translation layer handled, such as a per-request `num_ctx`, are no longer available.
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
- An upstream that rejects unknown request fields refuses streaming requests that don't ask for usage, because the gateway adds `stream_options.include_usage` to them.
- HTTPS upstreams need a certificate from a public certificate authority. The gateway trusts Mozilla's bundled root certificates, not the system's, so a private CA isn't accepted yet.
- Request bodies aren't decompressed: a body sent with `Content-Encoding: gzip` gets a 400. Leaving decompression out means a small compressed body can't expand into a huge one.
- Only Azure's v1 API (`/openai/v1`) works; the classic `/openai/deployments/<name>/…?api-version=…` URLs don't.
- Managed identity on Azure Arc, Service Fabric, Cloud Shell and Azure ML isn't supported.
- Azure tokens aren't renewed in the background: about once per token lifetime, a request waits for sign-in.
- Sovereign clouds (Azure Government, Azure China) are untested. `<NAME>_TOKEN_SCOPE` should make them work.
- Managed identity hasn't been checked end to end in Azure yet: VMs, App Service, Container Apps and AKS were tested only against simulated Azure replies.
- There are no retries, response caching or embeddings endpoint yet.

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
| `src/sign_in/` | Azure sign-in: credentials, the token cache and the HTTP transport they share with upstream requests. |
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
| Azure OpenAI, with keyless sign-in | Done |
| Retries with backoff | Planned |
| Embeddings endpoint | Planned |
| Response caching | Planned |

---

Built by Luis Monge.
