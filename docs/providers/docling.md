# Docling (docling-serve, self-hosted)

> **Status: verified locally** (a self-hosted engine, so there is no hosted API to verify against).
> docling-serve 1.35.0 (docling 2.130.0, docling-core 2.98.0)
> was installed with `pip install docling-serve` (CPU torch) and run with `docling-serve run` in
> the development sandbox on 2026-09-24. The `#[ignore]`d live test in `providers/docling.rs`
> passes, and both fixtures (`docling_multipage.json` for `multipage_001.pdf`,
> `docling_headings.json` for `headings_001.png`, from `benchmark/datasets/synthetic-v1`) are real
> `GET /v1/result/{task_id}` responses from that server.

## 1. Summary

| | |
|---|---|
| Provider name | `docling` |
| Runs | on your own [docling-serve](https://github.com/docling-project/docling-serve) (IBM's open-source Docling as an HTTP service) |
| Base URL | `http://localhost:5001` (override: `base_url` on the request, or `DOCLING_BASE_URL`) |
| API key | none by default. If the server runs with `DOCLING_SERVE_API_KEY`, set `DOCLING_API_KEY` (or `api_key`); it is sent as `X-Api-Key` |
| Price | $0 per page (`pricing.json` source `self-hosted`); the cost is your compute |
| API version | docling-serve v1 (`/v1/...`); the older `/v1alpha` paths and the `file_sources` / `http_sources` body fields are gone — current servers want `sources: [{kind, ...}]` |
| Implementation | `crates/puffinparse-core/src/providers/docling.rs` |

Start a server:

```bash
docker run -p 5001:5001 quay.io/docling-project/docling-serve        # or docling-serve-cpu / -cu128
# without Docker (≈2.3 GB with CPU-only torch; models download on first use):
pip install docling-serve --extra-index-url https://download.pytorch.org/whl/cpu
docling-serve run --port 5001
```

Docling runs a layout model, TableFormer for table structure and an OCR engine for bitmap text,
all locally. On a 4-core CPU a one-page image took ~30 s and a 2-page digital PDF ~20 s
(first-request model loading excluded); a GPU image is much faster.

## 2. Models exposed by PuffinParse

| Model | Modes | List price |
|---|---|---|
| `docling/default` *(default)* | `parse`, `ocr` (derived) | $0 |

`default` is docling's standard pipeline with the server's defaults (OCR on, table structure
`accurate`). Other pipelines and presets (`pipeline: "vlm"`, `ocr_preset`, `table_mode: "fast"`)
are reachable through `provider_options`.

## 3. Request flow PuffinParse uses

Asynchronous, because the synchronous `/v1/convert/source` is capped by the server's
`DOCLING_SERVE_MAX_SYNC_WAIT` (120 s by default) and long documents exceed it.

1. `POST {base}/v1/convert/source/async`, JSON:

   ```json
   {
     "options": {
       "to_formats": ["json"],
       "image_export_mode": "placeholder",
       "include_images": false,
       "page_range": [first, last],
       "ocr_lang": ["<language>"]
     },
     "sources": [{"kind": "file", "base64_string": "<base64>", "filename": "<name>"}]
   }
   ```

   URL inputs are downloaded by PuffinParse (public addresses only, 50 MiB cap; see
   [SECURITY.md](../../SECURITY.md#document-urls)) and sent as a `file` source: docling-serve is
   never asked to fetch a URL itself.
   `page_range` is only set when `pages` is given (docling takes one span; PuffinParse requests the
   span covering the selection and drops the other pages); `ocr_lang` only when `language` is set.
   `provider_options` are deep-merged into `options`.
   → `{"task_id", "task_status": "pending", "task_position", ...}`
2. `GET {base}/v1/status/poll/{task_id}` every 0.5 s growing to 5 s until `task_status` is
   `success` / `partial_success` / `failure` / `skipped`.
3. `GET {base}/v1/result/{task_id}` → `ConvertDocumentResponse`:
   `{document: {filename, md_content, json_content, ...}, status, errors[], processing_time, timings, confidence}`.

## 4. Response mapping

`json_content` is a
[DoclingDocument](https://docling-project.github.io/docling/concepts/docling_document/):
`body.children` (and `furniture.children`) are the reading order as `{"$ref": "#/texts/3"}`
pointers into `texts[]`, `tables[]`, `pictures[]` and `groups[]`. PuffinParse walks `body` depth-first
through groups, so blocks come out in docling's reading order.

| DoclingDocument | Unified block |
|---|---|
| `texts[]` label `title` | `title`, `# text` |
| `section_header` (with `level`) | `section_header`, `#` × (level + 1) — matches docling's own markdown (`##` for level 1) |
| `text`, `paragraph`, `reference`, `checkbox_*` | `text` |
| `list_item` (inside a `list` group) | `list`, one block per item, `- text` (or its `marker` when `enumerated`) |
| `caption` / `footnote` | `caption` / `footnote` |
| `page_header` / `page_footer` (in `furniture`) | `header` / `footer`, placed first / last on their page |
| `formula` | `formula`, `$$ … $$` |
| `code` | `other`, fenced |
| `tables[]` | `table`; markdown built from `data.grid` (spans expanded), else from `data.table_cells` offsets; `text` = one line per row. Captions follow the table |
| `pictures[]` | `figure` with empty content (text docling found inside the picture is not emitted, as in docling's markdown); captions follow |
| `groups[]` label `inline` | one `text` block (the formatted runs joined) |
| other groups (`list`, `key_value_area`, `form_area`, ...) | walked through for their children |

* **Boxes**: each item's `prov[].bbox` is `{l, t, r, b, coord_origin}`. With the usual
  `coord_origin: "BOTTOMLEFT"` (PDF convention, y up) PuffinParse converts `y0 = (H − t) / H`,
  `y1 = (H − b) / H`; `TOPLEFT` boxes are only divided. `H`/`W` come from `pages[n].size`
  (PDF points for PDFs, pixels for images).
* An item with several `prov` entries (a paragraph continuing on the next page) is split into one
  block per page using each entry's `charspan`.
* `Page.markdown` is the page's blocks joined; docling's own `md_content` is not used because it
  has no page boundaries.
* `Usage.pages` = pages in `json_content.pages` (after page selection); pages without content are
  kept as empty pages.
* Metadata: `docling_status`, `docling_processing_time_s`, `docling_confidence` (docling's
  `layout_score` / `ocr_score` / `mean_grade` report) and `docling_errors` on `partial_success`.
* Block `confidence` is not set: docling reports quality per page/document, not per item.
* `provider_job_id` = the docling-serve `task_id`; `raw` = the whole result JSON.

## 5. Errors and limits

| Situation | Error |
|---|---|
| Server not running / wrong port | `network_error` naming docling-serve, the start command and `DOCLING_BASE_URL` |
| 401/403 (server has an API key) | `authentication_error` |
| 422 (bad `options`) | `bad_request_error` with FastAPI's `detail` |
| `task_status: failure` | `provider_error` with the task's `error_message` / `failure` |
| result `status: failure` / `skipped` | `provider_error` with `errors[].error_message` joined |
| `partial_success` | success; the errors are in `metadata.docling_errors` |
| deadline | `timeout_error` (`timeout_secs` covers submit + polling + result) |

5xx and network errors are retried with backoff like every HTTP provider. Results are
**single-use** by default (`DOCLING_SERVE_SINGLE_USE_RESULTS=true`, removed
`DOCLING_SERVE_RESULT_REMOVAL_DELAY` = 300 s after completion), so a result fetch that is retried
after a dropped connection can 404; rerun the call. `DOCLING_SERVE_MAX_DOCUMENT_TIMEOUT` bounds
processing time server-side.

## 6. Gotchas

* The first request after start-up downloads and loads the models and can take minutes; the
  benchmark's latency numbers exclude that only if you warm the server first.
* The docs at `docs/usage.md` in docling-serve still show `file_sources` / `http_sources` in some
  examples; servers ≥ 1.x reject those with 422. PuffinParse sends `sources` with `kind`.
* `include_images: false` is sent so picture crops are not embedded in the JSON (they are unused
  and make results large). Override it in `provider_options` if you want them in `raw`.
* Heading levels are flat (`level 1`) unless you pass `do_pdf_heading_hierarchy: true`.

## 7. `provider_options` examples

```python
import puffinparse

puffinparse.parse("report.pdf", model="docling")                                   # standard pipeline
puffinparse.parse("report.pdf", model="docling", provider_options={"table_mode": "fast"})
puffinparse.parse("scan.pdf", model="docling",
              provider_options={"force_ocr": True, "ocr_preset": "tesseract"})
puffinparse.parse("paper.pdf", model="docling",
              provider_options={"do_formula_enrichment": True, "do_pdf_heading_hierarchy": True})
puffinparse.parse("doc.pdf", model="docling", base_url="http://gpu-box:5001")
```

Any `ConvertDocumentsOptions` field from the docling-serve API reference is accepted.

## 8. Links

* docling-serve: <https://github.com/docling-project/docling-serve> — usage: `docs/usage.md`,
  configuration: `docs/configuration.md`; live OpenAPI at `{base}/docs`
* DoclingDocument format: <https://docling-project.github.io/docling/concepts/docling_document/>
