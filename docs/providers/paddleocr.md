# PaddleOCR (PaddleX serving, self-hosted)

> **Status: docs-only.** Implemented from the PaddleOCR 3.x serving API reference (the
> "Service-Based Deployment" sections of `docs/version3.x/pipeline_usage/OCR.en.md` and
> `PP-StructureV3.en.md` in PaddlePaddle/PaddleOCR) and the PaddleX serving schemas
> (`paddlex/inference/serving/infra/models.py`: `DataInfo`, `ImageInfo`, `PDFInfo`), read
> 2026-09-24. No PaddleOCR server was run: the sandbox is CPU-only with limited disk and the
> PaddlePaddle + PaddleX serving stack and models were not installed. The fixtures
> `paddleocr_ocr.json` and `paddleocr_layout_parsing.json` are hand-built from those documented
> shapes. Mark this page **verified** after the `#[ignore]`d live test in `providers/paddleocr.rs`
> passes against a real server.
> Tracked in [issue #17](https://github.com/ajinkyashejul/puffinparse/issues/17).
>
> `paddleocr/vl` (added 2026-10-08) is **implemented from docs** too: the "Service Deployment"
> section of `docs/version3.x/pipeline_usage/PaddleOCR-VL.en.md` (PaddlePaddle/PaddleOCR, read
> 2026-10-08) and the PaddleOCR-VL-1.6 model card. Its fixture `paddleocr_vl_layout_parsing.json`
> is hand-built from that documented shape; no PaddleOCR-VL server was run.

## 1. Summary

| | |
|---|---|
| Provider name | `paddleocr` (aliases `paddle`, `paddle_ocr`, `paddlex`) |
| Runs | on your own PaddleOCR / PaddleX "basic serving" endpoints |
| Base URL | `http://localhost:8080` (override: `base_url` on the request, or `PADDLEOCR_BASE_URL`) |
| Parse base URL | `PADDLEOCR_PARSE_BASE_URL`, falling back to `PADDLEOCR_BASE_URL` (a `base_url` on the request wins for both modes) |
| PaddleOCR-VL base URL | `PADDLEOCR_VL_BASE_URL` for `paddleocr/vl`, falling back to `PADDLEOCR_BASE_URL` (a `base_url` on the request wins) |
| API key | none |
| Price | $0 per page (`pricing.json` source `self-hosted`) |
| Implementation | `crates/puffinparse-core/src/providers/paddleocr.rs` |

Each served pipeline is its own HTTP service, so ocr and parse usually run on two ports:

```bash
pip install "paddleocr[all]"          # or paddlex; plus paddlepaddle (CPU) or paddlepaddle-gpu
paddlex --install serving
paddlex --serve --pipeline OCR --port 8080              # → POST /ocr
paddlex --serve --pipeline PP-StructureV3 --port 8081   # → POST /layout-parsing
export PADDLEOCR_BASE_URL=http://localhost:8080 PADDLEOCR_PARSE_BASE_URL=http://localhost:8081
```

## 2. Models exposed by PuffinParse

| Model | Modes | Endpoint | List price |
|---|---|---|---|
| `paddleocr/default` *(default)* | `ocr` (native) | `POST /ocr` — general OCR pipeline (PP-OCRv5 by default) | $0 |
| | `parse` | `POST /layout-parsing` — PP-StructureV3 (layout, tables, formulas, reading order) | $0 |
| `paddleocr/vl` | `parse`, `ocr` (derived from parse) | `POST /layout-parsing` on a **PaddleOCR-VL** pipeline server — PP-DocLayout layout detection + the PaddleOCR-VL 0.9B VLM per region | $0 |

### PaddleOCR-VL (`paddleocr/vl`)

PaddleOCR-VL is not a single-prompt page parser: its model card and the PaddleOCR docs run it
through the PaddleOCR-VL **pipeline** (layout detection, then the VLM on each region with an
element prompt such as `OCR:` or `Table Recognition:`). PuffinParse therefore talks to the
pipeline's serving API, which has the same request body and `layoutParsingResults` response as
PP-StructureV3, rather than to the VLM directly. The pipeline version is the server's
`pipeline_version` (`v1.6`, i.e. PaddleOCR-VL-1.6, is the default; `v1.5` and `v1` also exist);
the response does not report it.

Serve it (commands from the PaddleOCR-VL usage doc, §4 "Service Deployment"):

```bash
# Docker Compose (recommended on NVIDIA GPUs): download compose.yaml and .env from
# deploy/paddleocr_vl_docker/accelerators/nvidia-gpu/ in PaddlePaddle/PaddleOCR, then
docker compose up            # pipeline API on :8080, VLM server (vLLM / FastDeploy) behind it

# or manually: the VLM inference server, then the pipeline server pointing at it
paddleocr genai_server --model_name PaddleOCR-VL-1.6-0.9B --backend vllm --port 8118
paddlex --install serving
paddlex --serve --pipeline PaddleOCR-VL          # :8080
# to use the 8118 server, set in the pipeline config:
#   VLRecognition: {genai_config: {backend: vllm-server, server_url: http://localhost:8118/v1}}

export PADDLEOCR_VL_BASE_URL=http://localhost:8080
```

The pipeline can also use a server started with the default parameters of `vllm serve` as its VLM
backend (`--vl_rec_backend vllm-server --vl_rec_server_url http://localhost:8000/v1
--vl_rec_api_model_name 'PaddlePaddle/PaddleOCR-VL-1.6'` in the doc's CLI example). `provider_options` reach the pipeline's
request fields: `useLayoutDetection`, `useChartRecognition`, `useSealRecognition`,
`promptLabel`, `temperature`, `topP`, `repetitionPenalty`, `minPixels` / `maxPixels`,
`maxNewTokens`, `markdownIgnoreLabels`, `restructurePages`, `mergeTables`, ….

## 3. Request flow PuffinParse uses

One synchronous JSON call per document:

```json
{"file": "<base64 of the file>", "fileType": 0, "visualize": false}
```

`fileType` is `0` for PDF and `1` for images (from magic bytes, then the file extension). A URL
input is downloaded by PuffinParse first (public addresses only, 50 MiB cap; see
[SECURITY.md](../../SECURITY.md#document-urls)) and sent as base64: the serving endpoint is never
asked to fetch a URL itself. `visualize: false` stops the server from
returning base64 visualisation images. `provider_options` are deep-merged into the body, so any
documented field (`useDocOrientationClassify`, `useDocUnwarping`, `useTextlineOrientation`,
`textDetLimitSideLen`, `textRecScoreThresh`, `useTableRecognition`, `returnMarkdownImages`, ...)
passes through.

Response envelope (both endpoints):

```json
{"logId": "<uuid>", "errorCode": 0, "errorMsg": "Success",
 "result": {"ocrResults" | "layoutParsingResults": [ ...one per page... ], "dataInfo": {...}}}
```

`dataInfo` is `{"width", "height", "type": "image"}` or
`{"numPages", "pages": [{"width", "height"}], "type": "pdf" | "tiff"}` — sizes of the images the
pipeline actually ran on (PDF pages are rendered), i.e. the same pixel space as every box.

## 4. Response mapping

**ocr** — `result.ocrResults[i].prunedResult` (page `i + 1`):

| Unified | Source |
|---|---|
| `Line.text` / `confidence` | `rec_texts[j]` / `rec_scores[j]`; empty texts dropped |
| `Line.bbox` | `rec_boxes[j]` (`[x_min, y_min, x_max, y_max]`), else the enclosing box of `rec_polys[j]`, normalised by the page size from `dataInfo` |
| `Word` | each line split on whitespace; no per-word geometry (PaddleOCR recognises lines), confidence = the line's |
| `TextPage.text` | lines joined by `\n`, in PaddleOCR's order |

**parse** — `result.layoutParsingResults[i].prunedResult.parsing_res_list[]`, already in reading
order:

| `block_label` | Block type / markdown |
|---|---|
| `doc_title` | `title`, `# …` |
| `paragraph_title` | `section_header`, `## …` |
| `text`, `content`, `abstract`, `reference`, `reference_content`, `aside_text`, unknown | `text` |
| `table` | `table`; `block_content` is HTML → converted to a markdown table when simple (no spans), else kept as HTML; `text` is one line per row |
| `image`, `chart`, `seal`, `header_image`, `footer_image` | `figure` |
| `figure_title`, `table_title`, `chart_title` | `caption` |
| `formula`, `display_formula`, `inline_formula` | `formula`, wrapped in `$$ … $$` unless already delimited |
| `header` / `footer`, `number` | `header` / `footer` |
| `footnote`, `vision_footnote` | `footnote` |
| `algorithm` | `other`, fenced |
| `formula_number` | `other` |

`block_bbox` (`[x_min, y_min, x_max, y_max]` pixels) is normalised by the page size from
`dataInfo` (falling back to `prunedResult.width/height`). Page markdown is built from the blocks;
PaddleOCR's own `markdown.text` is not used because it embeds tables and images as HTML.

Both modes: `Usage.pages` = pages returned (after `pages` selection, applied client-side);
metadata `paddleocr_log_id`, and `paddleocr_pages_truncated: {returned, document_pages}` when the
server returned fewer pages than `dataInfo.numPages` (see §6); `raw` = the whole envelope.

## 5. Errors and limits

Failures come back as `{"logId", "errorCode": <HTTP status>, "errorMsg": "..."}`. PuffinParse maps
the HTTP status (or `errorCode` if a 200 carries a non-zero code) through the usual table —
401/403 → `authentication_error`, 422/4xx → `bad_request_error`, 5xx → `provider_error` — with
`errorMsg` as the message, verbatim. A refused connection is a `network_error` that names the
serving command and `PADDLEOCR_BASE_URL` / `PADDLEOCR_PARSE_BASE_URL`. Non-image, non-PDF input is
an `input_error` before any request.

## 6. Gotchas

* **10-page limit.** By default the serving layer processes only the first 10 pages of a PDF or
  multi-page TIFF. Set `Serving: extra: max_num_input_imgs: null` in the pipeline config to lift
  it; PuffinParse flags truncation in `metadata.paddleocr_pages_truncated`.
* **Two servers.** `ocr` and `parse` hit different pipelines. If only the OCR pipeline is
  running, `parse` gets a 404; set `PADDLEOCR_PARSE_BASE_URL`. `paddleocr/vl` is a third server
  (`PADDLEOCR_VL_BASE_URL`); its `ocr` is derived from `parse` because the PaddleOCR-VL pipeline has
  no `/ocr` endpoint.
* **Image payloads.** Without `visualize: false` (PuffinParse sends it) the server returns several
  base64 JPEGs per page. PP-StructureV3 also returns markdown images unless
  `returnMarkdownImages: false` — pass it in `provider_options` to shrink responses.
* `pages` is applied after the call (the serving API has no page-range field), so every page up to
  the server's limit is processed.

## 7. `provider_options` examples

```python
import puffinparse

puffinparse.ocr("scan.png", model="paddleocr")
puffinparse.ocr("photo.jpg", model="paddleocr",
            provider_options={"useDocOrientationClassify": True, "useTextlineOrientation": True})
puffinparse.parse("report.pdf", model="paddleocr",
              provider_options={"returnMarkdownImages": False, "useChartRecognition": False})
puffinparse.parse("report.pdf", model="paddleocr", base_url="http://gpu-box:8081")
puffinparse.parse("report.pdf", model="paddleocr/vl",
              provider_options={"useChartRecognition": True, "returnMarkdownImages": False})
```

## 8. Links

* OCR pipeline, serving API: <https://www.paddleocr.ai/latest/en/version3.x/pipeline_usage/OCR.html>
* PP-StructureV3, serving API: <https://www.paddleocr.ai/latest/en/version3.x/pipeline_usage/PP-StructureV3.html>
* Serving deployment guide: <https://www.paddleocr.ai> (Deployment → Serving)
* PaddleOCR-VL usage and serving API: <https://www.paddleocr.ai/latest/en/version3.x/pipeline_usage/PaddleOCR-VL.html>
* PaddleOCR-VL-1.6 model card: <https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6>
