# ai-gateway

[![CI](https://github.com/luismonbo/ai-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/luismonbo/ai-gateway/actions/workflows/ci.yml)

One OpenAI-compatible endpoint for every model you run. Clients pick a backend with a model prefix; the gateway handles keys, rate limits, fallback and usage tracking.

```text
any OpenAI client ──▶ ai-gateway ──┬─▶ ollama/…    Ollama
                                   ├─▶ openai/…    OpenAI
                                   ├─▶ azure/…     Azure OpenAI (keyless)
                                   └─▶ <name>/…    any OpenAI-compatible API
```

Requests are forwarded with only the model rewritten (streams also ask for usage), so tools, vision, structured outputs and streaming work wherever the backend supports them. Written in Rust on actix-web; ships as a single binary or a distroless Docker image.

## Features

- **Prefix routing** to any number of OpenAI-compatible upstreams, with an optional default for unprefixed models.
- **Streaming** relayed event by event, with token usage captured wherever the upstream reports it.
- **Fallback** to another upstream on connection errors, timeouts, 5xx and 429. The switch happens before the first byte reaches the client, so streams fall back too.
- **Keyless Azure sign-in** with managed identity (VMs, App Service, Container Apps, AKS workload identity) or the Azure CLI, built on Microsoft's `azure_identity`.
- **Gateway keys** with user and admin roles. Keys are hashed at startup; logs and stats only ever see a key ID.
- **Per-key rate limits** with `Retry-After`, so the OpenAI SDKs back off on their own.
- **Usage stats** per key: requests, errors, latency, tokens and models, saved atomically.
- **Secrets stay secret:** upstream keys and Azure tokens never reach logs, stats or clients, and upstream error bodies are scrubbed before they're logged or passed on.
- **OpenAI-style errors** and JSON logs (`LOG_FORMAT=json`) with a per-request access log.

## Quick start

```bash
cp .env.example .env   # set GATEWAY_API_KEYS, e.g. to the output of: openssl rand -hex 32
cargo run --release
```

Then call it with that key:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "ollama/gemma4:e2b", "messages": [{"role": "user", "content": "Hello!"}]}'
```

Any OpenAI SDK works the same way:

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8080/v1", api_key="<gateway key>")
client.chat.completions.create(
    model="openai/gpt-6-terra",
    messages=[{"role": "user", "content": "Hello!"}],
)
```

### Docker

```bash
docker build -t ai-gateway .
docker run --rm -p 8080:8080 -v ai-gateway-data:/data \
  -e GATEWAY_API_KEYS="$GATEWAY_KEY" \
  -e UPSTREAMS=ollama -e OLLAMA_BASE_URL=http://host.docker.internal:11434/v1 \
  ai-gateway
```

The image is distroless and runs as a non-root user. It keeps stats on the `/data` volume and answers `/health` for liveness and readiness probes.

## Configuration

Settings come from environment variables or a `.env` file. They're validated at startup: a bad value stops the gateway with a message naming the variable. [`.env.example`](.env.example) lists them all with their defaults.

| Variable | Default | |
|---|---|---|
| `GATEWAY_API_KEYS`, `ADMIN_API_KEYS` | | Comma-separated client keys. Admins can also read everyone's stats. At least one key is required. |
| `UPSTREAMS` | | Upstream names, e.g. `ollama,openai,azure`. |
| `<NAME>_BASE_URL` | | Base URL including the version path, e.g. `http://localhost:11434/v1`. |
| `<NAME>_API_KEY` | | Sent to the upstream as a bearer token. |
| `<NAME>_AUTH` | | `azure_cli` or `managed_identity`, to [sign in to Azure](#azure-openai) instead of using a key. |
| `<NAME>_FALLBACK` | | `upstream/model` to use when this upstream fails. |
| `<NAME>_MODELS` | | What `/v1/models` lists for this upstream, instead of asking it. |
| `DEFAULT_UPSTREAM` | | Where models without a known prefix go. |
| `RATE_LIMIT_RPM` | `60` | Requests per minute per key. |
| `UPSTREAM_CONNECT_TIMEOUT_SECS` | `10` | Connect timeout. It also bounds Azure sign-in and each upstream's `/v1/models` answer. |
| `UPSTREAM_TIMEOUT_SECS` | `300` | Longest silence while waiting for upstream data. |
| `MAX_REQUEST_BYTES` | 10 MiB | |
| `HOST`, `PORT` | `127.0.0.1`, `8080` | The Docker image binds `0.0.0.0`. |
| `LOG_FORMAT`, `RUST_LOG` | `text`, `info` | |
| `STATS_PATH`, `STATS_FLUSH_SECS` | `stats.json`, `60` | Where and how often stats are saved. |

An upstream's variables use its name in upper case: `ollama` reads `OLLAMA_BASE_URL`.

## Routing and fallback

The prefix picks the upstream and is stripped: with `UPSTREAMS=ollama,openai`, `ollama/gemma4:e2b` reaches Ollama as `gemma4:e2b`. A model without a known prefix goes to `DEFAULT_UPSTREAM` unchanged, or gets a 400 listing the valid prefixes.

`OLLAMA_FALLBACK=openai/gpt-6-terra` sends a request to OpenAI when Ollama is unreachable, times out, or answers with a 5xx or 429. Client errors never fall back. Chat responses name the upstream that answered in `x-gateway-upstream`, and fallbacks add `x-gateway-fallback`.

## Azure OpenAI

Azure OpenAI and Azure AI Foundry are upstreams like any other. The base URL is the resource's endpoint plus `/openai/v1`, and the model is your deployment name. The gateway signs in by itself, so no key goes into the configuration:

```ini
UPSTREAMS=azure
AZURE_BASE_URL=https://my-resource.openai.azure.com/openai/v1
AZURE_AUTH=azure_cli                # local development: uses your az login
# AZURE_AUTH=managed_identity       # in Azure: VMs, App Service, Container Apps, AKS
# AZURE_CLIENT_ID=<client ID>       # only for a user-assigned identity
AZURE_MODELS=my-gpt4o,my-gpt5-mini  # your deployments, listed by /v1/models
```

The identity (or your own account, with `azure_cli`) needs the **Cognitive Services OpenAI User** role on the resource. Tokens are cached and renewed shortly before they expire. `AZURE_TOKEN_SCOPE` overrides the default scope, `https://ai.azure.com/.default`. If you'd rather use a key, set `AZURE_API_KEY` instead.

## API

| Endpoint | |
|---|---|
| `POST /v1/chat/completions` | Chat completions, streaming or not. |
| `GET /v1/models` | Every upstream's models in one list, prefixed so any ID can be sent back as `model`. |
| `GET /v1/stats` | Usage for the caller's key. Admins get every key's, or one with `?key_id=`. |
| `GET /health` | Liveness probe; no key needed. |

Every `/v1` route takes `Authorization: Bearer <gateway key>` and counts against that key's rate limit. Errors use OpenAI's format, with codes such as `upstream_unavailable`, `upstream_timeout` and `upstream_auth_failed`. Upstream 4xx answers other than 401, 403 and 429, such as an unknown model or a context that's too long, are passed through.

## Limitations

- Stats and rate limits are per process, and settings are read once at startup.
- Only chat completions and model listing; no embeddings, retries or response caching yet.
- HTTPS upstreams need a publicly trusted certificate; private CAs aren't supported yet.
- Azure:
  - Only the v1 API (`/openai/v1`) is supported.
  - Managed identity has been tested against simulated Azure replies, not yet in a live deployment.
  - Managed identity isn't supported on Azure Arc, Service Fabric, Cloud Shell or Azure ML.

## License

[Apache-2.0](LICENSE)
