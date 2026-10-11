# Extend

> **Status: live-verified.** The `#[ignore]`d live tests pass against the real Extend API,
> and its fixtures under `crates/puffinparse-core/tests/fixtures/` include redacted live responses.

## 1. Summary

| | |
|---|---|
| Provider name | `extend` |
| Base URL | `https://api.extend.ai` (override: `base_url` on the request, or `EXTEND_BASE_URL`). Regional hosts: `https://api.us2.extend.app` (note `.app`), `https://api.eu1.extend.ai` |
| API key | `EXTEND_API_KEY` (or `api_key` on the request) — sent as `Authorization: Bearer <key>` |
| Docs | <https://docs.extend.ai> (append `.md` to any docs path for raw markdown; index at `llms.txt`) |
| API version | **`2026-02-09`**, pinned by PuffinParse in the mandatory `x-extend-api-version` header (`extend::API_VERSION`) |
| Verified | 2026-09-11, live against the production host |
| Implementation | `crates/puffinparse-core/src/providers/extend.rs` |

Extend returns page chunks of markdown plus typed blocks with polygons, bounding boxes and OCR
confidence. PuffinParse always uses the **async** run API (`/parse_runs`), never the 5-minute sync `/parse`.

## 2. Models exposed by PuffinParse

| Model | Provider parameters PuffinParse sets | List price (`pricing.json`) |
|---|---|---|
| `extend/parse_performance` *(default)* | `config.engine = "parse_performance"` | $0.025 / page (2 credits) |
| `extend/parse_light` | `config.engine = "parse_light"` | $0.00625 / page (0.5 credits) |
| `extend/parse_auto` | `config.engine = "parse_auto"` | $0.025 / page (per-page: light pages bill 0.5 credits) |
| `extend/extraction_performance` *(default for `extract`)* | `config.baseProcessor = "extraction_performance"`, `config.parseConfig.engine = "parse_performance"` | $0.0625 / page (3 + 2 credits) |
| `extend/extraction_light` | `config.baseProcessor = "extraction_light"`, `config.parseConfig.engine = "parse_light"` | $0.015 / page (0.7 + 0.5 credits) |

The first three models serve `parse` and `ocr`; the last two serve `extract` only (§5). An extract run
triggers its own parse run and is billed for both, which is why the extract prices are the sum of the
two line items; re-extracting a file Extend has already parsed bills only the extraction.

Credits are the billing unit; pay-as-you-go is $0.0125/credit (Scale: $0.01). Prices above are the PAYG
list rate from <https://docs.extend.ai/general/how-credits-work>, used for `ParseResponse.cost_usd`. `parse_auto` is priced
at the performance rate, so its estimate is an upper bound. Surcharges PuffinParse does not model: agentic
text/table correction +1 credit per triggered page, priority parsing ×2, advanced Excel parsing
3 credits / 1 000 non-empty cells.

## 3. Request flow PuffinParse uses

Every request carries:

```
Authorization: Bearer $EXTEND_API_KEY
x-extend-api-version: 2026-02-09
x-extend-workspace-id: <provider_options.workspace_id>      # only when supplied
```

1. **Upload (only for path / bytes input).** `POST {base}/files/upload`, `multipart/form-data`, single
   part `file`. Response: a file object whose `id` (`file_…`) is used as `{"id": …}`.
   **URL inputs skip this step** — PuffinParse sends `{"url": "<url>", "name": "<filename>"}` and Extend
   downloads the file itself, creating a `file_…` record it returns in `run.file`.
2. **Create the run.** `POST {base}/parse_runs` with `Content-Type: application/json`. Body from
   `build_body()`:

   ```json
   {
     "file": { "id": "file_FAKEparseFile00000001" },
     "config": {
       "target": "markdown",
       "chunkingStrategy": { "type": "page" },
       "engine": "parse_performance",
       "blockOptions": { "tables": { "targetFormat": "markdown" } }
     }
   }
   ```

   * `pages="1-3,7"` adds `config.advancedOptions.pageRanges = [{"start":1,"end":3},{"start":7,"end":750}]`
     — Extend requires an `end`, and 750 is its documented maximum page-range end.
   * `targetFormat: "markdown"` is deliberate: Extend's default is `"html"`.
   * The response echoes the **resolved** config (including `engineVersion`, e.g. `"2.0.0"`), which is
     what PuffinParse reads back to label the response model.
3. **Poll.** `GET {base}/parse_runs/{id}` starting at 1 s, backing off ×1.5 to a maximum of 10 s, until
   `status` is `PROCESSED` or `FAILED`. (`PENDING`/`PROCESSING` keep polling.) If the create call already
   came back terminal, the poll loop is skipped.
4. **Result.** Normally `run.output` is inline. `responseType` is a **query parameter of
   `GET /parse_runs/{id}`** (not a body key): with `provider_options={"responseType": "url"}` PuffinParse
   consumes the key (it is never sent in the create body) and polls
   `GET /parse_runs/{id}?responseType=url`. The finished run then has `output: null` and an
   `outputUrl`, which PuffinParse fetches with a plain `GET` and **no** auth header; the payload has
   exactly the shape of `output` (`{chunks, metadata, ocr?}`). Presigned output URLs expire in
   15 minutes. (Before 2026-09-24 PuffinParse forwarded the key into the body, so this path never
   triggered.) Verified live on 2026-09-24; the captured run and output are the fixtures
   `extend_parse_run_url.json` / `extend_parse_run_url_output.json` (ids and signatures redacted),
   replayed by the loopback tests in `providers::extend::wire`.

**Where `provider_options` are merged:** `build_body()` removes `workspace_id` (it becomes a header)
and `responseType` (a query parameter, step 4),
then lifts any of `target`, `chunkingStrategy`, `engine`, `engineVersion`, `blockOptions`,
`advancedOptions` into `config` (deep-merged with an explicit `config` object if you passed one), and
deep-merges everything that remains at the **top level** of the body — which is how `metadata`,
`dataRetention` and even `file` overrides get through. So both spellings work:
`{"blockOptions": {...}}` and `{"config": {"blockOptions": {...}}}`.

### Jobs API and webhooks (`submit_parse` / `retrieve_parse`, SPEC §15)

* **Submit** — the same file reference and `POST {base}/parse_runs` as `parse`; the run id
  (`pr_…`) becomes `JobHandle.job_id`. `workspace_id` and `responseType` are kept in
  `JobHandle.provider_state` so retrieve sends the same header and query parameter.
* **Retrieve** — one `GET {base}/parse_runs/{id}` (retried on 429/5xx). `PROCESSED` → the output
  (inline or via `outputUrl`); `FAILED` / `CANCELLED` → `JobStatus::Failed` with
  `failureReason: failureMessage` (kinds as in §6); `PENDING` / `PROCESSING` → `Pending`.
* **`webhook_url` is rejected** (`input` error, no request sent). Extend has no per-run webhook:
  webhooks are workspace **endpoints** (`POST /webhook_endpoints` or the dashboard) subscribed to
  events such as `parse_run.processed` / `parse_run.failed`. Their body is
  `{"eventId", "eventType", "payload": {"object": "parse_run_status", "id", "status",
  "failureReason", "failureMessage", "metadata"}}`; `parse_webhook` maps `FAILED` straight to
  `Failed` (the reason is in the body), `PROCESSED` to `Finished` (then one retrieve), anything
  else to `Pending`. Endpoints configured for *signed download URL* delivery send
  `payload: {"data": "<url>"}` — download it and pass the JSON inside. Verify the
  `HMAC-SHA256(v0:{timestamp}:{body})` signature before trusting a body. Non-`parse_run.*`
  events are rejected.
* Verified live 2026-09-24 (`tests/live_jobs.rs::extend_submit_retrieve_live`, `parse_light`,
  1 page: 3 status checks, ~4.4 s).

## 4. Response mapping (`parse` / `ocr`)

| Extend field | PuffinParse unified field | Notes |
|---|---|---|
| `id` (`pr_…`) | `ParseResponse.provider_job_id` | |
| `config.engine` | `ParseResponse.model` | `extend/<engine>` read back from the resolved config; falls back to `parse_performance`. |
| `output.chunks[].content` | `Page.markdown` | Only for chunks with `type == "page"` and `pageRange.start == pageRange.end`. |
| `output.chunks[].blocks[]` | `Page.blocks[]` | Reading order preserved. |
| `blocks[].type` | `Block.type` | See mapping below. |
| `blocks[].content` | `Block.content` | Markdown; `output="text"` runs it through `markdown_to_text`. |
| `blocks[].metadata.page.number` | `Block.page_number` | Falls back to the chunk's `pageRange.start`. |
| `blocks[].metadata.page.{width,height}` | `Page.width` / `Page.height` | First value seen per page wins; these are the *raster* dimensions (e.g. 1241×1754 at 150 dpi), not PDF points. |
| `blocks[].boundingBox.{left,top,right,bottom}` | `Block.bbox` = `{x0,y0,x1,y1}` | **Absolute, top-left origin, page units.** Normalised by the page's `width`/`height` via `from_xywh(left, top, right-left, bottom-top, w, h)` and clamped to 0–1. No page dims for that page ⇒ `bbox = None`. |
| `blocks[].metadata.avgOcrConfidence` | `Block.confidence` | 0–1 float. `minOcrConfidence` and chunk-level confidences are ignored. |
| `metrics.pageCount` | `Usage.pages` | Rounded; falls back to the number of reconstructed pages. |
| `usage.credits` | `Usage.credits` | `null` for runs created before 2025-10-07. `totalCredits`/`breakdown` are not surfaced. |
| `metrics.processingTimeMs` | `metadata.extend_processing_time_ms` | |
| `blocks[].polygon`, `output.ocr.words[]`, `output.metadata.pages[]` | — | Not mapped; visible with `include_raw=True`. |

Block types: `text`, `key_value` → `text`; `heading` → `title`; `section_heading` → `section_header`;
`table`, `table_head`, `table_cell` → `table`; `figure` → `figure`; `formula` → `formula`;
`header` → `header`; `footer` → `footer`; everything else (`page_number`, `barcode`, …) → `other`.

Trimmed real response (`crates/puffinparse-core/tests/fixtures/extend_parse_run.json`, one chunk with one
block shown; the file holds 2 pages, 5 blocks and 21 OCR words):

```json
{
  "object": "parse_run",
  "id": "pr_FAKEparseRun000000001",
  "file": { "object": "file", "id": "file_FAKEparseFile00000001", "name": "test_multi.pdf",
            "type": "PDF", "parentFileId": null, "metadata": { "pageCount": 2 },
            "dataRetention": { "mode": "workspace_default", "status": "available" },
            "createdAt": "2026-09-11T07:18:23.026Z", "updatedAt": "2026-09-11T07:18:37.514Z" },
  "status": "PROCESSED",
  "failureReason": null,
  "failureMessage": null,
  "metadata": null,
  "dataRetention": { "mode": "workspace_default", "status": "available" },
  "output": {
    "chunks": [
      {
        "object": "chunk", "id": "chunk_1_iSMg4N", "type": "page",
        "content": "# Hello PuffinParse\n\nInvoice #1234\nTotal: $56.78\nDate: 2026-09-11\n\n| Item | Amount |\n| --- | --- |\n| Widget | $56.78 |",
        "metadata": { "pageRange": { "start": 1, "end": 1 },
                      "minOcrConfidence": 0.905, "avgOcrConfidence": 0.986 },
        "blocks": [
          {
            "object": "block", "id": "block_1_YFFbD5", "type": "table",
            "content": "| Item | Amount |\n| --- | --- |\n| Widget | $56.78 |",
            "details": { "type": "table_details", "rowCount": 2, "columnCount": 2 },
            "metadata": { "page": { "number": 1, "width": 1241, "height": 1754 },
                          "minOcrConfidence": 0.905, "avgOcrConfidence": 0.973 },
            "polygon": [ { "x": 90.365, "y": 382.357 }, { "x": 704.950, "y": 382.357 },
                         { "x": 704.950, "y": 585.043 }, { "x": 90.365, "y": 585.043 } ],
            "boundingBox": { "left": 90.365, "top": 382.357, "right": 704.950, "bottom": 585.043 }
          }
        ]
      }
    ],
    "metadata": { "originalMimeType": "application/pdf", "finalMimeType": "application/pdf",
                  "pages": [ { "number": 1, "rotationApplied": 0, "originalPageWidth": 596,
                               "originalPageHeight": 842, "dpi": 150 } ] }
  },
  "outputUrl": null,
  "metrics": { "processingTimeMs": 4383, "pageCount": 2 },
  "config": { "target": "markdown", "chunkingStrategy": { "type": "page" },
              "engine": "parse_performance", "engineVersion": "2.0.0",
              "blockOptions": { "tables": { "targetFormat": "markdown" }, "figures": { "enabled": true } } },
  "batchId": null,
  "usage": { "credits": 4, "totalCredits": 4,
             "breakdown": [ { "object": "parse_run", "id": "pr_FAKEparseRun000000001", "credits": 4,
                              "charges": [ { "product": "parse_performance", "unit": "page",
                                             "quantity": 2, "credits": 4 } ] } ] }
}
```

## 5. Extract mode (`extract`)

`puffinparse.extract(...)` reuses the file reference from §3 (URL passed through as `file.url`, everything
else uploaded to `POST /files/upload`) and then runs the async pair
`POST {base}/extract_runs` → poll `GET {base}/extract_runs/{id}`, with the same
`x-extend-api-version: 2026-02-09` (and optional `x-extend-workspace-id`) headers. Terminal statuses
are `PROCESSED`, `FAILED`, `CANCELLED`. The sync `POST /extract` is not used: Extend documents it as
onboarding-only and caps it at 5 minutes. Implementation: `Extend::extract` in
`crates/puffinparse-core/src/providers/extend.rs`.

Body built by `build_extract_body()`:

```json
{
  "file": { "id": "file_…" },
  "config": {
    "baseProcessor": "extraction_performance",
    "schema": { "...the request schema, adapted (see below)..." },
    "parseConfig": { "engine": "parse_performance" },
    "extractionRules": "…only when `instructions` is set…",
    "advancedOptions": { "citationsEnabled": true, "pageRanges": [{ "start": 1, "end": 3 }] }
  }
}
```

* `ExtractRequest.instructions` → `config.extractionRules` (natural-language guidance).
* `ExtractRequest.citations = true` → `config.advancedOptions.citationsEnabled`. This also switches on
  `ocrConfidence`, which is **absent** otherwise, and adds latency (a separate citation model runs).
  Granularity knobs (`citationMode: line|word|block`, `arrayCitationStrategy: item|property`) are
  available through `provider_options`.
* `pages` → `config.advancedOptions.pageRanges` (open-ended ranges end at Extend's 750-page cap).
* `provider_options` keys `baseProcessor`, `baseVersion`, `extractionRules`, `schema`,
  `advancedOptions`, `parseConfig` are merged into `config`; anything else (`metadata`,
  `dataRetention`, `extractor`) is merged at the top level — so a saved extractor is reachable with
  `provider_options={"extractor": {"id": "ex_…"}}`.

### Schema adaptation (mandatory)

Extend rejects plain JSON Schema with `400 INVALID_REQUEST`. `adapt_schema()` rewrites the request
schema before sending it:

| PuffinParse input | Sent to Extend | Why |
|---|---|---|
| `{"type": "string"}` | `{"type": ["string", "null"]}` | *"Non-nullable primitive type "string" is not allowed."* Applies to `string`, `number`, `integer`, `boolean`. |
| `{"type": "array", "items": {"type": "string"}}` | unchanged | Primitive **array items** must stay non-nullable — the documented exception. |
| `{"enum": ["a", "b"]}` | `{"enum": ["a", "b", null]}` | Enums must offer a `null` option (a real JSON `null` here, unlike the `"null"` *string* used in a type union). |

Everything else is passed through, including `required`, `description`, `extend:type`, `extend:name`.
Unsupported constructs (`anyOf`/`oneOf`/`allOf`, `$ref`, `const`, regex/format validation, nesting
deeper than 5) are *not* rewritten and will be rejected by the API.

### Response mapping

`output` has two halves sharing the same field paths: `output.value` (the data) and `output.metadata`
(per-field confidence + citations).

| Extend field | PuffinParse unified field | Notes |
|---|---|---|
| `output.value` | `ExtractResponse.data` | Exactly the schema shape. |
| `output.metadata["line_items[0].amount"]` | `ExtractResponse.fields["/line_items/0/amount"]` | Extend's path notation is converted to an RFC 6901 pointer; `~` and `/` inside names are escaped. Array *containers* (`line_items`, `line_items[0]`) get their own entries and are kept. |
| `metadata[].ocrConfidence` | `FieldInfo.confidence` | Falls back to `logprobsConfidence`, which is being phased out (`null` on `extraction_light` and on `extraction_performance ≥ 4.6.0`). |
| `metadata[].citations[].page.number` | `Citation.page_number` | 1-based. |
| `metadata[].citations[].polygon[]` | `Citation.bbox` | The polygon's axis-aligned bounds, normalised by `page.width`/`page.height` (page pixels). No page dimensions ⇒ `bbox: None`. |
| `metadata[].citations[].referenceText` | `Citation.text` | |
| `usage.breakdown[].charges[]` where `unit == "page"` | `Usage.pages` | Max `quantity` over the charges; falls back to `file.metadata.pageCount`, then the highest cited page, then 1. |
| `usage.totalCredits` (else `usage.credits`) | `Usage.credits` | `totalCredits` includes the parse run the extraction triggered. |
| `parseRunId` / `dashboardUrl` / `reviewed` | `metadata.extend_parse_run_id` / `extend_dashboard_url` / `extend_reviewed` | |
| `id` | `ExtractResponse.provider_job_id` | `exr_…` |
| `failureReason` + `failureMessage` | error message | `OUT_OF_CREDITS → authentication`; `INTERNAL_ERROR`, `FAILED_TO_PROCESS_FILE`, `PARSING_ERROR`, `PRE_`/`POST_PROCESSING_FAILURE` → `provider`; everything else (`INVALID_CONFIGURATION`, `SCHEMA_GENERATION_FAILED`, …) → `bad_request`. |

`reviewAgentScore` and `insights` (model reasoning) are not surfaced; enable them through
`provider_options` and read `response.raw`.

Trimmed real response (`crates/puffinparse-core/tests/fixtures/extend_extract_run.json`):

```json
{
  "object": "extract_run",
  "id": "exr_FAKEextractRun0000001",
  "status": "PROCESSED",
  "output": {
    "value": { "invoice_number": "INV-9865", "total": "$14,667.43", "vendor": "Cedar Ridge Supply",
               "line_items": [ { "description": "Hydraulic fluid, 5 gal", "amount": "$439.20" } ] },
    "metadata": {
      "invoice_number": {
        "ocrConfidence": 0.929,
        "logprobsConfidence": null,
        "reviewAgentScore": null,
        "citations": [
          { "fileId": "file_FAKEextractFile000001",
            "page": { "number": 1, "width": 1240, "height": 1754 },
            "referenceText": "Invoice #: INV-9865",
            "polygon": [ { "x": 78, "y": 468 }, { "x": 310, "y": 467 },
                         { "x": 310, "y": 495 }, { "x": 78, "y": 496 } ] }
        ]
      },
      "line_items[0].amount": { "…": "…" }
    }
  },
  "parseRunId": "pr_FAKEextractParse00001",
  "usage": { "credits": 3, "totalCredits": 3,
             "breakdown": [ { "object": "extract_run", "credits": 3,
                              "charges": [ { "product": "extraction_performance", "unit": "page",
                                             "quantity": 1, "credits": 3 } ] } ] }
}
```

**Verified live** on 2026-09-11 with `benchmark/datasets/synthetic-v1/docs/invoice_001.png`:
`{invoice_number: "INV-9865", total: "$14,667.43", date: "2024-08-03", vendor: "Cedar Ridge Supply"}`
on both processors, every field cited on page 1 with `ocrConfidence` 0.93–0.99. A fresh
`extraction_performance` run billed `totalCredits: 5` (3 extract + 2 parse); a second run on the
already-parsed file billed only the extraction. Test: `providers::extend::tests::live_extract`
(`#[ignore]`).

### Extract gotchas

* **Plain JSON Schema is a `400`.** The error names one path at a time
  (*"Path: config.properties.invoice_number"*), so an unadapted schema fails field by field. Note the
  asymmetry PuffinParse handles: type unions use the **string** `"null"`, enums use a real JSON `null`.
* **`metadata` keys are paths, not a tree** — `line_items[0].description`, with an entry for the array
  and for each item as well as each cell.
* **`ocrConfidence` only exists with citations on**; without them a field's entry can be empty.
* **Polygons are not rectangles** (four points, often slightly skewed) and are in page pixels, so the
  page dimensions in the same citation are required to normalise them.
* **Extract implicitly bills a parse run** on a file Extend has not parsed yet; `usage.credits` alone
  understates the job (use `totalCredits`).
* Omitting both `config` and `extractor` makes Extend infer a schema. PuffinParse always sends a schema —
  `extract` is schema-driven by definition — but `provider_options={"config": {"schema": null}}` is not
  a supported way around that; drop to `provider_options={"extractor": …}` instead.

## 6. Errors, status codes, rate limits, timeouts

Extend's error body is `{"code","message","requestId","retryable"}` (sometimes `docUrl`). PuffinParse's
`Error::from_http` uses `message` and classifies by HTTP status:

| Status | Typical `code` | PuffinParse `ErrorKind` |
|---|---|---|
| 400 | `INVALID_REQUEST` (missing version header, bad enum with a `Path: config.target` suffix), `INVALID_CONFIG_OPTIONS`, `UNABLE_TO_DOWNLOAD_FILE`, `FILE_TYPE_NOT_SUPPORTED`, `FILE_SIZE_TOO_LARGE` | `bad_request` |
| 401 | `UNAUTHORIZED` — "Invalid API key." | `authentication` |
| 403 | workspace lacks permission | `authentication` |
| 404 | `NOT_FOUND` — unknown file id, or an endpoint that does not exist | `bad_request` |
| 410 | `ENDPOINT_REMOVED` — e.g. `GET /parser_runs/{id}` | `bad_request` |
| 422 | corrupt / password-protected / conversion failure (sync `/parse` only) | `bad_request` |
| 429 | `RATE_LIMIT_EXCEEDED` (`retryable: true`) | `rate_limit` (retried) |
| 500 | `INTERNAL_ERROR`, OCR/chunking errors | `provider` (retried) |

On `/parse_runs`, only 400/401/403 happen at creation; **processing failures return HTTP 200** with
`status: "FAILED"`. PuffinParse maps `failureReason` itself:

| `failureReason` | PuffinParse `ErrorKind` |
|---|---|
| `OCR_ERROR`, `INTERNAL_ERROR` | `provider` |
| `OUT_OF_CREDITS` | `authentication` |
| anything else (`CORRUPT_FILE`, `PASSWORD_PROTECTED_FILE`, `FILE_TYPE_NOT_SUPPORTED`, `CHUNKING_ERROR`, `FAILED_TO_CONVERT_TO_PDF`, …) | `bad_request` |

The error message is `parse run failed: <reason>: <failureMessage>` and carries the run id as `job_id`.

**Retries.** `max_retries` (default 2), exponential backoff with full jitter, on rate-limit, network and
500/502/503/504 only.

**Rate limits.** Per organization, per category (independent GET / WRITE / RUN buckets), plus a
throughput cap in files/minute: PAYG 10 req/s and 80 files/min; Scale 25+ and 120+; Enterprise 75+ and
300+. Free-form `429`s are retryable. **No `X-RateLimit-*` headers are returned at all** — reactive 429
handling only, though `Retry-After` is sometimes present.

**Timeouts.** `timeout_secs` (default 300) is the whole-call deadline (upload + create + polling +
output download) and also caps each individual request. The async run itself has no server-side timeout;
Extend's sync `/parse` (which PuffinParse does not use) has a hard 5-minute one.

## 7. Gotchas (verified)

* **`x-extend-api-version` is mandatory** for any key created after 2025-04-21 — omitting it is a hard
  `400 INVALID_REQUEST`. Keys older than that silently fall back to the legacy `2024-12-23` behaviour.
  PuffinParse always pins `2026-02-09`; the response echoes the same header back.
* **Endpoints from the old API are gone.** `POST /parse_async` → `404 NOT_FOUND`;
  `GET /parser_runs/{id}` → `410 ENDPOINT_REMOVED` ("Use GET /parse_runs/:id instead").
  Async is `POST /parse_runs`.
* **Enums are lowercase.** `"target": "MARKDOWN"` is a `400`; it must be `"markdown"` (or `"spatial"`).
* **The run object *is* the response** on `2026-02-09` — there is no `parserRun`/`success` envelope, and
  chunks live at `output.chunks`, not `chunks`.
* **`blockOptions.tables.targetFormat` defaults to `html`**, which surprises anyone expecting markdown
  end to end. PuffinParse sets `markdown` explicitly.
* **`details` is often the literal empty object `{}`** (text, heading, footer blocks) even though the
  docs describe a tagged union. Anything reading `details.type` must tolerate its absence.
* **`output.metadata.pages` is `null` for raw images** — only PDFs (and files converted to PDF) get page
  metadata. For images, block `metadata.page.width/height` are raw pixels, so PuffinParse still normalises
  boxes correctly.
* **Undocumented-but-always-present keys**: `dataRetention` on both file and run objects, `chunk.id`,
  `block.id`.
* **Coordinates are in the raster space at `dpi`**, not PDF points: a 596×842 pt page reports
  1241×1754 at 150 dpi. Normalising by `metadata.page.width/height` (what PuffinParse does) is correct in
  either space; converting back to source points needs `originalPageWidth × dpi/72` and the inverse of
  `rotationApplied`.
* **`file.metadata` is `{}` right after upload** even for PDFs; `pageCount` only appears once a run has
  processed the file.
* **`usage` is `null` for runs created before 2025-10-07**, and `totalCredits`/`breakdown` are missing on
  runs persisted before 2026-05-14 — all three are optional.
* **The docs site moved.** `https://docs.extend.ai/2025-04-21/developers/api-reference/…` URLs now 404;
  current docs live at the root.

## 8. Useful `provider_options` passthrough

```python
# 1. Organization-scoped API keys need a workspace; PuffinParse turns this into a header.
puffinparse.ocr("doc.pdf", model="extend/parse_performance",
            provider_options={"workspace_id": "ws_…"})

# 2. Word-level OCR boxes + confidence in the raw payload.
puffinparse.ocr("scan.pdf", model="extend/parse_performance", include_raw=True,
            provider_options={"advancedOptions": {"returnOcr": {"words": True}}})

# 3. Skip figure processing, and let the table agent fix messy tables.
puffinparse.ocr("report.pdf", model="extend/parse_performance",
            provider_options={"blockOptions": {"figures": {"enabled": False},
                                               "tables": {"agentic": {"enabled": True}}}})

# 4. Password-protected PDF by URL, tagged for usage reporting, with no data retained.
puffinparse.ocr("https://example.com/locked.pdf", model="extend/parse_light",
            provider_options={"file": {"settings": {"password": "…"}},
                              "metadata": {"extend:usage_tags": ["prod"]},
                              "dataRetention": {"mode": "zero"}})

# 5. Spreadsheets: advanced parsing, hidden content skipped.
puffinparse.ocr("book.xlsx", model="extend/parse_performance",
            provider_options={"advancedOptions": {"excelParsingMode": "advanced",
                                                  "excelSkipHiddenContent": True}})
```

## 9. Links

* Docs home: <https://docs.extend.ai> · index: <https://docs.extend.ai/llms.txt> ·
  compact platform context: <https://docs.extend.ai/agents.md>
* Credits and pricing: <https://docs.extend.ai/general/how-credits-work>
* API versions in use: `2026-02-09` (current), `2025-04-21`, `2024-12-23`, `2024-11-14`, `2024-07-30`,
  `2024-02-01` — pin one with `x-extend-api-version`.
* Webhooks for `parse_run.processed` / `parse_run.failed` (HMAC-SHA256 over `v0:{timestamp}:{body}`)
  are the recommended alternative to polling at scale.
