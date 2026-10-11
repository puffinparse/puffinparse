# PuffinParse gateway (`puffinparse serve`)

A small HTTP server in front of every provider PuffinParse supports, in the spirit of the LiteLLM
proxy: clients call one endpoint with one API shape and a gateway-issued key; the gateway holds
the provider keys, routes aliases to provider models with fallbacks, enforces per-key model
lists, monthly budgets and rate limits, and emits JSON-lines request logs and Prometheus metrics.

It is a thin layer over `puffinparse-core` (crate `crates/puffinparse-server`, axum + tower-http). It adds
no provider logic: every call is `puffinparse_core::parse / ocr / extract` with the request fields
below, so responses are exactly the unified types of [SPEC §5](SPEC.md#5-unified-responses), or
a vendor shape via `output_format` ([COMPAT.md](COMPAT.md)). Long documents can go through the
async jobs API instead (`POST /v1/jobs` + `GET /v1/jobs/{id}`, core `submit_parse` /
`retrieve_parse`, [SPEC §15](SPEC.md#15-asynchronous-jobs-and-webhooks)), so no connection is held
open while the provider works.

## Run it

```bash
export PUFFINPARSE_MASTER_KEY=sk-master-change-me
export PUFFINPARSE_KEY_BILLING=sk-billing-change-me PUFFINPARSE_KEY_RESEARCH=sk-research-change-me
export REDUCTO_API_KEY=... EXTEND_API_KEY=... LLAMA_API_KEY=...
puffinparse serve --config examples/server/puffinparse.toml          # --host / --port override the file
```

Without `--config` it reads `./puffinparse.toml` if present (or `$PUFFINPARSE_CONFIG`), else runs with
defaults: `127.0.0.1:4000`, no aliases, **no auth** (a warning is printed). Anything reachable
beyond localhost should have a `master_key` or `[[keys]]`.

> **Do not expose the gateway to untrusted callers yet.** Run it on localhost or a private
> network, behind auth. Two hardening items are still open: some models download
> `document_url` inside the gateway process (Tesseract, PaddleOCR, Unstructured, Textract,
> Gemini, the OpenAI and Anthropic vision models, Upstage, Document AI, LlamaParse extract), and
> the download does not yet refuse private or link-local addresses; and the gateway does not
> yet cap concurrency or request time. Put it behind a reverse proxy with limits, and only
> accept URLs from callers you trust.

Docker (multi-stage build, distroless runtime, runs as non-root). Releases publish the image to
`ghcr.io/ajinkyashejul/puffinparse` (tags `latest`, the version such as `0.1.2`, and `0.1`). From the
next release it is multi-arch (linux/amd64 and linux/arm64), so Apple Silicon and Graviton hosts
pull a native image; releases up to 0.1.6 are linux/amd64 only, so pass `--platform linux/amd64` for
those, or build it yourself with `docker build -t puffinparse .`. Licence files are in
`/usr/share/doc/puffinparse/`. The image contains no self-hosted engines (Tesseract,
Docling, PaddleOCR): use it for the hosted providers, or build an image that adds them.

```bash
docker run --rm -p 4000:4000 -v $PWD/examples/server/puffinparse.toml:/etc/puffinparse/puffinparse.toml:ro \
  -e PUFFINPARSE_MASTER_KEY -e PUFFINPARSE_KEY_BILLING -e PUFFINPARSE_KEY_RESEARCH \
  -e REDUCTO_API_KEY -e EXTEND_API_KEY -e LLAMA_API_KEY ghcr.io/ajinkyashejul/puffinparse
```

The image's default command is `serve --host 0.0.0.0 --config /etc/puffinparse/puffinparse.toml`; it exits
if no config is mounted rather than serving an open gateway.

## Configuration (`puffinparse.toml`)

Every secret may be written as `"env:VAR"`, resolved once at startup. A virtual key or master key
whose reference resolves to nothing is a startup error (fail closed); a provider key that resolves
to nothing prints a warning and the core falls back to the provider's default env var.

```toml
master_key = "env:PUFFINPARSE_MASTER_KEY"   # full access: all models, no budget, no rate limit

[server]
host = "127.0.0.1"
port = 4000
log_stdout = true                        # JSON-lines request log on stdout
log_file = "requests.jsonl"              # optional append-only copy
state_file = "usage.json"                # optional: per-key monthly spend survives restarts
max_body_mb = 50                         # multipart upload / base64 JSON body cap
max_timeout_secs = 300                   # cap (and default) for a request's `timeout`
max_retries = 2                          # per provider call, when the client sends none
allow_direct_models = true               # false: only the aliases below may be requested
job_retention_hours = 168                # how long POST /v1/jobs handles stay readable

[webhooks]                               # optional provider webhook receiver (off by default)
enabled = false
secret = "env:PUFFINPARSE_WEBHOOK_SECRET"    # required when enabled; sent as ?token= or a header

[providers.reducto]                      # one table per provider (name as in `puffinparse providers`)
api_key = "env:REDUCTO_API_KEY"
# base_url = "https://eu.platform.reducto.ai"

[[models]]                               # an alias clients can send as "model"
name = "invoices"
targets = ["reducto/standard", { model = "extend/parse_performance", api_key = "env:EXTEND_KEY_2" }]
strategy = "ordered"                     # or "round_robin"
fallback_on = ["provider", "rate_limit", "timeout", "network"]   # the default

[[keys]]
id = "billing-team"                      # appears in logs, metrics, usage; never the secret
key = "env:PUFFINPARSE_KEY_BILLING"          # the bearer token the client sends
models = ["invoices", "llamaparse/*"]    # aliases, provider/model, provider/*, or "*"; empty = all
monthly_budget_usd = 50.0                # calendar month, UTC
rpm = 60                                 # requests per minute, sliding 60 s window
```

A complete sample is [`examples/server/puffinparse.toml`](../examples/server/puffinparse.toml).

**Routing.** `model` is looked up as an alias first, then (if `allow_direct_models`) as a registry
model (`reducto/standard`, or a bare provider for its default model in that mode). An alias expands
to its targets: `ordered` always starts at the first, `round_robin` rotates the start per request,
and both fall back in order when the error kind is in `fallback_on`. A request's own `fallbacks`
list is appended after that (aliases expand in order). Every target is checked against the
endpoint's mode before any provider is called. Target-level `api_key` / `base_url` override the
`[providers.*]` ones, so two deployments of the same provider (two accounts, two regions) can sit
behind one alias. When a fallback served the call, the response metadata carries
`puffinparse_fallback_index` and `puffinparse_fallback_from_error`, as with the SDK `Router`.

**Budgets.** Spend is the response's `cost_usd` (provider-reported cost when available, otherwise
the list-price estimate from `pricing.json`), summed per key per UTC calendar month. The check runs
before the call: a key whose spend has reached its budget gets `402`. One in-flight request can take
a key past its budget; nothing is charged for failed attempts (a provider may still bill a failed
job). An async job is charged to the key that submitted it, once, when it is first observed
succeeded (by `GET /v1/jobs/{id}` or a webhook); submitting and polling are free. State is in
memory; with `state_file` it (spend, counts and submitted jobs) is rewritten (temp file + rename)
after every change and reloaded on startup. Rate-limit windows are not persisted.

## API

Authenticate with `Authorization: Bearer <key>` or `x-api-key: <key>`. Send `x-request-id` to
choose the request id (echoed in the `x-request-id` response header and the log), otherwise a
UUID is generated.

### `POST /v1/parse`, `POST /v1/ocr`, `POST /v1/extract`

Body is JSON or `multipart/form-data` with the same field names (multipart fields are text; the
JSON-valued ones — `provider_options`, `schema`, `metadata`, `fallbacks` — are JSON strings, and
`fallbacks` may also be comma-separated).

| Field | Type | Notes |
|---|---|---|
| `model` | string | **Required.** Alias or `provider/model`. |
| `document_url` | string | Public http(s) URL. Passed to the provider where it accepts URLs; otherwise the gateway downloads it (see the warning above). |
| `document` | string | Base64 bytes (a `data:…;base64,` prefix is accepted). Needs `filename`. |
| `file` | multipart file part | The upload; its filename sets the type (or send `filename`). |
| `filename` | string | Sets the MIME type for `document` / overrides the part's filename. |
| `pages`, `language` | string | As in SPEC §4.1. |
| `output` | `"markdown"` \| `"text"` | Block content format (parse). |
| `output_format` | `"reducto"` \| `"extend"` \| `"llamaparse"` \| `"puffinparse"` | Vendor-native response shape (parse, extract). |
| `provider_options` | object | Merged into the provider request verbatim. |
| `include_raw` | bool | Attach the provider payload as `raw`. |
| `timeout` | number (s) | Capped at `server.max_timeout_secs`. |
| `max_retries` | int | Per provider call (max 10). |
| `fallbacks` | string[] | Extra aliases/models tried after `model`'s own targets. |
| `metadata` | object | Echoed back in the response. |
| `schema`, `instructions`, `citations` | | `/v1/extract` only; `schema` required there. |

Exactly one of `document_url`, `document`, `file`. `api_key` and `base_url` are **rejected**: a
client must never be able to point the gateway's provider credentials at another host. Local file
paths are not accepted either.

The response is the unified `ParseResponse` / `TextResponse` / `ExtractResponse` JSON (or the
vendor shape), with headers `x-puffinparse-model` (served model), `x-puffinparse-cost-usd`, `x-request-id`.

### `POST /v1/jobs`, `GET /v1/jobs/{id}` (async parse)

`POST /v1/jobs` takes the `/v1/parse` body (JSON or multipart, same fields) plus an optional
`webhook_url`, uploads the document, starts the provider job and answers **202** at once:

```json
{"id": "job_4f0c…", "object": "job", "status": "pending",
 "job": {"provider": "reducto", "model": "reducto/standard", "job_id": "c1e2…",
         "submitted_at": "2026-09-24T20:23:52.140Z", "output": "markdown", "include_raw": false}}
```

`job` is the core `JobHandle` (SPEC §15) minus the operator's `base_url`. Authentication, the
key's model allow-list, aliases, the budget pre-check and `rpm` apply exactly as for
`/v1/parse`. Only providers with a job queue can take jobs (`reducto`, `extend`, `llamaparse`,
`opendocrouter`; others give `400 unsupported_model_error`). **Fallback does not apply to jobs**: an alias submits
to its first target (in `round_robin`, the next one in rotation) and a request `fallbacks` list is
rejected, because a provider failure only shows up later, on retrieve. `webhook_url` is
forwarded to the provider's per-job webhook (Reducto `async.webhook`, LlamaParse `webhook_url`;
Extend and OpenDocRouter reject it) and is refused by the synchronous endpoints.

`GET /v1/jobs/{id}` asks the provider once and returns

```json
{"id": "job_4f0c…", "object": "job", "status": "pending" | "succeeded" | "failed",
 "model": "reducto/standard", "provider": "reducto", "provider_job_id": "c1e2…",
 "submitted_at": "…", "result": {…ParseResponse…}, "error": {…error object…}}
```

with `result` only when `succeeded` (the unified `ParseResponse`, or the vendor shape for the
`output_format` sent at submit time; `?output_format=reducto` overrides it per call) and `error`
only when `failed` (the same object as an error body's `error`, with the provider's message and
`job_id`). A failed *job* is still HTTP 200; a failed *status check* (provider credentials,
network) is an error response as usual. Job ids are opaque and bound to the key that created the
job: any other key gets `404 not_found`, exactly as for an unknown id (the master key can read all
jobs). Polls count toward the key's `rpm` but are not budget-gated. The job's cost is charged to
its owner once, the first time it is seen succeeded. Credentials are resolved from the config on
every poll (the alias target's own key, else `[providers.*]`), never stored; handles are kept for
`job_retention_hours` (and in `state_file` when set).

### `POST /v1/webhooks/{provider}` (optional)

Off by default (404). With `[webhooks] enabled = true` and a `secret`, the gateway accepts the
body a provider POSTs when a job changes state — Reducto direct webhooks, Extend `parse_run.*`
events, LlamaCloud `parse.*` events — authenticated by `?token=<secret>` or the
`x-puffinparse-webhook-secret` header (compared in constant time). The body is read with core
`parse_webhook`; when it only says the job finished, the gateway makes one status check. The
provider's job id must match a job submitted through this gateway (else 404); a success is
charged to the job's owner (once, shared with `GET`), and the answer is an acknowledgement
`{"ids": [...], "status"}` — clients still collect the result with `GET /v1/jobs/{id}`. `ids` can
hold several gateway jobs: LlamaParse returns the same (cached) job id for an identical upload, so
two submissions of the same file share one provider job, and each is settled and charged to its
own key. Point a provider's
webhook at `https://<gateway>/v1/webhooks/reducto?token=<secret>` (per job via `webhook_url`, or a
workspace-level endpoint for Extend / LlamaCloud). This is a shared secret, not the vendors' HMAC
signatures: keep the URL private and serve the gateway over TLS. A LlamaParse `webhook_url`
result push that names no job id cannot be attributed and is rejected with 400.

### Errors

Every error has the same body:

```json
{"error": {"type": "provider_error", "message": "…the provider's own message…", "provider": "reducto",
           "provider_status": 503, "job_id": null, "request_id": "5c0f…"}}
```

| HTTP | `type` | Cause |
|---|---|---|
| 400 | `input_error`, `bad_request_error`, `unsupported_model_error` | Malformed request, provider 4xx, unknown model or model without this mode |
| 401 | `unauthorized` | Missing or unknown gateway key |
| 402 | `budget_exceeded` | Key's monthly budget spent |
| 403 | `model_not_allowed` | Model (or a fallback) not in the key's `models` |
| 404 | `not_found` | Unknown job id, a job another key owns, or webhooks disabled |
| 413 | `payload_too_large` | Body over `max_body_mb` |
| 429 | `key_rate_limited` | Key's `rpm` reached (`Retry-After` set) |
| 429 | `rate_limit_error` | Provider rate limit after retries and fallbacks |
| 502 | `provider_error`, `network_error`, `authentication_error` | Provider 5xx / failed job, network failure, provider rejected the gateway's credentials |
| 504 | `timeout_error` | `timeout` exceeded |

Provider credential failures are 502, not 401: the caller's key was fine, the operator's was not.

### `GET /v1/models`

Aliases (targets, strategy, the modes every target supports) and, with `allow_direct_models`,
every registry model with its modes, per-page list price per mode and whether a provider key is
configured. Filtered to what the caller's key may use.

### `GET /v1/usage`

This month's spend, requests, pages, remaining budget and limits: the caller's own key, or all
keys for the master key.

### `GET /health`, `GET /metrics`

`/health` → `{"status": "ok", "version": …}`. `/metrics` is Prometheus text, unauthenticated
(it carries model names, counts and costs, no secrets or content; firewall it if that matters):

| Metric | Labels |
|---|---|
| `puffinparse_requests_total` | `mode`, `model` (served model, or requested alias on failure, `-` if it never resolved), `status` |
| `puffinparse_errors_total` | `type` (the error `type` above) |
| `puffinparse_request_duration_seconds` (histogram, 0.25 s – 300 s buckets) | `mode` |
| `puffinparse_pages_total` | `model` |
| `puffinparse_cost_usd_total` | `model` |
| `puffinparse_fallbacks_total` | — |
| `puffinparse_jobs_total` | `event`: `submitted`, and `succeeded` / `failed` the first time a job is seen terminal |

`mode` is `parse` / `ocr` / `extract` for the synchronous endpoints and `job_submit`,
`job_retrieve`, `webhook` for the jobs API, so job latencies do not mix with blocking calls. A
job's pages and cost are counted once, when it is first seen succeeded.

## Request log

One JSON object per request, on stdout and/or `log_file`:

```json
{"ts":"2026-09-24T20:23:52.140Z","request_id":"bf397168-…","key_id":"demo","method":"POST",
 "path":"/v1/parse","mode":"parse","model":"cheap","served_model":"llamaparse/cost_effective",
 "provider":"llamaparse","fallback_index":0,"pages":1,"cost_usd":0.00375,"latency_ms":10384,
 "status":200,"error_type":null,"provider_status":null}
```

Jobs API lines add `job_id` (the gateway id) and `job_status` (`pending` / `succeeded` /
`failed`, as observed by that request); `method` is `GET` for status checks, and `key_id` is
`webhook` for provider webhooks. `pages` and `cost_usd` are set only on the request that charged
the job.

The record has no field for document bytes, URLs, extracted content, provider error text (some
providers echo document text in errors), provider keys or gateway key secrets.

## curl examples

```bash
GW=http://127.0.0.1:4000; KEY=sk-billing-change-me

# Upload a file (multipart) to an alias
curl -s $GW/v1/parse -H "Authorization: Bearer $KEY" -F model=invoices -F file=@invoice.pdf | jq .markdown

# URL input, vendor-native shape, a request-level fallback
curl -s $GW/v1/parse -H "Authorization: Bearer $KEY" -H 'content-type: application/json' -d '{
  "model": "invoices", "document_url": "https://example.com/invoice.pdf",
  "output_format": "reducto", "fallbacks": ["llamaparse/cost_effective"]}'

# Base64 input, OCR mode
curl -s $GW/v1/ocr -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d "{\"model\": \"reducto\", \"filename\": \"scan.png\", \"document\": \"$(base64 -w0 scan.png)\"}" | jq .text

# Extract with a schema
curl -s $GW/v1/extract -H "Authorization: Bearer $KEY" -F model=invoice-fields -F file=@invoice.pdf \
  -F 'schema={"type":"object","properties":{"total":{"type":"number"}}}'

# Async job: submit, then poll until status is succeeded or failed
JOB=$(curl -s $GW/v1/jobs -H "Authorization: Bearer $KEY" -F model=invoices -F file=@big.pdf | jq -r .id)
curl -s $GW/v1/jobs/$JOB -H "Authorization: Bearer $KEY" | jq .status
curl -s "$GW/v1/jobs/$JOB?output_format=reducto" -H "Authorization: Bearer $KEY" | jq .result

curl -s $GW/v1/models -H "Authorization: Bearer $KEY" | jq '.data[].id'
curl -s $GW/v1/usage  -H "Authorization: Bearer $KEY"
curl -s $GW/metrics
```

## Not in scope (yet)

Streaming, jobs for `ocr` / `extract` (jobs are parse-only, like the core), fallback for jobs,
the gateway registering its own webhook URL with providers automatically, vendor HMAC signature
checks on webhooks, key management over HTTP (keys live in the config file; restart to change them), a database, response
caching, per-key budgets by model, TLS termination (put it behind a reverse proxy), and metrics
auth.
