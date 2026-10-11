<p align="center"><img src="https://raw.githubusercontent.com/ajinkyashejul/puffinparse/main/website/assets/puffin.svg" width="128" alt="The PuffinParse puffin waving, three pages in its beak"></p>

# PuffinParse

[![PyPI](https://img.shields.io/pypi/v/puffinparse?color=E95C20)](https://pypi.org/project/puffinparse/) [![GitHub stars](https://img.shields.io/github/stars/ajinkyashejul/puffinparse?style=flat&color=E95C20)](https://github.com/ajinkyashejul/puffinparse/stargazers) [![CI](https://github.com/ajinkyashejul/puffinparse/actions/workflows/ci.yml/badge.svg)](https://github.com/ajinkyashejul/puffinparse/actions/workflows/ci.yml) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/ajinkyashejul/puffinparse/blob/main/LICENSE) [![Docs](https://img.shields.io/badge/docs-puffinparse.com-E95C20)](https://puffinparse.com/docs/) [![Benchmark](https://img.shields.io/badge/benchmark-results-E95C20)](https://puffinparse.com/benchmark-results/) [![Python 3.9+](https://img.shields.io/badge/python-3.9%2B-blue)](https://github.com/ajinkyashejul/puffinparse/blob/main/pyproject.toml)

**One API for every document parser: parse, OCR and extract.** Rust core, Python and TypeScript SDKs, a CLI, a self-hosted gateway, and an open benchmark that ranks providers on accuracy, latency and cost.

```python
import puffinparse

doc = puffinparse.parse("invoice.pdf", model="reducto/standard")     # or "extend/parse_performance", "llamaparse/agentic", ...
print(doc.markdown)                                              # unified markdown, every provider
print(doc.pages[0].blocks[0].bbox, doc.usage.pages, doc.cost_usd)

text = puffinparse.ocr("scan.png", model="llamaparse/fast")          # plain text + line/word boxes
print(text.text, text.pages[0].lines[0].bbox)
```

Switch providers by changing one string. Same request, same response shape, same errors.

## Why

Every document-parsing vendor has its own upload flow, polling loop, JSON layout, block vocabulary, coordinate system and billing unit. PuffinParse hides all of that behind one call, tracks cost per call, retries and falls back across providers, and ships a reproducible benchmark so you can pick a provider on evidence instead of marketing.

Two things make switching real rather than aspirational. **Modes**: every call names `parse`, `ocr` or `extract`, models declare the modes they serve, and a model that cannot serve the one you asked for fails before any network call — so a provider swap can never quietly change the shape of your answer. **Native-format compatibility**: if you are already integrated with Reducto, Extend or LlamaParse, `output_format="reducto"` (and friends) renders *any* provider's result into that vendor's own JSON, so you can re-point a request without touching your parsing code — [see below](#keep-your-reducto-extend-llamaparse-code).

| | |
|---|---|
| **Providers (v0.1)** | 20 providers · 79 models · 3 modes. **Live-verified** against the real APIs: [Reducto](https://reducto.ai), [Extend](https://extend.ai), [LlamaParse](https://cloud.llamaindex.ai). **Verified locally**: the self-hosted Tesseract and Docling. **Docs-only** (implemented from the provider's API documentation and tested against fixture payloads, not yet run live): Mistral, Azure, Textract, Gemini, OpenAI, Anthropic, Mathpix, Datalab, Unstructured, Upstage, Landing AI, Google Document AI, OpenDocRouter and the self-hosted PaddleOCR and vLLM presets ([help verify them](https://github.com/ajinkyashejul/puffinparse/issues/10)). [Full table](#model-names) |
| **Modes** | `parse` (markdown + blocks), `ocr` (plain text + boxes), `extract` (JSON from a schema) |
| **Core** | Rust (`puffinparse-core`): `reqwest` + `tokio`, no vendor SDKs, `#![forbid(unsafe_code)]` |
| **SDKs** | Python 3.9+ (sync + async, fully typed) and Node.js / TypeScript ([`js/`](https://github.com/ajinkyashejul/puffinparse/blob/main/js/README.md)), both on the same Rust core |
| **Gateway** | `puffinparse serve`: one HTTP endpoint with aliases, fallbacks, virtual keys, budgets, rate limits, JSON logs and Prometheus metrics ([`docs/SERVER.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/SERVER.md)) |
| **Long documents** | `submit` / `retrieve` jobs and provider webhooks instead of a blocking call ([below](#long-documents-jobs-and-webhooks)) |
| **CLI** | `puffinparse parse`, `puffinparse ocr`, `puffinparse extract`, `puffinparse providers`, `puffinparse bench` |
| **Reliability** | Retries with jittered backoff, whole-call deadlines, `Router` with ordered fallbacks / round-robin |
| **Compatibility** | `output_format` renders any provider's result in Reducto's, Extend's or LlamaParse's own JSON, so an existing integration keeps its parser ([`docs/COMPAT.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/COMPAT.md)) |
| **Cost** | Embedded, overridable price table → `cost_usd` on every response |
| **Benchmark** | One harness over synthetic data and public benchmarks (ParseBench, olmOCR-bench, OmniDocBench, DP-Bench); deterministic metrics and rule checks, latency, $/1k pages; every output inspectable at [puffinparse.com/benchmark-results](https://puffinparse.com/benchmark-results/) |

## Install

```bash
pip install puffinparse              # Python SDK (abi3 wheels: Linux glibc 2.28+, macOS, Windows)
docker pull ghcr.io/ajinkyashejul/puffinparse   # gateway image (linux/amd64; linux/arm64 too from the next release)
```

The CLI (which also runs the gateway) is a single binary: download the archive for your platform
from [GitHub Releases](https://github.com/ajinkyashejul/puffinparse/releases/latest), or build it
with `cargo install --git https://github.com/ajinkyashejul/puffinparse puffinparse-cli`. On macOS, a
binary downloaded with a browser needs `xattr -d com.apple.quarantine ./puffinparse` once (it is not
notarized yet). The Node.js SDK is not on npm yet: build it from [`js/`](https://github.com/ajinkyashejul/puffinparse/blob/main/js/README.md).

Set the keys for the providers you use:

```bash
export REDUCTO_API_KEY=...
export EXTEND_API_KEY=...
export LLAMA_API_KEY=llx-...      # LlamaCloud / LlamaParse
```

No key yet? The self-hosted engines work out of the box once installed:

```bash
sudo apt-get install tesseract-ocr poppler-utils     # or: brew install tesseract poppler
python -c 'import puffinparse; print(puffinparse.ocr("scan.png", model="tesseract").text)'
puffinparse ocr scan.png -m tesseract                # same with the CLI binary: word boxes + confidences
```

Every provider reads its own variable: [`.env.example`](https://github.com/ajinkyashejul/puffinparse/blob/main/.env.example) lists all of them, the
[model tables](#model-names) say which belongs to which provider, and `puffinparse providers` shows
which ones are set in your shell.

From source (Rust stable + Python 3.9+):

```bash
git clone https://github.com/ajinkyashejul/puffinparse && cd puffinparse
python -m venv .venv && . .venv/bin/activate
pip install maturin && maturin develop --release     # builds the extension into the venv
cargo build --release -p puffinparse-cli                 # ./target/release/puffinparse
```

## Modes

Document AI vendors sell three different products, and they are not interchangeable: layout
**parsing**, plain-text **OCR**, and schema-driven **extraction**. A call picks one mode, and the
mode decides the response type:

| Mode | Call | Returns | Use it for |
|---|---|---|---|
| `parse` | `puffinparse.parse(...)` | `ParseResponse` — `markdown`, `pages[].blocks[]` with types and boxes | RAG chunks, tables, document structure |
| `ocr` | `puffinparse.ocr(...)` | `TextResponse` — `text`, `pages[].lines[]` / `words[]` with boxes | search indexes, redaction, overlays |
| `extract` | `puffinparse.extract(..., schema)` | `ExtractResponse` — `data` shaped by your JSON Schema, plus per-field confidence and citations | invoices, forms, anything with fields |

**Providers are swappable only within a mode.** Every model declares the modes it serves, so a
model that cannot do what you asked raises `UnsupportedModelError` *before* any network call
instead of silently returning the wrong shape. `puffinparse.list_models("ocr")` lists the candidates
for a mode; `puffinparse providers --mode ocr` does the same on the command line. Providers without a
native OCR endpoint serve `ocr` from their parse output, flagged as
`resp.metadata["puffinparse_derived_from"] == "parse"`.

> Which models serve which mode is a registry fact, not a guess — ask
> `puffinparse.list_models("extract")` or `puffinparse providers --mode extract`. Most providers serve
> `parse` and `ocr`; `extract` needs a model built for it (`reducto/extract`,
> `extend/extraction_light`, `azure/invoice`, the vision-LLM models, ...), and calling it with a
> parse-only model raises `UnsupportedModelError` naming the mode.

## Usage

### One call, any provider

```python
import puffinparse

# path, URL, or bytes (+ filename)
doc = puffinparse.parse("contract.pdf", model="extend/parse_performance")
doc = puffinparse.parse("https://cdn.reducto.ai/samples/fidelity-example.pdf", model="reducto/r-1", pages="1-2")
doc = puffinparse.parse(open("scan.png", "rb").read(), filename="scan.png", model="llamaparse/agentic")

doc.markdown            # whole document
doc.text                # plain text
doc.pages[0].markdown   # per page
for block in doc.pages[0].blocks:
    block.type           # text | title | section_header | list | table | figure | header | footer | ...
    block.content        # markdown
    block.bbox           # BBox(x0, y0, x1, y1) normalised 0..1, origin top-left (or None)
    block.confidence     # 0..1 when the provider reports one
doc.usage.pages, doc.usage.credits, doc.cost_usd, doc.latency_ms
```

### Plain text and boxes (`ocr`)

```python
text = puffinparse.ocr("scan.png", model="reducto/standard")

text.text                     # whole document, pages joined by a blank line
page = text.pages[0]
page.text                     # plain text in reading order
for line in page.lines:       # Line(text, bbox, confidence)
    if line.bbox and page.width and page.height:   # normalised 0-1 box -> pixels
        x0, y0, x1, y1 = line.bbox.to_pixels(page.width, page.height)
for word in page.words:       # Word(text, bbox, confidence)
    ...
```

### Structured extraction (`extract`)

```python
schema = {
    "type": "object",
    "properties": {"invoice_number": {"type": "string"}, "total": {"type": "number"}},
    "required": ["invoice_number", "total"],
}
result = puffinparse.extract("invoice.pdf", schema, model="reducto/extract", citations=True)

result.data                          # {"invoice_number": "INV-42", "total": 1280.5}
result.fields["/total"].confidence   # per-field confidence, keyed by JSON pointer
result.citations("/total")           # [Citation(page_number=2, bbox=..., text="Total due 1,280.50")]
```

### Async

```python
doc = await puffinparse.aparse("contract.pdf", model="llamaparse/cost_effective")
text = await puffinparse.aocr("scan.png", model="llamaparse/fast")
```

Runs on the Rust runtime; the event loop is never blocked.

### Long documents: jobs and webhooks

`parse` waits for the provider. For long documents, batches or webhook-driven pipelines, split it:

```python
job = puffinparse.submit("annual-report.pdf", model="reducto/standard",
                     webhook_url="https://example.com/hooks/puffinparse")   # optional
store(job)                                  # a Job is plain data: serialisable, holds no key

result = puffinparse.retrieve(job)              # Job (still pending) or ParseResponse
# ...or, in your web handler, turn the provider's webhook body into a result:
result = puffinparse.handle_webhook(request.json(), model="reducto")  # verify the signature first
```

Reducto, Extend, LlamaParse and OpenDocRouter support jobs; `webhook_url` maps to each provider's
per-job webhook where one exists (Extend only has workspace-level webhooks and OpenDocRouter has
none, so it is rejected there). See
[`docs/SPEC.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/SPEC.md) §15.

### TypeScript / Node.js

The same core as a napi-rs addon, with camelCase typed responses:

```ts
import { parse, Router } from "puffinparse";

const doc = await parse("invoice.pdf", { model: "reducto/standard", fallbacks: ["llamaparse/agentic"] });
console.log(doc.markdown, doc.usage.pages, doc.costUsd);
```

The npm package is not published yet; build it from a clone (`cd js && npm ci && npm run build`), see
[`js/README.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/js/README.md). Prebuilt binaries for Linux x64/arm64 (glibc), macOS and Windows x64
will ship with the npm release.

### Model names

`"<provider>/<model>"`, like LiteLLM. A bare provider name picks that provider's default model
**for the mode you called** — marked `*` below. `puffinparse providers` and
`puffinparse.list_models(mode)` print the live list; the tables here are the built-in registry
(`crates/puffinparse-core/src/model.rs` and `pricing.json`).

Prices are public pay-as-you-go **list prices, per page, per mode**, shown as
`parse · ocr · extract` with `—` where a model does not serve that mode. Override one mode at a
time with `puffinparse.set_pricing({"reducto/standard": 0.012}, "parse")`, and estimate with
`puffinparse.estimate_cost("reducto/standard", pages=1000, mode="ocr")`. The vision-LLM providers
(Gemini, OpenAI, Anthropic) bill tokens rather than pages, so their per-page numbers are
**estimates** — see the source lines in `pricing.json`.

Each provider carries one of three verification labels:

- **live-verified**: the live tests pass against the real API with a key, and the fixtures include
  redacted live responses (Reducto, Extend, LlamaParse).
- **verified locally**: a self-hosted engine whose live tests pass against a local install
  (Tesseract, Docling).
- **docs-only**: implemented from the provider's API documentation and tested against fixture
  payloads built from it, but not yet run against the live API. Expect wire-format differences
  until it is verified; [issue #10](https://github.com/ajinkyashejul/puffinparse/issues/10) tracks this, and a run with your own key is a welcome
  contribution.

[`docs/providers/README.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/providers/README.md) tracks the state and links one reference
page per provider.

**Reducto** · `REDUCTO_API_KEY` · live-verified — Layout parsing plus a schema extractor with citations; the default parse target.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `reducto/standard` | parse `*`, ocr `*` | $0.015 · $0.015 · — | Reducto Parse with account-default model (legacy standard) |
| `reducto/r-1` | parse, ocr | $0.01 · $0.01 · — | Reducto Parse with settings.model=r-1 (newest model, cheaper) |
| `reducto/agentic` | parse, ocr | $0.03 · $0.03 · — | Reducto Parse with agentic text+table enhancement (highest accuracy, 2x cost) |
| `reducto/extract` | extract `*` | — · — · $0.035 | Reducto Extract: POST /extract with a JSON schema, citations on request |
| `reducto/deep_extract` | extract | — · — · $0.055 | Reducto Deep Extract (settings.deep_extract = true) for long or complex documents |

**Extend** · `EXTEND_API_KEY` · live-verified — Parse engines that trade accuracy for cost per page, and two extraction processors.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `extend/parse_performance` | parse `*`, ocr `*` | $0.025 · $0.025 · — | Extend engine=parse_performance (highest accuracy) |
| `extend/parse_light` | parse, ocr | $0.00625 · $0.00625 · — | Extend engine=parse_light (fast, cheap, digital-native docs) |
| `extend/parse_auto` | parse, ocr | $0.025 · $0.025 · — | Extend engine=parse_auto (picks light or performance per page) |
| `extend/extraction_performance` | extract `*` | — · — · $0.0625 | Extend Extract, baseProcessor=extraction_performance (runs parse_performance) |
| `extend/extraction_light` | extract | — · — · $0.015 | Extend Extract, baseProcessor=extraction_light (runs parse_light) |

**LlamaParse (LlamaCloud)** · `LLAMA_API_KEY` · live-verified — Credit-priced tiers from plain text extraction to agentic parsing; extract from `cost_effective` up.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `llamaparse/fast` | parse, ocr | $0.00125 · $0.00125 · — | LlamaParse tier=fast (text extraction, no OCR of images) |
| `llamaparse/cost_effective` | parse `*`, ocr `*`, extract `*` | $0.00375 · $0.00375 · $0.01 | LlamaParse tier=cost_effective |
| `llamaparse/agentic` | parse, ocr, extract | $0.0125 · $0.0125 · $0.03125 | LlamaParse tier=agentic |
| `llamaparse/agentic_plus` | parse, ocr, extract | $0.05625 · $0.05625 · $0.11875 | LlamaParse tier=agentic_plus (highest accuracy) |

**Mistral Document AI** · `MISTRAL_API_KEY` · docs-only — One OCR model with pinned versions; annotations give it an extract mode.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `mistral/ocr-latest` | parse `*`, ocr `*`, extract `*` | $0.004 · $0.004 · $0.005 | Mistral OCR, latest alias (mistral-ocr-latest; currently OCR 4.1) |
| `mistral/ocr-4-1` | parse, ocr, extract | $0.004 · $0.004 · $0.005 | Mistral OCR 4.1 pinned (mistral-ocr-4-1; blocks + block confidence scores) |
| `mistral/ocr-4-0` | parse, ocr, extract | $0.004 · $0.004 · $0.005 | Mistral OCR 4.0 pinned (mistral-ocr-4-0; paragraph blocks, no block confidence) |
| `mistral/ocr-2512` | parse, ocr, extract | $0.002 · $0.002 · $0.003 | Mistral OCR 3 pinned (mistral-ocr-2512; cheaper, no paragraph blocks) |

**Azure AI Document Intelligence** · `AZURE_DOCUMENT_INTELLIGENCE_KEY` (+ `AZURE_DOCUMENT_INTELLIGENCE_ENDPOINT`) · docs-only — Prebuilt Document Intelligence models: `read` for OCR, `layout` for structure, fixed schemas for extraction.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `azure/read` | ocr `*` | — · $0.0015 · — | Azure prebuilt-read: native OCR, words/lines with confidence (cheapest) |
| `azure/layout` | parse `*`, ocr | $0.01 · $0.01 · — | Azure prebuilt-layout: markdown, paragraphs with roles, tables, polygons |
| `azure/invoice` | extract `*` | — · — · $0.01 | Azure prebuilt-invoice: fixed invoice schema |
| `azure/receipt` | extract | — · — · $0.01 | Azure prebuilt-receipt: fixed receipt schema |
| `azure/id_document` | extract | — · — · $0.01 | Azure prebuilt-idDocument: fixed ID document schema |
| `azure/tax_us_w2` | extract | — · — · $0.01 | Azure prebuilt-tax.us.w2: fixed W-2 schema |
| `azure/custom` | parse, ocr, extract | $0.03 · $0.03 · $0.03 | Azure custom model, id via provider_options.model_id |

**AWS Textract** · `AWS_ACCESS_KEY_ID` (+ `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`) · docs-only — One AWS API, four feature sets; SigV4-signed, no SDK.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `textract/detect-text` | ocr `*` | — · $0.0015 · — | Textract DetectDocumentText: raw OCR, lines + words with boxes (cheapest) |
| `textract/layout` | parse `*`, ocr | $0.015 · $0.015 · — | Textract AnalyzeDocument LAYOUT + TABLES: reading-order markdown + tables |
| `textract/queries` | extract `*` | — · — · $0.015 | Textract AnalyzeDocument QUERIES: one natural-language query per schema field |
| `textract/forms` | extract | — · — · $0.05 | Textract AnalyzeDocument FORMS: key-value pairs matched to schema fields |

**Google Gemini** · `GEMINI_API_KEY` · docs-only — Vision-LLM transcription. Prices are per-page **estimates** (~1500 in + ~800 out tokens); `cost_usd` uses them until the response reports real usage.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `gemini/2.5-flash` | parse `*`, ocr `*`, extract `*` | $0.00245 · $0.00245 · $0.0012 | Gemini 2.5 Flash: vision-LLM transcription, the price/quality default |
| `gemini/2.5-pro` | parse, ocr, extract | $0.009875 · $0.009875 · $0.004875 | Gemini 2.5 Pro: highest accuracy, ~4x the cost of Flash |
| `gemini/2.5-flash-lite` | parse, ocr, extract | $0.00047 · $0.00047 · $0.00027 | Gemini 2.5 Flash-Lite: cheapest and fastest, clean documents |
| `gemini/3.5-flash` | parse, ocr, extract | $0.00945 · $0.00945 · $0.00495 | Gemini 3.5 Flash: frontier Flash generation (GA 2026-05-19) |
| `gemini/3.5-flash-lite` | parse, ocr, extract | $0.00245 · $0.00245 · $0.0012 | Gemini 3.5 Flash-Lite: low-latency 3.x tier (GA 2026-07-21) |
| `gemini/3.8-flash` | parse, ocr, extract | $0.004125 · $0.004125 · $0.00225 | Gemini 3.8 Flash: newest Flash model (GA 2026-09-02, introductory pricing) |
| `gemini/3.8-flash-low` | parse, ocr, extract | $0.004125 · $0.004125 · $0.00225 | Gemini 3.8 Flash with thinkingConfig.thinkingLevel=low (cheaper, faster transcription) |
| `gemini/3-flash-preview` | parse, ocr, extract | $0.00315 · $0.00315 · $0.00165 | Gemini 3 Flash preview (gemini-3-flash-preview; PDF and image input) |

**OpenAI** · `OPENAI_API_KEY` · docs-only — Vision transcription through the Responses API. Per-page prices are estimates, as for Gemini.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `openai/gpt-5.6-luna` | parse `*`, ocr `*`, extract `*` | $0.00114 · $0.00114 · $0.00114 | OpenAI Responses API, gpt-5.6-luna (cheapest current vision model) |
| `openai/gpt-5.6-terra` | parse, ocr, extract | $0.0114 · $0.0114 · $0.0114 | OpenAI Responses API, gpt-5.6-terra (balanced capability/price) |
| `openai/gpt-5.6-sol` | parse, ocr, extract | $0.02 · $0.02 · $0.02 | OpenAI Responses API, gpt-5.6-sol (flagship GPT-5.6) |
| `openai/gpt-6-astra` | parse, ocr, extract | $0.05 · $0.05 · $0.05 | OpenAI Responses API, gpt-6-astra (most capable, most expensive) |
| `openai/gpt-6-luna` | parse, ocr, extract | $0.0005 · $0.0005 · $0.0005 | OpenAI Responses API, gpt-6-luna (cheapest GPT-6, image and PDF input) |

**Anthropic (Claude)** · `ANTHROPIC_API_KEY` · docs-only — Vision transcription through the Messages API. Per-page prices are estimates.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `anthropic/claude-sonnet-5` | parse `*`, ocr `*`, extract `*` | $0.01 · $0.01 · $0.01 | Claude Messages API, claude-sonnet-5 (balanced vision transcription) |
| `anthropic/claude-haiku-4-5` | parse, ocr, extract | $0.005 · $0.005 · $0.005 | Claude Messages API, claude-haiku-4-5 (cheapest, 200K context) |
| `anthropic/claude-opus-5` | parse, ocr, extract | $0.025 · $0.025 · $0.025 | Claude Messages API, claude-opus-5 (highest accuracy) |
| `anthropic/claude-opus-5-5` | parse, ocr, extract | $0.02 · $0.02 · $0.02 | Claude Messages API, claude-opus-5-5 (current Opus; tool_choice auto, no forced tool) |
| `anthropic/claude-haiku-5-5` | parse, ocr, extract | $0.0005 · $0.0005 · $0.0005 | Claude Messages API, claude-haiku-5-5 (cheapest Claude, 1M context) |

**Mathpix** · `MATHPIX_APP_KEY` (+ `MATHPIX_APP_ID`) · docs-only — Maths-first OCR: Mathpix Markdown with LaTeX, line and word polygons.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `mathpix/pdf` | parse `*`, ocr `*` | $0.005 · $0.005 · — | Mathpix v3/pdf document OCR (image inputs auto-routed to v3/text); MMD + line polygons |
| `mathpix/text` | parse, ocr | $0.002 · $0.002 · — | Mathpix v3/text single-image OCR (line + word polygons, per-image billing) |

**Datalab (Marker)** · `DATALAB_API_KEY` · docs-only — Marker's hosted API: three quality modes at two price points.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `datalab/fast` | parse, ocr | $0.004 · $0.004 · — | Datalab Convert mode=fast (lowest latency, digital-native documents) |
| `datalab/balanced` | parse `*`, ocr `*` | $0.004 · $0.004 · — | Datalab Convert mode=balanced (Datalab's recommended default) |
| `datalab/accurate` | parse, ocr | $0.01 · $0.01 · — | Datalab Convert mode=accurate (scans, dense layouts, complex tables) |

**Unstructured** · `UNSTRUCTURED_API_KEY` · docs-only — The partitioner behind many RAG pipelines; flat per-page price across strategies.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `unstructured/hi_res` | parse `*`, ocr `*` | $0.015 · $0.015 · — | Unstructured strategy=hi_res (layout model + OCR; coordinates, table HTML, confidence) |
| `unstructured/fast` | parse, ocr | $0.015 · $0.015 · — | Unstructured strategy=fast (text-layer extraction, no OCR, rejects images) |
| `unstructured/auto` | parse, ocr | $0.015 · $0.015 · — | Unstructured strategy=auto (routes each page to fast / hi_res / VLM) |

**Upstage Document Parse** · `UPSTAGE_API_KEY` · docs-only — Korean/English layout parsing to HTML or markdown.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `upstage/document-parse` | parse `*`, ocr `*` | $0.01 · $0.01 · — | Upstage Document Parse (layout to HTML/Markdown; 100 pages sync, 1000 async) |
| `upstage/document-parse-nightly` | parse, ocr | $0.01 · $0.01 · — | Upstage Document Parse nightly build (newest layout model, may change without notice) |

**Landing AI (Agentic Document Extraction)** · `LANDINGAI_API_KEY` · docs-only — Agentic Document Extraction (DPT-2): parse, then extract against your schema.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `landingai/dpt-2` | parse `*`, ocr `*`, extract `*` | $0.03 · $0.03 · $0.04 | Landing AI ADE DPT-2; extract runs parse then /v1/ade/extract with the JSON schema |

**Google Cloud Document AI** · `GOOGLE_DOCUMENTAI_ACCESS_TOKEN` (+ `GOOGLE_DOCUMENTAI_PROJECT`, `_LOCATION`, `_PROCESSOR_ID`) · docs-only — Processor-based: pick the processor id in `provider_options`; OCR, layout, forms and prebuilt extractors.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `google_documentai/ocr` | parse `*`, ocr `*` | $0.0015 · $0.0015 · — | Document OCR processor: native text + word boxes (processor id from config) |
| `google_documentai/layout` | parse, ocr | $0.01 · $0.01 · — | Layout Parser processor: documentLayout blocks (headings, tables, lists) |
| `google_documentai/form` | parse, ocr, extract `*` | $0.03 · $0.03 · $0.03 | Form Parser processor: paragraphs + tables, entities as extraction |
| `google_documentai/prebuilt` | parse, ocr, extract | $0.03 · $0.03 · $0.03 | Prebuilt or custom extractor (invoice, W2, ...): entities as extraction |

**OpenDocRouter** · `OPEN_DOC_ROUTER_API_KEY` · docs-only — LlamaIndex's hosted router: frontier VLMs and open OCR models behind one endpoint, billed per token at the providers' prices, per-page status and charge, layout boxes on request. Model ids keep the router's own `<vendor>/<model>` after the provider name. Prices here are the site's average charge per page (price version 2026-10-06); `cost_usd` is the actual `charge_usd`.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `opendocrouter/google/gemini-3.8-flash-low` | parse `*`, ocr `*` | $0.005908 · $0.005908 · — | Gemini 3.8 Flash (low thinking); the router's quickstart model |
| `opendocrouter/google/gemini-3-flash` | parse, ocr | $0.019667 · $0.019667 · — | Gemini 3 Flash |
| `opendocrouter/anthropic/claude-opus-5-5` | parse, ocr | $0.04882 · $0.04882 · — | Claude Opus 5.5 (frontier, most expensive) |
| `opendocrouter/anthropic/claude-haiku-5-5` | parse, ocr | $0.001225 · $0.001225 · — | Claude Haiku 5.5 |
| `opendocrouter/openai/gpt-5.6-terra` | parse, ocr | $0.019886 · $0.019886 · — | GPT-5.6 Terra |
| `opendocrouter/openai/gpt-6-luna` | parse, ocr | $0.000798 · $0.000798 · — | GPT-6 Luna (cheapest frontier model) |
| `opendocrouter/infly/infinity-parser2-flash` | parse, ocr | $0.004344 · $0.004344 · — | Infinity-Parser2-Flash (open model hosted by the router) |
| `opendocrouter/opendatalab/mineru2.5-pro` | parse, ocr | $0.000861 · $0.000861 · — | MinerU2.5-Pro (open model hosted by the router) |
| `opendocrouter/xingchen-agi/teleocr` | parse, ocr | $0.002702 · $0.002702 · — | TeleOCR (open model hosted by the router) |
| `opendocrouter/rednote-hilab/dots.mocr` | parse, ocr | $0.00397 · $0.00397 · — | dots.mocr (open model hosted by the router) |
| `opendocrouter/paddlepaddle/paddleocr-vl-1.6` | parse, ocr | $0.002111 · $0.002111 · — | PaddleOCR-VL-1.6 (open model hosted by the router) |

**Self-hosted engines** · no key · $0/page — out-of-process: a local binary or a server you run. Tesseract and Docling are verified locally; PaddleOCR is docs-only.
**Self-hosted engines** · no key · $0/page — out-of-process: a local binary or a server you run. Tesseract and Docling are verified locally; PaddleOCR and vLLM are docs-only.

| Model | Modes | List price / page (parse · ocr · extract) | Notes |
|---|---|---|---|
| `tesseract/default` | ocr `*` (native), parse `*` | $0 · $0 · — | local `tesseract` binary (`TESSERACT_CMD`), PDFs via `pdftoppm`; word/line boxes + confidences, no layout model; verified locally |
| `docling/default` | parse `*`, ocr `*` | $0 · $0 · — | your docling-serve (`DOCLING_BASE_URL`): layout, tables, OCR; verified locally |
| `paddleocr/default` | ocr `*` (native), parse `*` (PP-StructureV3) | $0 · $0 · — | your PaddleOCR serving (`PADDLEOCR_BASE_URL`); docs-only |
| `paddleocr/vl` | parse, ocr | $0 · $0 · — | your PaddleOCR-VL pipeline serving (`PADDLEOCR_VL_BASE_URL`; PaddleOCR-VL-1.6 by default): layout + 0.9B VLM; docs-only |
| `vllm/infinity-parser2-flash` | parse `*`, ocr `*` | $0 · $0 · — | `infly/Infinity-Parser2-Flash` on your `vllm serve` (`VLLM_BASE_URL`): page images, layout blocks with boxes; docs-only |
| `vllm/dots.mocr` | parse, ocr | $0 · $0 · — | `rednote-hilab/dots.mocr` on your `vllm serve` (`VLLM_BASE_URL`): page images, layout blocks with boxes; docs-only |

### Keep your Reducto / Extend / LlamaParse code

Already integrated with a vendor? Ask for its shape and PuffinParse renders the response into that
vendor's own JSON, whatever provider actually ran the call — switch the model string, keep your
parser:

```python
doc = puffinparse.parse("invoice.pdf", model="extend/parse_light", output_format="reducto")
block = doc["result"]["chunks"][0]["blocks"][0]            # Reducto's shape, Extend's engine
draw(block["bbox"]["left"], block["bbox"]["top"], block["type"])
```

`output_format` accepts `reducto`, `extend`, `llamaparse`, or `puffinparse` (the unified shape, and
the default). It returns a plain `dict` instead of a dataclass, and works on `parse`, `extract`,
their async variants, the `Router`, and the CLI (`--output-format <vendor>` with `--format json`).
An unknown name raises `BadRequestError` before any network call.

What is guaranteed is **structural fidelity** — key set and nesting, one chunk/page per page, the
content strings, the vendor's own block vocabulary and coordinate units, the billed page count —
not byte equality with what the vendor would have returned. Fields PuffinParse does not model
(presigned URLs, studio links, billing breakdowns, OCR word layers) are `null` or empty, and a few
block types are lossy. [`docs/COMPAT.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/COMPAT.md) enumerates all of it, per format;
`examples/switch_provider_keep_format.py` is a runnable version of the above.

### Provider-specific options

Anything the common request doesn't cover is passed through verbatim:

```python
puffinparse.parse("doc.pdf", model="reducto/standard",
              provider_options={"settings": {"return_ocr_data": True}, "async": True})
puffinparse.parse("doc.pdf", model="extend/parse_performance",
              provider_options={"blockOptions": {"figures": {"enabled": False}}})
puffinparse.ocr("doc.pdf", model="llamaparse/agentic",
            provider_options={"take_screenshot": True, "version": "2026-08-19"})
```

`include_raw=True` attaches the provider's original payload as `resp.raw`.

### Router: fallbacks and load balancing

```python
router = puffinparse.Router(
    ["reducto/standard", "llamaparse/agentic", "extend/parse_light"],
    mode="parse",                # the mode every model must serve; "ocr" and "extract" too
    strategy="ordered",          # or "round_robin"
)
resp = router.parse("doc.pdf")   # falls back on provider / rate-limit / timeout / network errors
router.stats()                   # per-model successes, failures, latency, cost, pages
```

A router is bound to one mode: `Router([...], mode="parse").ocr(...)` raises `InputError` rather
than quietly changing the answer's shape, and every model is validated against the mode when the
router is built. Auth, bad-request and input errors never trigger a fallback.

### Errors

All errors derive from `puffinparse.PuffinParseError` and carry `provider`, `status_code`, `job_id` and `retryable`:

`AuthenticationError`, `RateLimitError`, `BadRequestError`, `ProviderError`, `TimeoutError`, `UnsupportedModelError`, `InputError`, `NetworkError`.

`UnsupportedModelError` also covers a model asked to do a mode it does not serve; the message
names the mode and the models that do serve it.

### Callbacks and logging

Callbacks fire for every mode, with that mode's response object:

```python
puffinparse.success_callback.append(lambda r: print(r.model, r.usage.pages, r.cost_usd))
puffinparse.failure_callback.append(lambda e: print("failed:", e))
puffinparse.init_logging("debug")    # Rust core tracing on stderr (or PUFFINPARSE_LOG=debug for the CLI)
```

### CLI

```bash
puffinparse providers                                             # models, modes, prices, key status
puffinparse providers --mode ocr                                  # only models that serve a mode
puffinparse parse invoice.pdf -m extend/parse_light               # markdown to stdout
puffinparse parse scan.png -m llamaparse/agentic -f json --raw    # full unified JSON (+ provider payload)
puffinparse parse doc.pdf -m extend/parse_light -f json \
    --output-format reducto                                   # ... in Reducto's response shape
puffinparse parse doc.pdf -m reducto/r-1 --pages 1-3 -f text
puffinparse ocr scan.png -m reducto/standard                      # plain text to stdout
puffinparse ocr scan.png -f json                                  # TextResponse: text + lines + words
puffinparse extract invoice.pdf -s schema.json --citations        # JSON object from a schema
puffinparse extract invoice.pdf -s '{"type":"object"}'            # inline schema also works
puffinparse extract invoice.pdf -s schema.json --output-format extend   # Extend's extract_run shape
puffinparse providers --json | jq '.output_formats'               # the vendor shapes this build renders
```

### Gateway server

Run PuffinParse as one HTTP endpoint so applications never hold provider keys:

```bash
puffinparse serve --config puffinparse.toml       # or: docker run ghcr.io/ajinkyashejul/puffinparse (docs/SERVER.md)
curl -H "Authorization: Bearer $TEAM_KEY" -F file=@invoice.pdf -F model=invoices \
     http://localhost:4000/v1/parse
```

`puffinparse.toml` defines aliases (`invoices = [reducto/standard, extend/parse_performance]` with
ordered or round-robin fallback), provider keys as `env:` references, and virtual keys with model
allow-lists, monthly USD budgets and per-minute limits. `/v1/models`, `/v1/usage`, `/health` and
Prometheus `/metrics` are built in; request logs are JSON lines that never contain document content
or secrets. Defaults are hardened for untrusted callers (no unauthenticated start off localhost,
no in-gateway URL downloads unless enabled and then public addresses only, concurrency and
request-time limits; see the Hardening section of `docs/SERVER.md`). Reference: [`docs/SERVER.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/SERVER.md), sample:
[`examples/server/puffinparse.toml`](https://github.com/ajinkyashejul/puffinparse/blob/main/examples/server/puffinparse.toml).

### Rust

```rust
use puffinparse_core::{extract, ocr, parse, DocumentRequest, ExtractRequest};

let doc = parse(DocumentRequest::from_path("invoice.pdf").model("reducto/standard")).await?;
println!("{} pages, ${:.4}\n{}", doc.usage.pages, doc.cost_usd.unwrap_or(0.0), doc.markdown);

let text = ocr(DocumentRequest::from_path("scan.png").model("llamaparse/fast")).await?;
println!("{} lines on page 1", text.pages[0].lines.len());

let schema = serde_json::json!({"type": "object", "properties": {"total": {"type": "number"}}});
let req = ExtractRequest::new(DocumentRequest::from_path("invoice.pdf").model("reducto/extract"), schema);
let data = extract(req.citations(true)).await?.data;
```

## Benchmark

PuffinParse ships an open, reproducible benchmark. Pages from public benchmarks are scored against their own published truth or rules, the synthetic set's truth is exact by construction (its documents are rendered from the same source as the truth files), metrics are deterministic text comparisons with no LLM judge, and every run records the dataset hash, model, latency and cost.

```bash
python benchmark/generate_synthetic.py                       # regenerate the dataset (byte-reproducible)
puffinparse bench run --dataset benchmark/datasets/synthetic-v1 \
    --models reducto/standard extend/parse_performance llamaparse/cost_effective
puffinparse bench report benchmark/results/2026-09-25-combined-v3.json   # one dataset's leaderboard section
puffinparse bench score prediction.md truth.md                   # metrics for one pair, no network
```

Metrics (after NFKC + markdown stripping + whitespace collapsing, case-insensitive by default):

- **Overall** = `100 ×` the mean of each document's headline metric: `char_similarity` (`1 − levenshtein / max(len)`) for transcripts, the table score for table-only pages, the rule pass rate for rule-checked pages; a failed call scores 0 (details in [`benchmark/README.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/benchmark/README.md))
- **CER**, **WER**, **word F1** (bag-of-words precision / recall)
- **Order**: Kendall-τ-style agreement of shared line order (reading order)
- **Table**: character similarity restricted to markdown table rows
- **Latency** p50 / p95 and ms per page; **$/1k pages** from the price table

The current leaderboard is in [`benchmark/LEADERBOARD.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/benchmark/LEADERBOARD.md), and every
document, output, diff and rule check is browsable at
[puffinparse.com/benchmark-results](https://puffinparse.com/benchmark-results/). Datasets:
`synthetic-v1` (exact truth by construction) and the headline `combined-v3`, which adds subsets of
[ParseBench](https://github.com/run-llama/ParseBench), [olmOCR-bench](https://huggingface.co/datasets/allenai/olmOCR-bench),
[OmniDocBench](https://github.com/opendatalab/OmniDocBench) and
[DP-Bench](https://huggingface.co/datasets/upstage/dp-bench) converted by
[`benchmark/adapters/`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/benchmarks/adapters.md) at pinned revisions. Older `combined-v1` and
`combined-v2` runs are kept for comparison. Long runs are safe:
`--dry-run` and `--max-cost` show and cap the spend before any call, and `--resume` continues an
interrupted run.

## How it maps providers

| | Reducto | Extend | LlamaParse |
|---|---|---|---|
| Upload | `POST /upload` → `reducto://` id (URLs passed directly) | `POST /files/upload` → `file_…` (URLs passed directly) | multipart `file` or `input_url` |
| Parse | `POST /parse` (sync) or `/parse_async` + `GET /job/{id}` | `POST /parse_runs` + `GET /parse_runs/{id}` | `POST /api/v1/parsing/upload` + `GET …/job/{id}` + `…/result/json` |
| Pages | `chunk_mode=page`, blocks grouped by `bbox.page` | page chunks, blocks by `metadata.page.number` | `pages[].md` / `items[]` |
| Boxes | already normalised | divided by `metadata.page.width/height` | `bBox` divided by page `width/height` |
| Usage | `usage.num_pages`, `usage.credits` | `metrics.pageCount`, `usage.credits` | `job_metadata.job_pages` |

Full details, including the exact wire formats verified against live responses, are in [`docs/SPEC.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/SPEC.md).

## Project layout

```
crates/puffinparse-core     Rust library: types, providers, router, pricing, benchmark metrics
crates/puffinparse-cli      `puffinparse` binary
crates/puffinparse-python   PyO3 extension (puffinparse._core)
crates/puffinparse-node     napi-rs addon for the Node.js SDK
crates/puffinparse-server   HTTP gateway behind `puffinparse serve`
python/puffinparse          Python package (typed public API)
js/                     Node.js / TypeScript package
benchmark/              dataset generator, adapters, datasets, results, leaderboard, viewer
docs/SPEC.md            specification
```

## Roadmap

Planned work is tracked in [GitHub Issues](https://github.com/ajinkyashejul/puffinparse/issues).
Open items:

- **Packaging**: PyPI wheels and CLI binaries ship today; npm and crates.io are next, plus a
  multi-arch gateway image and a notarized macOS binary ([#9](https://github.com/ajinkyashejul/puffinparse/issues/9)).
- **Providers**: live-verify the docs-only providers ([#10](https://github.com/ajinkyashejul/puffinparse/issues/10)), including PaddleOCR
  against a real server ([#17](https://github.com/ajinkyashejul/puffinparse/issues/17)).
- **Benchmark**: add `docling/default` with a hardware note
  ([#12](https://github.com/ajinkyashejul/puffinparse/issues/12)) to `combined-v3` (`tesseract/default` is already in it); regenerate the leaderboard in one command
  ([#13](https://github.com/ajinkyashejul/puffinparse/issues/13)); score table-cell neighbour relations exactly ([#15](https://github.com/ajinkyashejul/puffinparse/issues/15)); a READoc
  long-document track ([#16](https://github.com/ajinkyashejul/puffinparse/issues/16)).
- **SDK**: `output_format="mistral"` for code written against Mistral OCR responses
  ([#14](https://github.com/ajinkyashejul/puffinparse/issues/14)).
- **Playground**: upload a document and compare up to three models side by side
  ([#18](https://github.com/ajinkyashejul/puffinparse/issues/18)).

## Support

PuffinParse is built in the open by one person. If it saved you time, a star on GitHub helps other
developers find it. Reports of a wrong score, a missing provider or a confusing doc help even more:
[open an issue](https://github.com/ajinkyashejul/puffinparse/issues/new/choose).

## Contributing

See [CONTRIBUTING.md](https://github.com/ajinkyashejul/puffinparse/blob/main/CONTRIBUTING.md). Adding a provider is one Rust file plus a fixture test; the checklist is in the [new provider issue template](https://github.com/ajinkyashejul/puffinparse/blob/main/github/ISSUE_TEMPLATE/new_provider.md). Issues labelled [`good first issue`](https://github.com/ajinkyashejul/puffinparse/labels/good%20first%20issue) are a good place to start, and [AGENTS.md](https://github.com/ajinkyashejul/puffinparse/blob/main/AGENTS.md) summarises the build commands and conventions for coding agents.

## Acknowledgements

PuffinParse stands on other people's work:

- **Benchmarks and datasets.** The combined benchmark is built on
  [ParseBench](https://github.com/run-llama/ParseBench) (LlamaIndex; Zhang et al., 2026,
  arXiv:2604.08538; Apache-2.0),
  [olmOCR-bench](https://huggingface.co/datasets/allenai/olmOCR-bench) (Allen Institute for AI;
  Poznanski et al., 2025, arXiv:2502.18443; ODC-BY-1.0),
  [OmniDocBench](https://github.com/opendatalab/OmniDocBench) (OpenDataLab / Shanghai AI
  Laboratory; Ouyang et al., 2024, arXiv:2412.07626; research-only, so only an index is committed)
  and [DP-Bench](https://huggingface.co/datasets/upstage/dp-bench) (Upstage AI; MIT). Each
  `benchmark/datasets/<name>/README.md` gives the pinned revision, the licence, what we changed and
  the citation the authors ask for. If you use these numbers, please cite those benchmarks too.
- **Metrics.** The rule checks re-implement the test semantics of olmOCR-bench and ParseBench, and
  table structure is scored with TEDS (Zhong, ShafieiBavani and Jimeno Yepes, 2020, "Image-based
  table recognition: data, model, and evaluation"). The scorer is our own Rust code; the source
  comments say whose behaviour each part mirrors.
- **API design.** The `"<provider>/<model>"` model strings and the gateway server follow
  [LiteLLM](https://github.com/BerriAI/litellm), which did this first for LLM APIs.
- **Providers and engines.** The compatibility shapes mirror the public response formats of
  Reducto, Extend and LlamaParse so existing code can switch, and the open baselines are
  [Tesseract](https://github.com/tesseract-ocr/tesseract) and
  [Docling](https://github.com/docling-project/docling). Provider names are trademarks of their
  owners. PuffinParse is not affiliated with or endorsed by any of them.
- **Libraries.** [tokio](https://tokio.rs), [reqwest](https://github.com/seanmonstar/reqwest),
  [serde](https://serde.rs), [axum](https://github.com/tokio-rs/axum),
  [clap](https://github.com/clap-rs/clap), [PyO3](https://pyo3.rs),
  [maturin](https://github.com/PyO3/maturin), [napi-rs](https://napi.rs) and the other crates in
  [THIRD_PARTY_NOTICES.md](https://github.com/ajinkyashejul/puffinparse/blob/main/THIRD_PARTY_NOTICES.md).
  The site uses GitHub's [Octicons](https://github.com/primer/octicons) GitHub mark (MIT), and the
  results viewer uses [pdf.js](https://github.com/mozilla/pdf.js) (Apache-2.0).

## License

MIT. See [LICENSE](https://github.com/ajinkyashejul/puffinparse/blob/main/LICENSE). The third-party
crates compiled into the binaries are listed in
[THIRD_PARTY_NOTICES.md](https://github.com/ajinkyashejul/puffinparse/blob/main/THIRD_PARTY_NOTICES.md);
every release artifact (CLI archives, wheels and sdist, npm packages, and the Docker image under
`/usr/share/doc/puffinparse/`) ships `LICENSE`, that file and `THIRD_PARTY_LICENSES.txt` with each
crate's full licence text. The benchmark data keeps its own licence, stated per dataset.
