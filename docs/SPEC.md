# PuffinParse Specification

> **One API for every OCR / document-parsing provider.** Rust core, Python SDK, CLI,
> and an open benchmark that ranks providers on accuracy, latency and cost.

Status: `v0.1` — 19 providers (the registry in `crates/puffinparse-core/src/model.rs`; per-provider
references in [`providers/`](providers/README.md)). §8 spells out the wire mapping for the three
live-verified providers, **Reducto**, **Extend** and **LlamaParse**, and the self-hosted engines.

---

## 1. Goals and non-goals

### Goals

1. **Single call, any provider.** `puffinparse.parse("invoice.pdf", model="reducto/standard")`
   returns the same `ParseResponse` shape whether the backend is Reducto, Extend,
   LlamaParse, or anything added later. Switching provider is a one-string change,
   within a mode (§3.1).
2. **Fast and lite.** The core is a Rust library (`puffinparse-core`) with a small
   dependency set. Python only wraps it (PyO3). No provider SDKs are vendored;
   every provider is talked to over plain HTTPS with `reqwest`.
3. **Cost tracking.** Every response carries `usage` (pages, provider credits)
   and a computed `cost_usd` from an embedded, overridable pricing table.
4. **Reliability primitives.** Retries with backoff, per-call timeouts, and a
   `Router` with ordered fallbacks and simple load balancing.
5. **Open benchmark.** A reproducible harness + datasets + metrics that ranks
   providers, with results committed to the repo and published as a leaderboard.
6. **Open-source standards.** MIT license, CI (fmt, clippy, tests, Python lint +
   tests), semver, CHANGELOG, contributor docs, typed Python API, docstrings.

### Non-goals (v0.1)

- A hosted service. (A self-hosted gateway now exists: `puffinparse serve`, §14.)
- Bundling or running OCR models in-process. Local and self-hosted engines are supported only
  as providers that call *out* to them — the `tesseract` binary via `tokio::process`, a
  docling-serve or PaddleOCR serving endpoint over HTTP (§8.4) — never through C bindings or an
  embedded runtime.
- A parsing-only scope. v0.1 specifies and wires three modes end to end (§3.1); which models
  serve `extract` is a registry fact reported by `list_models("extract")`, and the three
  original providers serve `parse` and `ocr` only. Classification and other vendor products
  remain out of scope.

---

## 2. Architecture

```
┌────────────────────────────────────────────────────────────────────┐
│  Python SDK (python/puffinparse)         CLI (crates/puffinparse-cli)      │
│  parse / ocr / extract (+ a*)        puffinparse parse | ocr | extract │
│  Router(models, mode=...)            puffinparse providers | bench     │
└───────────────┬────────────────────────────────┬───────────────────┘
                │ PyO3 (crates/puffinparse-python)   │
┌───────────────▼────────────────────────────────▼───────────────────┐
│  puffinparse-core (Rust)                                               │
│  ├─ modes        parse → Parse / ocr → Text / extract → Extract     │
│  ├─ types        DocumentRequest / ParseResponse / TextResponse /   │
│  │               ExtractResponse / Page / Block / Line / Word       │
│  ├─ providers    trait Provider     { reducto, extend, llamaparse } │
│  ├─ router       fallbacks, retries, strategy (ordered/round-robin) │
│  ├─ pricing      embedded per-mode price table → cost_usd           │
│  ├─ input        path | bytes | url  → DocumentInput                │
│  └─ bench        text normalisation + metrics (CER, WER, similarity)│
└────────────────────────────────────────────────────────────────────┘
```

Crates:

| Crate | Purpose |
|---|---|
| `crates/puffinparse-core` | Library. All provider logic, types, router, pricing, benchmark metrics. `#![forbid(unsafe_code)]`. |
| `crates/puffinparse-cli` | `puffinparse` binary: `parse`, `ocr`, `extract`, `providers`, `bench run`, `bench report`. |
| `crates/puffinparse-python` | PyO3 extension module `puffinparse._core`, built with maturin. |
| `crates/puffinparse-server` | HTTP gateway behind `puffinparse serve` (axum): aliases, virtual keys, budgets, metrics. §14, `docs/SERVER.md`. |
| `python/puffinparse` | Pure-Python public API, dataclasses, callbacks, typing. |
| `benchmark/` | Datasets, manifests, ground truth, results, leaderboard generator. |

---

## 3. Modes and model naming

### 3.1 Modes

Document-AI vendors sell three different products, and they are not interchangeable. PuffinParse
makes that explicit: every call names a **mode**, and the mode decides both which providers can
serve it and what comes back.

| Mode | Entry point | Response | What it is for |
|---|---|---|---|
| `parse` | `puffinparse.parse` / `puffinparse_core::parse` | `ParseResponse` (§5.1) | layout-aware markdown + typed blocks: RAG chunks, tables, structure |
| `ocr` | `puffinparse.ocr` / `puffinparse_core::ocr` | `TextResponse` (§5.2) | plain text with line/word boxes: search, redaction, overlays |
| `extract` | `puffinparse.extract` / `puffinparse_core::extract` | `ExtractResponse` (§5.3) | a JSON object shaped by a schema, with per-field citations |

**Providers are swappable only within a mode.** A one-string provider switch is only honest
between models that do the same job: a markdown parse and a schema extraction are not
substitutes for each other, and a router that silently fell back from one to the other would
change the shape of the answer. So each model in the registry declares `modes: &[Mode]`, and
`ModelRef::parse_for(model, mode)` rejects a model that does not serve the requested mode —
before any network call, with a message naming the mode and the models that do serve it.

`Mode` is a Rust enum (`Mode::{Parse, Ocr, Extract}`) with `FromStr` (`"ocr"`/`"text"`,
`"extract"`/`"extraction"`, case-insensitive), `as_str`, and `Mode::ALL`. In Python it is the
string literal type `puffinparse.Mode = Literal["parse", "ocr", "extract"]`. `list_models(mode)`
(`list_models_for` in Rust) lists the models for one mode; with no mode it lists all of them.

A provider that has no native OCR endpoint serves `ocr` from its own parse output (the default
`Provider::ocr` implementation): lines come from block text, words from lines, and the response
is tagged `metadata["puffinparse_derived_from"] = "parse"` so callers can tell native OCR geometry
from derived geometry. Nothing is derived across any other mode pair.

### 3.2 Model naming

Like LiteLLM, the `model` string selects provider and model: `"<provider>/<model>"`.

| Provider | Models (v0.1) | Modes | Maps to |
|---|---|---|---|
| `reducto` | `reducto/standard` (default), `reducto/r-1`, `reducto/agentic` | parse, ocr | default settings / `settings.model="r-1"` / `enhance.agentic=[{scope:text},{scope:table}]` |
| `reducto` | `reducto/extract` (default), `reducto/deep_extract` | extract | `POST /extract` / `settings.deep_extract=true` |
| `extend` | `extend/parse_performance` (default), `extend/parse_light`, `extend/parse_auto` | parse, ocr | `config.engine` |
| `extend` | `extend/extraction_performance` (default), `extend/extraction_light` | extract | Extract run, `baseProcessor` |
| `llamaparse` | `llamaparse/fast`, `llamaparse/cost_effective` (default), `llamaparse/agentic`, `llamaparse/agentic_plus` | parse, ocr; extract from `cost_effective` up | `tier` form field (+ `version=latest`); LlamaExtract `POST /api/v2/extract` |

These are the live-verified three; the other providers' models are listed in the README model
tables and `docs/providers/`. `list_models("extract")` reports which models serve extraction;
calling `extract` with a parse-only model raises `UnsupportedModelError` before any network call.

Aliases: `llama`, `llama_parse`, `llamacloud` → `llamaparse`. Matching is case-insensitive.

`model="reducto"` (no slash) selects the provider's default model **for the mode being called**.
Unknown providers or models raise `UnsupportedModelError` before any network call, as does a
known model asked for a mode it does not declare.

Provider-specific knobs that do not fit the common request are passed through
`provider_options` (a JSON object) and merged into the provider request body
verbatim. This is the escape hatch; it never changes the response shape.

---

## 4. Unified request

### 4.1 The common document request

Every mode takes the same document request; `parse` and `ocr` take nothing else.

```python
puffinparse.parse(
    input,                       # str path | pathlib.Path | bytes | "https://..." URL
    model: str = "reducto",      # "<provider>/<model>", must support this mode
    *,
    filename: str | None = None, # required when input is bytes
    pages: str | None = None,    # "1-3,7" 1-based page selection (best effort per provider)
    language: str | None = None, # BCP-47 hint, forwarded if provider supports it
    output: Literal["markdown", "text"] = "markdown",   # preferred `content` of blocks (parse only)
    output_format: str | None = None,   # "reducto" | "extend" | "llamaparse": return that vendor's
                                        # native JSON shape instead of the unified response
                                        # (docs/COMPAT.md, ADR-13); None = unified
    provider_options: dict | None = None,
    include_raw: bool = False,   # attach the provider's raw JSON to response.raw
    timeout: float = 300.0,      # seconds, whole call including polling
    max_retries: int = 2,        # on 429 / 5xx / network errors, exponential backoff
    api_key: str | None = None,  # overrides env var
    base_url: str | None = None, # overrides provider base URL
    metadata: dict | None = None # echoed back, useful for callbacks/logging
) -> ParseResponse
```

`aparse(...)` is the `async def` equivalent. In Rust this is `DocumentRequest`, a builder over
the same fields. `DocumentRequest` also carries `webhook_url: Option<String>`, used only when the
request is *submitted* as a job (§15; Python `submit(..., webhook_url=...)`); `parse` ignores it.

### 4.2 `ocr`

```python
puffinparse.ocr(input, model="reducto", *, ...same keywords, minus `output`...) -> TextResponse
```

`ocr` always returns plain text, so `output` does not apply; `aocr(...)` is the async form.

### 4.3 `extract`

```python
puffinparse.extract(
    input,
    schema: dict,                # JSON Schema (draft 2020-12 subset) object for the result
    *,
    model: str = "reducto",      # must support the `extract` mode
    instructions: str | None = None,  # extra natural-language guidance, forwarded if supported
    citations: bool = False,     # ask for per-field page/box/source-text citations
    ...same common keywords as §4.1...
) -> ExtractResponse
```

`aextract(...)` is the async form. In Rust this is `ExtractRequest { document: DocumentRequest
(flattened when serialised), schema, instructions, citations }`. A `schema` that is not a JSON
object is rejected before any network call (`TypeError` in Python, `InputError` in the core).

### 4.4 Input handling

Input handling (`DocumentInput`) is identical in all three modes:

- **Path** → read bytes, sniff MIME from extension (`mime_guess`), upload.
- **Bytes** → require `filename` (used for MIME + provider upload).
- **URL** (`http(s)://`) → passed to the provider as a remote URL when the
  provider supports it (all three do); otherwise downloaded and uploaded. Which providers download
  in-process is `fetch::fetches_url_in_process(provider, mode)`; the download itself is
  `fetch::fetch_document`: http(s) only, public addresses only (the resolved address is checked and
  pinned, redirects re-checked, at most 5), at most 50 MiB (`PUFFINPARSE_MAX_DOWNLOAD_MB`), within
  the request deadline, and error messages never include the response body.
  `PUFFINPARSE_ALLOW_PRIVATE_URLS=1` (process-wide, never per request) lifts the address check for
  trusted setups; an embedding application may fix the policy with `fetch::set_process_policy`.
  The self-hosted engines' servers (docling-serve, PaddleOCR serving) are never handed a URL.

Supported document types are whatever the provider accepts; PuffinParse does not
pre-validate beyond a non-empty body.

---

## 5. Unified responses

One response type per mode. All three carry the same envelope — `id`, `provider`, `model`,
`provider_job_id`, `usage`, `cost_usd`, `latency_ms`, `created_at`, `metadata`, `raw` — and
differ only in the payload.

### 5.1 `parse` → `ParseResponse`

```python
@dataclass
class ParseResponse:
    id: str                    # puffinparse-generated uuid
    provider: str              # "reducto"
    model: str                 # "reducto/standard"
    provider_job_id: str | None
    pages: list[Page]
    markdown: str              # whole document, pages joined by "\n\n"
    text: str                  # plain text
    usage: Usage
    cost_usd: float | None     # None if pricing unknown
    latency_ms: int            # wall-clock for the whole call, incl. polling
    created_at: str            # RFC 3339
    metadata: dict
    raw: Any | None            # provider payload if include_raw

@dataclass
class Page:
    page_number: int           # 1-based
    width: float | None        # points/pixels if provider reports it
    height: float | None
    markdown: str
    text: str
    blocks: list[Block]

@dataclass
class Block:
    type: BlockType            # text | title | section_header | list | table | figure |
                               # header | footer | footnote | caption | formula | other
    content: str               # markdown (tables as markdown/HTML per provider)
    text: str | None           # plain text if provider gives a separate one
    bbox: BBox | None          # normalised 0..1 {x0, y0, x1, y1}, origin top-left
    confidence: float | None   # 0..1 if provider reports one
    page_number: int

@dataclass
class Usage:
    pages: int                 # pages billed/processed
    credits: float | None      # provider-native credit units, if any
    provider_cost_usd: float | None   # if the provider reports $ directly
```

Rules:

- `markdown`/`text` at document level are derived from pages in order.
- A provider that only returns per-chunk (not per-page) content gets pages
  reconstructed from block page numbers; if that is impossible, a single page
  `page_number=1` is emitted and `Usage.pages` still reflects the billed count.
- Block types are mapped from each provider's vocabulary (see §8). Unknown → `other`.
- `bbox` is normalised so consumers can draw overlays without knowing the
  page size. Providers reporting absolute coordinates are divided by page dims.

### 5.2 `ocr` → `TextResponse`

```python
@dataclass
class TextResponse:
    id: str
    provider: str
    model: str
    provider_job_id: str | None
    pages: list[TextPage]
    text: str                  # whole document, pages joined by "\n\n"
    usage: Usage
    cost_usd: float | None
    latency_ms: int
    created_at: str
    metadata: dict             # "puffinparse_derived_from": "parse" when derived (§3.1)
    raw: Any | None

@dataclass
class TextPage:
    page_number: int           # 1-based
    width: float | None
    height: float | None
    text: str                  # plain text in reading order, lines separated by "\n"
    lines: list[Line]
    words: list[Word]

@dataclass
class Line:                    # and Word, identical shape
    text: str
    bbox: BBox | None          # normalised 0..1, origin top-left
    confidence: float | None
```

No markdown, no block types: `ocr` is recognition, not layout analysis. When the result is
derived from a parse (§3.1), lines carry their block's box and words carry none.

### 5.3 `extract` → `ExtractResponse`

```python
@dataclass
class ExtractResponse:
    id: str
    provider: str
    model: str
    provider_job_id: str | None
    data: Any                  # the extracted object, shaped by the request schema
    fields: dict[str, FieldInfo]   # keyed by JSON pointer into `data`, e.g. "/invoice/total"
    usage: Usage
    cost_usd: float | None
    latency_ms: int
    created_at: str
    metadata: dict
    raw: Any | None

@dataclass
class FieldInfo:
    confidence: float | None
    citations: list[Citation]

@dataclass
class Citation:
    page_number: int           # 1-based
    bbox: BBox | None          # normalised 0..1, origin top-left
    text: str | None           # source text the value was read from, if reported
```

`fields` is empty when the provider reports no per-field metadata; `citations` is only populated
when the request asked for them and the provider supports them. `ExtractResponse.field_info(p)`
and `.citations(p)` are Python conveniences over the pointer map.

---

## 6. Errors

All errors derive from `puffinparse.PuffinParseError`:

| Error | When |
|---|---|
| `AuthenticationError` | 401/403 from provider or missing API key |
| `RateLimitError` | 429 (retried first; raised after `max_retries`) |
| `BadRequestError` | 4xx other than auth/rate limit |
| `ProviderError` | 5xx or provider-reported job failure |
| `TimeoutError` | overall timeout (upload + poll) exceeded |
| `UnsupportedModelError` | bad `model` string, or a model that does not serve the requested mode |
| `InputError` | unreadable file, bytes without filename, empty body, bad mode name, router asked for another mode |

Every error carries `provider`, `status_code` (if any), `message`, and
`request_id`/`job_id` when available.

Mode errors are raised before any network call. `UnsupportedModelError` from a mode mismatch
names the offending model, the modes it does serve, and the models that serve the mode you
asked for.

---

## 7. Router

```python
router = puffinparse.Router(
    models=["reducto/standard", "llamaparse/agentic", "extend/parse_light"],
    mode="parse",                # "parse" (default) | "ocr" | "extract"
    strategy="ordered",          # "ordered" (fallback order) | "round_robin"
    fallback_on=("ProviderError", "RateLimitError", "TimeoutError", "NetworkError"),
)
resp = router.parse("doc.pdf")   # tries each in turn / rotates
```

A router is bound to one mode. Every model is validated against it at construction
(`UnsupportedModelError` otherwise), and calling a method for a different mode — say
`Router([...], mode="parse").ocr(...)` — raises `InputError` rather than answering with a
different shape. `router.mode` reports it; `parse`/`ocr`/`extract` (and `aparse`/`aocr`/
`aextract`) are the per-mode calls, each taking the same arguments as the module-level function
minus `model`.

Semantics: `ordered` → try `models[0]`, on a fallback-eligible error move on.
`round_robin` → rotate the starting index per call, then fallback in order.
The router records per-model success/failure counts and average latency,
exposed as `router.stats()`. Auth/BadRequest/Input errors never trigger fallback.
When a fallback served the call, the response metadata carries `puffinparse_fallback_index` and
`puffinparse_fallback_from_error`.

---

## 8. Provider mapping

All three mappings below were verified against live API responses on 2026-09-11;
the captured payloads live in `crates/puffinparse-core/tests/fixtures/` and drive unit tests.

### 8.1 Reducto

- Base: `https://platform.reducto.ai`, header `Authorization: Bearer <key>`
  (`REDUCTO_API_KEY`, `REDUCTO_BASE_URL` override).
- Flow: `POST /upload` (multipart `file`) → `{file_id: "reducto://…"}`; URLs are passed
  directly. Then `POST /parse` (sync, 900 s ceiling) with the v3 body
  `{input, retrieval:{chunking:{chunk_mode:"page"}}, formatting:{table_output_format:"md"}, settings:{…}}`.
  With `provider_options.async = true`: `POST /parse_async` → `{job_id}` → poll
  `GET /job/{id}` (`Pending` → `Completed` | `Failed`); the parse payload is `job.result`.
- `pages` → `settings.page_range = [{start,end}]` (1-based).
- Response: `result.type` is `"full"` (`chunks[]`) or `"url"` (fetch `result.url`; the body is
  the same `FullResult` object). `chunks[].blocks[]` carry `type` (values contain spaces,
  e.g. `"Section Header"`), `bbox{left,top,width,height,page,original_page}` already normalised
  to 0..1, `content`, `confidence` (`"high"|"low"`), `granular_confidence.parse_confidence`.
  `usage.num_pages`, `usage.credits` (null on per-product pricing accounts).
- Block types: `Title→title`, `Section Header→section_header`, `Text`/`Key Value`/`Comment→text`,
  `List Item→list`, `Table→table`, `Figure→figure`, `Header→header`, `Footer→footer`,
  `Footnote→footnote`, `Caption→caption`, `Formula→formula`, else `other`.
- Pages: with `chunk_mode=page` each chunk is one page; page number is taken from the chunk's
  blocks' `bbox.page` (chunks themselves have no page field). Chunk `content` is kept as the
  page markdown.
- Errors: `{"error":{"code","name","message"},"detail"}`, `422` Pydantic arrays, a bare nginx
  HTML `403` when the header is missing, and a non-standard `442` for password-protected files.

### 8.2 Extend

- Base: `https://api.extend.ai` (`EXTEND_API_KEY`, `EXTEND_BASE_URL`), headers
  `Authorization: Bearer <key>` and the **mandatory** `x-extend-api-version: 2026-02-09`.
  `provider_options.workspace_id` sets `x-extend-workspace-id` for org-scoped keys.
- Flow: `POST /files/upload` (multipart) → `{id: "file_…"}`; URLs are passed as
  `file:{url,name}`. Then `POST /parse_runs` (async) → poll `GET /parse_runs/{id}` until
  `status ∈ {PROCESSED, FAILED}`. (The sync `POST /parse` has a 5-minute hard limit, so PuffinParse
  always uses runs.)
- Body: `{file, config:{target:"markdown", chunkingStrategy:{type:"page"}, engine,
  blockOptions:{tables:{targetFormat:"markdown"}}, advancedOptions:{pageRanges}}}`.
  `provider_options` keys `target`, `chunkingStrategy`, `engine`, `engineVersion`,
  `blockOptions`, `advancedOptions` are merged into `config`; others (`metadata`,
  `dataRetention`) at top level.
- Response: the run object itself: `output.chunks[]` (`type:"page"`, `content`,
  `metadata.pageRange`) with `blocks[]` (`type`, `content`, `metadata.page{number,width,height}`,
  `metadata.avgOcrConfidence`, `boundingBox{left,top,right,bottom}` in page pixels);
  `metrics.pageCount`, `usage.credits`. `responseType=url` results are fetched from `outputUrl`.
- Block types: `heading→title`, `section_heading→section_header`, `text`/`key_value→text`,
  `table`/`table_head`/`table_cell→table`, `figure→figure`, `formula→formula`, `header`,
  `footer`, else `other` (`page_number`, `barcode`).
- Errors: `{code, message, requestId, retryable}`; failed runs carry `failureReason`.

### 8.3 LlamaParse

- Base: `https://api.cloud.llamaindex.ai` (`LLAMA_API_KEY`, `LLAMA_BASE_URL`; EU:
  `https://api.cloud.eu.llamaindex.ai`), header `Authorization: Bearer llx-…`.
- Flow: `POST /api/v1/parsing/upload` (multipart `file` or `input_url`; form fields
  `tier`, `version=latest`, `language`, `target_pages` (0-based, converted from `pages`), plus any
  `provider_options` as extra form fields) → `{id, status}`; poll `GET /api/v1/parsing/job/{id}`
  until `SUCCESS | PARTIAL_SUCCESS | ERROR | CANCELLED`; then
  `GET /api/v1/parsing/job/{id}/result/json`.
- Response: `pages[].{page (1-based), text, md, items[], width, height}`; items have `type`
  (`heading` with `lvl`, `text`, `table`), `md`, `value`, `bBox{x,y,w,h,confidence}` in page
  units. `job_metadata.job_pages`; `job_credits_usage` is `0` until billing settles and is
  therefore only reported when positive.
- Block types: `heading` lvl 1 → `title`, other headings → `section_header`, `text→text`,
  `table→table`, else `other`.
- Errors: FastAPI `{"detail": "…"}` / `{"detail": [ValidationError]}`.

### 8.4 Self-hosted engines (Tesseract, Docling, PaddleOCR, vLLM)

Listed in `model::SELF_HOSTED`; `ProviderInfo::self_hosted()` is `true`, no API key is required
(`env_var` is empty, or names an optional key), prices are `0.0` with `source: "self-hosted"`, and
`puffinparse providers` shows `local` in the Key column (`self_hosted` / `key_required` in `--json`
and in Python `providers()`).

- `tesseract/default`: `tesseract <image> stdout ... tsv`, PDFs rasterised with `pdftoppm`; `ocr`
  is native (TSV words/lines, confidences), `parse` is one `text` block per Tesseract paragraph.
- `docling/default`: docling-serve `POST /v1/convert/source/async` → poll → `GET /v1/result`;
  DoclingDocument items mapped to blocks, bottom-left boxes flipped to top-left.
- `paddleocr/default`: PaddleX serving `POST /ocr` (ocr) and `POST /layout-parsing`
  (PP-StructureV3, parse). `paddleocr/vl`: `POST /layout-parsing` on a PaddleOCR-VL pipeline server
  (same response shape); `ocr` derived from `parse`.
- `vllm/<preset>`: one `POST /v1/chat/completions` per page image (PDFs rasterised with
  `pdftoppm`) to your vLLM / OpenAI-compatible server, with the preset's model-card prompt and
  sampling; the answer's layout cells (`bbox`, `category`, `text`) become typed blocks with
  normalised boxes (`infinity-parser2-flash`: 0–1000 grid; `dots.mocr`: pixels after
  `smart_resize`). `ocr` derived from `parse`.

Details, errors and limits: `docs/providers/{tesseract,docling,paddleocr,vllm}.md`.

---

## 9. Pricing

`crates/puffinparse-core/pricing.json` (embedded via `include_str!`) maps
`"<provider>/<model>"` → `{ "parse": float, "ocr": float, "extract": float, "source": url,
"updated": date }` — a per-page price **per mode**, each optional, since vendors price parsing,
OCR and extraction differently. `cost_usd = usage.pages * price_per_page(model, mode)` unless the
provider reports credits with a known credit price, in which case `credits * per_credit_usd` is
used. A mode with no price yields `cost_usd = None`.
Users can override one mode at a time with
`puffinparse.set_pricing({"reducto/standard": 0.01}, "parse")`, and ask for an estimate with
`puffinparse.estimate_cost("reducto/standard", 1000, "ocr")`.
Prices are best-effort public list prices; the benchmark reports them as such.

---

## 10. Benchmark

### 10.1 Principles

1. **Reproducible**: every run records provider, model, dataset hash, options,
   timestamp, and the raw provider outputs (optionally, gitignored).
2. **Machine-checkable ground truth**: text-based metrics that don't need an LLM.
   An optional LLM-judge is a plug-in, never required for the leaderboard.
3. **Three axes**: accuracy, latency (p50/p95 per page), cost per 1k pages.
4. **Open datasets only**: synthetic documents generated by the repo (so the
   ground truth is exact) plus adapters for public sets that users download
   themselves. Implemented adapters: ParseBench (LlamaIndex), olmOCR-bench (AI2),
   OmniDocBench (OpenDataLab, index only: research-only licence), DP-Bench (Upstage, MIT;
   folded into `combined-v3`). Planned: RealDocBench (Extend) and LongExtractBench (Reducto / micro_1) once public. Licences and
   mappings: `docs/benchmarks/academic-benchmarks.md`, `docs/benchmarks/adapters.md`.
   Vendor-published benchmarks are each won by their publisher; running all of them
   through one harness with one scoring pipeline is the point of the meta-benchmark.
   Each adapter is `benchmark/adapters/<name>.py`, emits a `manifest.json`, and records
   the upstream version/commit so results stay tied to a dataset revision. The combined
   leaderboard reports per-dataset scores and an unweighted mean across datasets.

### 10.2 Dataset format

```
benchmark/datasets/<name>/
  manifest.json          # {name, version, description, license, documents:[…]}
  docs/<id>.<ext>        # input file
  truth/<id>.md          # expected markdown (or .txt for text-only docs)
```

`manifest.documents[]`: `{id, file, truth, pages, tags:[...], category}`.
Categories in the built-in `synthetic-v1` set: `plain`, `invoice`, `table`,
`two_column`, `headings`, `noisy_scan`, `low_res`, `multipage`, `skewed`, `dense`, `faded`,
`receipt`, `complex_table`. The generator (`benchmark/generate_synthetic.py`) is seeded and
byte-reproducible; the truth is produced from the same source the pixels are rendered from.

### 10.3 Metrics (computed in Rust, `puffinparse_core::bench`)

Given predicted `P` and truth `T` after **normalisation** (NFKC, strip markdown syntax and HTML
tags, decode HTML entities, straighten quotes/dashes, collapse whitespace, lowercase for the
`case_insensitive` variant):

- `char_similarity = 1 - levenshtein(P, T) / max(|P|, |T|)` (the primary metric of a plain
  transcript document)
- `cer = levenshtein(P, T) / |T|`
- `wer = word_levenshtein(P_words, T_words) / |T_words|`
- `word_recall` = fraction of truth word tokens present in prediction (bag-of-words)
- `word_precision`, `word_f1`
- `table_score` (when the truth has a table): char_similarity restricted to table rows. Tables are
  markdown pipe tables **or HTML `<table>`s** (thead/tbody, th/td, `colspan`/`rowspan` repeated
  into every slot they cover, entities decoded), each reduced to rows of normalised cells; a row is
  its cells joined by a space. `puffinparse_core::bench::tables`.
- `teds_grid` (when the truth has a table): TEDS (Zhong et al. 2020) computed on the grid tree
  `table > row > cell` — `1 - TED / max(|Tp|, |Tt|)`, Zhang–Shasha tree edit distance, unit
  insert/delete, cell rename = normalised Levenshtein of the contents. It is TEDS without
  `thead`/`tbody` nodes and span attributes, because the ground truth is markdown. Each truth
  table is matched to its best predicted table; extra predicted tables are not penalised.
- `order_score`: Kendall-τ–like agreement of the order of lines shared by both texts

Per document all metrics are recorded, plus the document's **headline**: `table_score` for
`table-only` documents, the rule pass rate for `kind: rules`, `char_similarity` otherwise.
Aggregate = mean over docs (failures count 0), plus per category. `Summary.headline` = mean of the
document headlines (0–1), **Overall** = `100 * headline`; `Summary.char_similarity` is the plain
mean of the documents' `char_similarity` (a rule document has no transcript, so its
`char_similarity` field carries the pass rate).

Rule matching (`score_rules`) normalises both sides with the run's options, then drops every
space adjacent to punctuation, so tokenised rule text (`(this " agreement ")`) matches the printed
page. `bag_of_sentences` counts a sentence as present when a window of the prediction is ≥ 0.8
similar (`BAG_SENTENCE_MIN_SIMILARITY`); the rule passes when at least `threshold` of them are
(default `BAG_DEFAULT_THRESHOLD = 0.8`). A rule's optional `max_diffs` (olmOCR-bench) allows that
many Levenshtein edits in `present` / `absent` / `order` / `table_cell` matching.

The scorer is versioned: `puffinparse_core::bench::SCORER_VERSION` (currently `2`) is written to every
result as `scorer_version` and bumped whenever a change would move a committed score.

### 10.4 Runner and outputs

```
puffinparse bench run --dataset benchmark/datasets/synthetic-v1 \
    --models reducto/standard extend/parse_performance llamaparse/agentic \
    --out benchmark/results/<date>-synthetic-v1.json
puffinparse bench report benchmark/results/*.json --format markdown > benchmark/LEADERBOARD.md
```

Result JSON: `{run_id, created_at, puffinparse_version, scorer_version, rescored_at?,
dataset:{name, version, documents, sha256}, normalize, models:[{model, docs:[{id, category, kind,
table_only, pages, metrics, headline, latency_ms, cost_usd, error}], summary:{documents, failed,
headline, char_similarity, cer, wer, word_f1, order_score, table_score, teds_grid, rule_pass_rate,
overall, latency_p50_ms, latency_p95_ms, latency_per_page_ms, total_pages, total_cost_usd,
cost_per_1k_pages_usd, by_category}}]}`. The `sha256` covers the manifest plus every input, truth
and rule file, so a result is tied to an exact dataset revision.

Consumers (the viewer in `benchmark/site/`, `bench report`) rank on `summary.headline` (0–1) when
present, else `overall / 100`, and show a document's `headline` when present. Compatibility with
older files: a file without `scorer_version` is scorer v1; its `summary.headline` is read as
`overall / 100`, `teds_grid` is absent, and — unlike v2 — its `summary.char_similarity` held the
headline (table-only documents contributed `table_score`). `puffinparse bench rescore <result.json>
--outputs <dir> [--dataset <dir>] [--out <path>]` re-scores a run from its saved per-document
outputs with the current scorer, without network access: metrics, headlines and summaries are
recomputed, `kind`/`table_only`/`category` are refreshed from the manifest, latency, cost, pages
and errors are kept as measured, `scorer_version` and `rescored_at` are set, and the dataset
`sha256` is updated (with a warning) if the dataset changed since the run. A missing output for a
successful document is an error unless `--keep-missing` is given, which keeps that document's
recorded scores and records how many in `rescore_kept_docs` (used for sources whose outputs are
not committed, e.g. research-only datasets). A successful call that returned only whitespace is
flagged `empty_output: true` and counted in `summary.empty_outputs`; it is scored, not failed.

Each document record also carries audit fields (all optional, so older result files still load):
`provider_job_id` (the provider's id for the call — Reducto job id, Extend parse run id,
LlamaParse job id — taken from `ParseResponse.provider_job_id`, or from `Error.job_id` for a
failure), `cache_hit` (`true` if the provider reported a result-cache hit via a
`<provider>_cache_hit` metadata key, `false` if caches were disabled for the run, `null` unknown;
always written), `attempts` (calls the runner issued, >1 only with `--retries`; retries inside the
HTTP client are not counted), `started_at` (RFC 3339, first attempt) and, for failures,
`error_kind` (the `ErrorKind` serde name, `"input"` for an unreadable truth/rule file) next to the
`error` message.

Resilience: while running, every finished (model, document) record is appended and flushed to
`<out>.partial.jsonl` (first line `{"type":"header", run_id, created_at, dataset_sha256,
normalize}`, then `{"type":"doc", model, doc}` per call). The final JSON is written from those
records and the log is then deleted. `bench run --resume` reads the result JSON and/or the partial
log, refuses them if the dataset `sha256` or normalisation differs, keeps the original `run_id`,
and calls only the pairs without a successful record. A leftover log without `--resume` is an
error, never silently overwritten. `--dry-run` prints the plan (calls, manifest pages, list-price
estimate per model) without network access; `--max-cost <usd>` aborts before the first call when
the estimate exceeds it (or when a planned model has no list price). The run ends with one line:
calls made, resumed, failed, total cost, this invocation's cost and wall time.

`LEADERBOARD.md` is regenerated from committed results and links to each run.

The site publishes every committed result file, the run index and each model's saved outputs as a
read-only JSON API under `/benchmark-results/data/` (`index.json`, `runs/<run_id>.json`,
`outputs/<run_id>/<model_slug>/<doc>.{md,json}`, `datasets/<name>/manifest.json`), described by
the OpenAPI 3.1 document `/openapi.json` (schemas in `website/data_api.py`) and listed in the RFC
9727 catalog `/.well-known/api-catalog`. A change to the result JSON updates those schemas too.

Document kinds: a manifest document is `kind: "transcript"` (default; `truth` markdown, scored by
the text metrics) or `kind: "rules"` (a `rules` file of machine-checkable assertions — `present`,
`absent`, `order`, `table_cell`, `bag_of_sentences` — scored by `puffinparse_core::bench::score_rules`,
reported as `rule_pass_rate` / `rules_passed` / `rules_total` in `Metrics` and `rule_pass_rate` in
`Summary`). Documents tagged `table-only` are headlined by `table_score`. Result JSON documents
carry `kind` and `table_only`; rule files are included in the dataset `sha256`.

---

## 11. Python SDK details

- `python/puffinparse/__init__.py` exports the three modes — `parse`/`aparse`, `ocr`/`aocr`,
  `extract`/`aextract` — plus `Router`, the response dataclasses (`ParseResponse`, `Page`,
  `Block`, `BBox`, `Usage`, `TextResponse`, `TextPage`, `Line`, `Word`, `ExtractResponse`,
  `FieldInfo`, `Citation`), the `Mode` literal (`"parse" | "ocr" | "extract"`), errors,
  `set_pricing`, `estimate_cost`, `list_models`, `resolve_model`, `providers`, `modes`
  and the callback lists.
- Mode arguments are plain strings everywhere (`list_models("ocr")`,
  `Router([...], mode="ocr")`, `set_pricing({...}, "ocr")`, `estimate_cost(m, 1000, "ocr")`),
  typed as `puffinparse.Mode`.
- Callbacks: `puffinparse.success_callback: list[Callable[[Response], None]]` where `Response` is
  the union of the three response types, and `puffinparse.failure_callback` — both fire for every
  mode, sync and async; awaitables returned by a callback are awaited (on the caller's loop for
  `a*` calls, on a private loop otherwise).
- Bytes never cross the FFI boundary as base64: the Python layer passes the document
  as a separate `bytes` argument and the extension builds `DocumentInput::Bytes`. `extract`
  follows the same convention — the request dict is `ExtractRequest` with the document fields
  flattened into it.
- Errors from a mode mismatch are enriched in Python: an `UnsupportedModelError` from
  `extract`/`ocr` names the mode *and* the models that serve it (`list_models(mode)`), so a
  user who passes a parse-only model to `extract` is told what to use instead. Passing a
  non-dict `schema` raises `TypeError` before any FFI call.
- Jobs (§15): `submit`/`asubmit` → `Job` (dataclass mirroring `JobHandle`),
  `retrieve`/`aretrieve` and `handle_webhook`/`ahandle_webhook` → `Job | ParseResponse`
  (`puffinparse.JobResult`), implemented in `python/puffinparse/jobs.py` over `_core.submit`,
  `_core.retrieve` and `_core.parse_webhook`.
- `puffinparse.score`, `normalize_text`, `markdown_to_text` expose the benchmark metrics.
- Logging: `PUFFINPARSE_LOG=debug` enables tracing in the core; Python uses
  `logging.getLogger("puffinparse")`.
- Typing: fully typed, `py.typed` shipped; dataclasses mirror the Rust structs 1:1.
- Build: maturin, `abi3-py39` wheels, `pip install puffinparse`.

---

## 12. CLI

```
puffinparse parse   <file|url> [--model reducto/standard] [--format markdown|text|json] [--raw]
puffinparse ocr     <file|url> [--model reducto/standard] [--format text|json]
puffinparse extract <file|url> --schema <file.json|inline JSON> [--instructions TEXT] [--citations]
puffinparse providers [--mode parse|ocr|extract] [--json]   # models, modes, per-mode pricing, key status
puffinparse bench run|report|score|rescore                  # see §10 (parse mode; rescore is offline)
puffinparse serve [--config puffinparse.toml] [--host H] [--port P]   # HTTP gateway, see §14
```

One subcommand per mode; `--model` must name a model that serves that subcommand's mode, and a
bare provider name resolves to its default model *for that mode*. All three share the common
options (`--pages`, `--language`, `--options`, `--timeout`, `--max-retries`, `--api-key`,
`--base-url`, `--raw`).

- `parse` prints markdown (default), plain text, or the whole `ParseResponse` as JSON.
- `ocr` prints the plain text (default) or the whole `TextResponse` as JSON, which includes
  per-page `lines[]` and `words[]` with boxes.
- `extract` always prints the `ExtractResponse` as JSON. `--schema` is either a path to a JSON
  file or inline JSON starting with `{`.
- `providers` lists every model with the modes it serves and its price in each mode; `--mode`
  filters the table to one mode and shows a single price column.

Non-JSON output prints a one-line summary to stderr (`[model] N page(s) in T ms, est. $X`).
Exit code 0 on success, 1 on provider error, 2 on usage/config error (including a model that
does not serve the requested mode).

---

## 13. Quality bar

- Rust: `cargo fmt --check`, `cargo clippy -D warnings`, unit tests with
  fixtures for each provider's parser, no network in tests (live tests are
  `#[ignore]` and run only when keys are present).
- Python: `ruff`, `mypy --strict` on the package, `pytest` with a fake core
  for unit tests; live tests skipped without keys.
- CI: GitHub Actions on push/PR (Linux; wheels build matrix on tags).
- Versioning: semver, single workspace version, `CHANGELOG.md` (Keep a Changelog).

---

## 14. Gateway server

`puffinparse serve` (crate `puffinparse-server`) exposes the three modes over HTTP for clients that should
not hold provider keys. Operator reference: [`SERVER.md`](SERVER.md). Contract:

- **Endpoints.** `POST /v1/parse | /v1/ocr | /v1/extract` take the §4 fields (`model`, `pages`,
  `language`, `output`, `output_format`, `provider_options`, `include_raw`, `timeout`,
  `max_retries`, `metadata`; `schema` / `instructions` / `citations` for extract) plus
  `fallbacks: [str]`, as JSON (`document_url`, or base64 `document` + `filename`) or multipart
  (`file` part + the same fields). `api_key`, `base_url` and local paths are rejected. They return
  the §5 response JSON unchanged, or the vendor shape for `output_format` (parse, extract). Also
  `GET /v1/models`, `GET /v1/usage`, `GET /health`, `GET /metrics` (Prometheus text).
- **Jobs (§15).** `POST /v1/jobs` takes the `/v1/parse` body plus `webhook_url` and returns `202
  {id, object: "job", status: "pending", job: JobHandle}` (without `base_url`). Auth, allow-lists,
  aliases, budget pre-check and `rpm` apply as for `/v1/parse`, but an alias submits to its first
  target and `fallbacks` is rejected (no fallback for jobs). `GET /v1/jobs/{id}` checks the
  provider once: `{id, object, status: pending|succeeded|failed, model, provider,
  provider_job_id, submitted_at, result?, error?}`, `result` in the submit-time `output_format`
  (or `?output_format=`), `error` the error object below. `id` is opaque and bound to the
  submitting key (other keys get `404 not_found`; the master key sees all). Cost is charged to
  that key once, when the job is first observed succeeded; polls count toward `rpm`, not the
  budget. Handles (no secrets) live in memory and the `state_file` for `job_retention_hours`.
  Optional `POST /v1/webhooks/{provider}` (`[webhooks] enabled`, shared `secret` as `?token=` or
  `x-puffinparse-webhook-secret`, else 404) resolves a provider webhook body with core
  `parse_webhook` (+ one retrieve when needed) against a stored job and settles it the same way.
- **Config.** One TOML file: `[server]`, `master_key`, `[providers.<name>]` (`api_key`,
  `base_url`), `[[models]]` aliases (`name`, `targets`, `strategy`, `fallback_on`, with the §7
  semantics and per-target credential overrides), `[[keys]]` virtual keys (`id`, `key`, `models`
  allow-list with `provider/*` wildcards, `monthly_budget_usd`, `rpm`). Secrets may be
  `env:VAR`. No master key and no keys means auth is off.
- **Hardening.** Defaults are safe for untrusted callers: no start on a non-loopback host without
  keys (`allow_unauthenticated`); `document_url` for a model that downloads in-process
  (`fetch::fetches_url_in_process`) is a 400 unless `fetch_document_urls`, and such downloads use
  the §4.4 restrictions (`allow_private_document_urls`, `max_download_mb`); self-hosted engines
  named directly need an alias, a key whose `models` names them, the master key, or
  `allow_local_engines`; `/metrics` needs a key unless `public_metrics`; at most
  `max_concurrent_requests` document requests in flight, `request_timeout_secs` (default
  `max_timeout_secs` + 60) per request, `header_read_timeout_secs` for headers.
- **Accounting.** Spend = response `cost_usd`, per key per UTC calendar month, checked before each
  call (`402` once spent ≥ budget); `rpm` is a sliding 60 s window (`429` + `Retry-After`). State
  is in memory, optionally persisted to a JSON `state_file`.
- **Errors.** One body shape, `{"error": {type, message, provider, provider_status, job_id,
  request_id}}`. `ErrorKind` → HTTP: input / bad_request / unsupported_model → 400, rate_limit →
  429, timeout → 504, provider / network / authentication → 502 (provider credentials are the
  operator's). Gateway-own types: `unauthorized` 401, `budget_exceeded` 402, `model_not_allowed`
  403, `not_found` 404, `payload_too_large` 413, `key_rate_limited` 429. The provider's message is
  passed through.
- **Logs.** One JSON line per request: `ts, request_id, key_id, method, path, mode, model,
  served_model, provider, fallback_index, pages, cost_usd, latency_ms, status, error_type,
  provider_status`, plus `job_id, job_status` on jobs API lines. Never document content or URLs,
  provider error text, or any secret. Metrics label jobs traffic `mode="job_submit" |
  "job_retrieve" | "webhook"` and count `puffinparse_jobs_total{event}`.

## 15. Asynchronous jobs and webhooks

`parse` blocks until the provider is done (polling job-queue providers internally). The jobs API
splits that call in two so the caller owns the waiting — for long documents, large batches, or
pipelines driven by provider webhooks. An SDK cannot *receive* a webhook, so PuffinParse exposes the
primitives and a parser for webhook bodies; your web handler does the receiving. `parse` mode only.

```rust
puffinparse_core::submit_parse(DocumentRequest) -> Result<JobHandle>
puffinparse_core::retrieve_parse(&JobHandle) -> Result<JobStatus>            // env credentials
puffinparse_core::retrieve_parse_with(&JobHandle, &RetrieveOptions) -> Result<JobStatus>
puffinparse_core::parse_webhook(model, &serde_json::Value) -> Result<WebhookEvent>   // no network
puffinparse_core::resolve_webhook(model, &Value, &RetrieveOptions) -> Result<JobStatus>
```

```python
job = puffinparse.submit("big.pdf", model="reducto/standard", webhook_url="https://…/hook")  # -> Job
puffinparse.retrieve(job)                    # -> Job (still running) | ParseResponse; raises on failure
puffinparse.handle_webhook(body, model="reducto")   # -> Job | ParseResponse; raises on failure
# asubmit / aretrieve / ahandle_webhook are the async equivalents
```

```ts
const job = await submit('big.pdf', { model: 'reducto/standard', webhookUrl })  // -> Job (camelCase JobHandle)
await retrieve(job, { apiKey?, outputFormat? })   // -> the same Job | ParseResponse; rejects on failure
await handleWebhook(body, { model: 'reducto' })   // -> Job | ParseResponse; rejects on failure
```

Over HTTP the gateway exposes the same pair as `POST /v1/jobs` / `GET /v1/jobs/{id}` (§14).

**`JobHandle`** (`Job` in Python): `provider`, `model` (qualified), `job_id` (the provider's id),
`submitted_at` (RFC 3339), `output`, `include_raw`, `base_url`, `provider_state`, `metadata`.
It never contains a secret: API keys are resolved again at retrieve time (env var or
`RetrieveOptions.api_key` / `retrieve(api_key=…)`), and `provider_state` holds only the
non-secret options a later call needs (Extend `workspace_id`, `responseType`). Serialise it with
serde / `Job.to_dict()` to store it or hand it to another process.

**`JobStatus`**: `Pending` | `Succeeded(ParseResponse)` | `Failed(Error)`. A succeeded job is
normalised exactly like `parse` (qualified model, cost, request metadata echoed, `raw` only with
`include_raw`); `latency_ms` is the time since submission. A provider-side failure is
`Ok(Failed(e))` with `e.job_id` set — `Err` means the status check itself failed (auth, network).
Python raises the typed exception for `Failed`.

**Webhooks.** `webhook_url` maps to each provider's own per-job setting; `parse_webhook` reads
the body the provider POSTs:

| Provider | Submit / retrieve | `webhook_url` → | Webhook body → `WebhookStatus` |
|---|---|---|---|
| `reducto` | `POST /parse_async` / `GET /job/{id}` | `async.webhook = {"mode": "direct", "url": …}` | `{"status", "job_id"}`: `Completed`/`Failed` → `Finished` (retrieve for result or reason), else `Pending` |
| `extend` | `POST /parse_runs` / `GET /parse_runs/{id}` | **rejected** (`input` error): Extend only has workspace webhook endpoints | `{"eventType": "parse_run.*", "payload": parse_run_status}`: `PROCESSED` → `Finished` (or `Succeeded` if a full run with output), `FAILED` → `Failed` (reason + message), else `Pending` |
| `llamaparse` | `POST /api/v1/parsing/upload` / `GET /api/v1/parsing/job/{id}` (+ `result/json`) | multipart field `webhook_url` | the `webhook_url` result push `{"txt","md","json":[pages]}` → `Succeeded`; a LlamaCloud event `{"event_type": "parse.*", "data": {"job_id"}}` → `Finished` / `Pending` |

`WebhookStatus::Finished` means "terminal, but the body carries neither the result nor the error
detail"; `resolve_webhook` (Python `handle_webhook`) then makes one retrieve. Verifying webhook
authenticity (Extend and LlamaCloud HMAC signatures, a secret in Reducto `async.metadata`) is the
caller's job and must happen before the body is trusted. Other providers return
`unsupported_model` from `submit_parse`.
