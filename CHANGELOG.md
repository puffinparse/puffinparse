# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The workspace crates (`puffinparse-core`, `puffinparse-cli`, `puffinparse-python`, …) and the
Python package `puffinparse` share a single version. Entries before the rename say LiteOCR.

## [Unreleased]

### Security

- Real provider ids are gone from committed test data: the Extend run, parse-run and file ids and
  the dashboard link, the LlamaParse extract/parse job, file and project ids, and the Reducto job
  ids and studio links in the live-captured fixtures (and the matching samples in
  `docs/providers/extend.md` / `reducto.md`) are now obviously fake ids of the same prefix and
  length. The 556 `studio.reducto.ai/job/<id>` links in the saved raw responses of runs
  `run-20260924T211006Z` and `run-20260925T090851Z` point at a zero UUID; the scored `.md` outputs
  are untouched and `bench rescore` reproduces every score. Benchmark results keep
  `provider_job_id` for auditability, as `SECURITY.md` now states.
- **Document URLs are downloaded through one hardened path** (`puffinparse_core::fetch`). Providers
  without URL input (Tesseract, Docling, PaddleOCR, vLLM, Unstructured, Textract, Gemini, the
  OpenAI and Anthropic vision models, Upstage, Document AI, LlamaParse extract) fetched
  `document_url` in-process with no address filtering and reqwest's default 10 redirects, so a
  caller-chosen URL could reach `169.254.169.254`, localhost or the private network (SSRF). The
  download now accepts http(s) only, resolves the host itself and refuses loopback, private,
  link-local, CGNAT, unique-local, multicast, broadcast and reserved addresses (including
  IPv4-mapped and NAT64/6to4 forms), connects only to the vetted address (no DNS rebinding),
  ignores proxy variables, follows at most 5 redirects re-checking each hop, stops at 50 MiB
  (`PUFFINPARSE_MAX_DOWNLOAD_MB`) and the request deadline, and never echoes the response body in
  an error (Gemini, Upstage and Document AI used to include part of it).
- `DocumentRequest`'s `Debug` output no longer prints `api_key` or `webhook_url` (which may carry a
  shared secret), nor the document bytes.
- Gateway hardening (`docs/SERVER.md#hardening`): `document_url` is no longer downloaded inside the
  gateway by default; self-hosted engines are no longer open to every key; concurrency, a
  per-request time limit and a header read timeout are enforced; `/metrics` needs a key; the
  gateway refuses to start unauthenticated on a public address; `Debug` of the gateway's config,
  deployments, virtual keys and state redacts every secret. The "do not expose yet" warning is gone.

### Changed

- **Behaviour change for SDK and CLI users:** a URL input to one of the providers above that
  points at a private, loopback or link-local address now fails with `InputError`. Set
  `PUFFINPARSE_ALLOW_PRIVATE_URLS=1` in a trusted setup to allow it (process-wide; there is no
  per-request switch). Downloads over 50 MiB fail unless `PUFFINPARSE_MAX_DOWNLOAD_MB` is raised.
- **Breaking for gateway operators** (`puffinparse serve`):
  - The gateway refuses to start on a non-loopback `host` (for example the Docker image's
    `0.0.0.0`) without `master_key` or `[[keys]]`; set `server.allow_unauthenticated = true` if a
    proxy in front already authenticates every caller.
  - `document_url` with a model whose provider cannot fetch URLs (Tesseract, Docling, PaddleOCR,
    vLLM, Unstructured, Textract, Gemini, OpenAI, Anthropic, Upstage, Document AI, LlamaParse
    extract) is rejected with `400 input_error`, also when only an alias target or a request
    fallback would download. Uploads and URL-capable providers (Reducto, Extend, LlamaParse parse,
    Mistral, Azure, Datalab, Mathpix, Landing AI, OpenDocRouter) are unaffected. New
    `server.fetch_document_urls = true` restores it, with public addresses only
    (`server.allow_private_document_urls`, `server.max_download_mb`); the gateway ignores
    `PUFFINPARSE_ALLOW_PRIVATE_URLS`.
  - Self-hosted engines named directly (`tesseract`, `docling/default`, ...) get `403
    model_not_allowed` unless the key's `models` names them (`"*"` or an empty list does not
    count), the master key calls, or `server.allow_local_engines = true`. Aliases targeting them
    work as before. `GET /v1/models` hides them from keys that cannot use them.
  - `GET /metrics` needs a gateway key (any virtual key or the master key) unless
    `server.public_metrics = true`; point the Prometheus scraper at it with a bearer token.
  - New limits: `server.max_concurrent_requests` (default 64 document requests in flight, the
    rest wait), `server.request_timeout_secs` (default `max_timeout_secs` + 60; then `504
    timeout_error`), `server.header_read_timeout_secs` (default 30).
  - `server.allow_direct_models` keeps its default (`true`); give every key an explicit `models`
    list.
- Docling and PaddleOCR no longer forward a URL input to docling-serve / the PaddleOCR server
  (which would fetch it from inside your network); PuffinParse downloads it through the same path
  and sends the bytes. PaddleOCR's `fileType` now always comes from the downloaded bytes.
- CI (and `make lint`) runs `scripts/third_party_notices.py --check`, which now also fails when a
  bundled crate has no licence text or `licenses/extra/` holds an unused entry.
- `pyproject.toml` uses an SPDX `license = "MIT"` with `license-files` (core metadata 2.4) and
  requires maturin >= 1.9.3.

### Added

- `puffinparse bench report --by-dataset [--intro FILE]` renders the whole leaderboard page: one
  section per dataset, the newest run's dataset first as the headline. `make leaderboard` uses it
  (the hand-written introduction lives in `benchmark/LEADERBOARD.intro.md`), and a test fails when
  `benchmark/LEADERBOARD.md` is out of date with the committed results. `make leaderboard` used to
  overwrite the page with one merged table.
- Benchmark data API: the results viewer's static JSON (leaderboard index, full runs with
  per-document scores, per-document model outputs, dataset manifests) is documented as a read-only
  API at `/docs/benchmark/data-api/` with curl examples, described by an OpenAPI 3.1 document at
  `/openapi.json` (schemas derived from the published files, examples taken from the newest run)
  and listed in `llms.txt`.
- `/.well-known/api-catalog` (RFC 9727 Linkset, served as `application/linkset+json` with the
  RFC 9727 profile and a `Link: rel="api-catalog"` header) and RFC 8631 `service-desc` /
  `service-doc` links plus `rel="api-catalog"` in the head of every page, including the landing
  page and the results viewer.
- Site QA in CI: a `site` job builds the site with `--check` and runs `website/qa.py`, which checks
  offline that every docs page has a Markdown negotiation route, `llms.txt` is well formed and its
  links resolve, JSON-LD parses on every page, `robots.txt` names the AI crawlers and a sitemap,
  `/openapi.json` is valid, its example paths exist and the published JSON matches its schemas,
  and the API catalog is a valid Linkset.
- The gateway image is multi-arch: linux/amd64 and linux/arm64, each built natively (GitHub's
  `ubuntu-24.04-arm` runner for arm64, no QEMU) and joined into one manifest list, so
  `docker pull ghcr.io/ajinkyashejul/puffinparse` gets a native image on Apple Silicon and Graviton.
- Licence files in every release artifact: the CLI archives, the wheels and sdist (PEP 639
  `license-files`, in `.dist-info/licenses/`), every npm package and the Docker image
  (`/usr/share/doc/puffinparse/`) carry `LICENSE`, `THIRD_PARTY_NOTICES.md` and
  `THIRD_PARTY_LICENSES.txt` (each bundled crate's full licence text). The 11 crates that publish no
  licence file have theirs under `licenses/extra/`, fetched from the crate's repository at the
  published commit; r-efi's notices come from its `AUTHORS` file. `scripts/check_release_licenses.py`
  fails a release build whose artifact lacks them.

## [0.1.6] - 2026-10-08

### Changed

- Benchmark scorer v3 (`SCORER_VERSION = 3`): single `*` / `_` emphasis is stripped, inline HTML
  tags (`<sup>`, `<i>`, …) no longer split words, dot leaders collapse to a space, figure markup and
  markdown images are dropped from transcript predictions (the truths carry no figure content), and
  `table_cell` rules can match a table's header row. The results viewer's in-browser rule checker
  mirrors all of it. Every committed run was re-scored offline: `combined-v3` is now led by
  `llamaparse/cost_effective` 85.63 and `llamaparse/agentic` 85.28 (a statistical tie), then
  `reducto/r-1` 82.80, `extend/parse_performance` 80.77, `reducto/standard` 80.29,
  `extend/parse_light` 79.89 and `tesseract/default` 56.41. OmniDocBench's 40 documents keep their
  v2 scores (outputs not committed). Details in `docs/benchmarks/findings.md`.

## [0.1.5] - 2026-10-08

### Added

- OpenDocRouter provider (`opendocrouter/<vendor>/<model>`, 11 models, `OPEN_DOC_ROUTER_API_KEY`):
  LlamaIndex's hosted parsing router, docs-only. Layout elements become typed blocks with boxes,
  `cost_usd` is the response's actual `charge_usd`, partial results keep the pages that worked and
  list the failed ones in `metadata.opendocrouter_failed_pages`, files over ~3 MB go through the
  uploads flow, documents over 50 pages run async, and `submit` / `retrieve` jobs are supported.
- Gateway key allow-lists accept nested prefixes such as `opendocrouter/google/*`.
- Frontier vision models in the existing providers (docs-only, from each vendor's model and pricing
  pages, checked 2026-10-08): `anthropic/claude-opus-5-5`, `anthropic/claude-haiku-5-5`,
  `gemini/3-flash-preview`, `gemini/3.8-flash-low` (Gemini 3.8 Flash with
  `thinkingLevel: "low"`) and `openai/gpt-6-luna`, with per-page estimates in `pricing.json` and
  exact token prices (including Haiku 5.5's over-100k-token tier) in the providers.
- `vllm` provider (self-hosted, $0): open document-parsing VLMs on your own `vllm serve` /
  OpenAI-compatible server (`VLLM_BASE_URL`, optional `VLLM_API_KEY`). Presets
  `vllm/infinity-parser2-flash` and `vllm/dots.mocr` send each page image (PDFs rasterised with
  `pdftoppm`) with the model card's prompt and sampling, and turn the layout JSON into typed
  blocks with normalised boxes. Implemented from the model cards; not yet run against a server.
- `paddleocr/vl`: the PaddleOCR-VL pipeline (PaddleOCR-VL-1.6 by default) through its
  `/layout-parsing` serving API (`PADDLEOCR_VL_BASE_URL`); `ocr` is derived from `parse`.

### Changed

- Anthropic: models that reject a forced `tool_choice` (Opus 5.5, Sonnet 5.5, Fable 5.1, Mythos 5.1)
  now get `tool_choice: auto` with a strict parse tool and an instruction to call it, decided from
  the model id actually sent (so a `provider_options.model` override is covered too).
- The `pdftoppm` rasterisation and local-tool runner moved from the Tesseract provider to
  `providers/local.rs` so the vLLM provider shares them.

## [0.1.4] - 2026-10-08

### Security

- Gateway: `provider_options.cmd` and `provider_options.pdftoppm_cmd` are rejected (400). They
  choose the program the Tesseract provider runs, so any key holder could run commands on the
  gateway host. Operators keep `TESSERACT_CMD` / `PDFTOPPM_CMD`.
- Textract `region` and Google Document AI `location` must be a single DNS label. Both are spliced
  into the provider hostname, so a crafted value could send the signed request (Textract) or the
  OAuth access token (Document AI) to another host.

## [0.1.3] - 2026-10-08

### Fixed

- Tesseract: a corrupt PDF or a page range past the end is now an `InputError` (not retryable),
  so a `Router` no longer falls through to other models and hides the real cause; tool stderr in
  error messages is cut to its first lines (a corrupt PDF produced hundreds), and exit 127 in
  minimal containers is reported as a missing binary.

### Added

- `THIRD_PARTY_NOTICES.md` (generated by `scripts/third_party_notices.py`, which also fails on a
  copyleft-only dependency and writes the full licence texts with `--full`), an Acknowledgements
  section in the README, BibTeX citations and licence files for the benchmark datasets, provenance
  notes for the test fixtures, and source credits for the TEDS, olmOCR and OmniDocBench logic the
  scorer and adapters mirror.

### Changed

- The results viewer build never copies documents tagged `fetch-required` (OmniDocBench,
  research-only), nor provider outputs for them, even in a clone that fetched them locally.

## [0.1.2] - 2026-10-08

### Fixed

- Release versions now match across all manifests (`js/package-lock.json` still said 0.1.0), so
  the release pipeline publishes again; 0.1.1 was tagged but never published.

### Added

- Agent-friendly website: docs URLs return their Markdown for `Accept: text/markdown` (with
  `Vary: Accept`, and a Markdown 404 for unknown docs paths); one schema.org JSON-LD block per page
  (`SoftwareApplication`, `WebSite` with a docs-search `SearchAction` backed by `/docs/?q=`,
  `TechArticle`, and licensed `Dataset` entries on the benchmark viewer); `robots.txt` names the
  major AI crawlers explicitly; `llms.txt` gains when-to-use guidance and the benchmark JSON
  endpoints; and a new `/docs/agents/` page, also served as `/agents.md`, with a copyable
  onboarding prompt.

## [0.1.1] - 2026-10-08 [not published]

### Fixed

- The source distribution now includes the LICENSE file its metadata names; PyPI rejected the
  0.1.0 sdist for this, so 0.1.0 shipped wheels only.

## [0.1.0] - 2026-10-08

First public release (PyPI wheels, CLI binaries, gateway image).

### Changed

- Provider verification labels are consistent across the README, the provider reference, the
  landing page and the docs site: **live-verified** (Reducto, Extend, LlamaParse), **verified
  locally** (Tesseract, Docling) and **docs-only** (the rest, implemented from documentation and
  tested against fixtures, not yet run live; tracked in issue #10). Tesseract and Docling were
  previously counted as live-verified on the landing page.
- README roadmap now lists the open GitHub issues; added issue templates for benchmark and
  dataset suggestions, a shorter pull-request template, `AGENTS.md`, and private vulnerability
  reporting in `SECURITY.md`.

- Benchmark and docs tables use sentence-case headers, and a column's best value is bold only
  when it is unique as displayed (ties are no longer bolded).

- **Renamed to PuffinParse** (ADR-23). Crates `puffinparse-*`, Python package `puffinparse`
  (`import puffinparse`), Node package and CLI binary `puffinparse`, environment variables
  `PUFFINPARSE_*` (was `LITEOCR_*`), metadata keys `puffinparse_*`, `PuffinParseError`, and
  `output_format="puffinparse"` for the native shape. The site moves to puffinparse.vercel.app
  (liteocr.vercel.app still works). Old result files with `liteocr_version` still load.

### Added

- **Release pipeline** (`docs/RELEASING.md`). A `v*` tag publishes abi3 wheels and an sdist to PyPI,
  `puffinparse` plus five prebuilt platform packages to npm, `puffinparse-core`/`-server`/`-cli` to
  crates.io, the gateway image to `ghcr.io/ajinkyashejul/puffinparse`, and CLI archives with
  `SHA256SUMS` to GitHub Releases, all through trusted publishing (no stored registry tokens).
  npm and crates.io are switched on per repository variable once their one-time setup is done.

- **Tesseract baseline in `combined-v3`.** `tesseract/default` (Tesseract 5.5.1, one OpenMP
  thread per process) was added to the 2026-09-25 run with `bench run --resume`: 199 documents,
  0 failures, Overall 56.36 (dpbench 86.77, olmocr 41.67, omnidocbench 35.70, parsebench 20.73,
  synthetic 97.98). The six API rows are unchanged. Licence audit notes in the DP-Bench README
  (5 of the 40 vendored pages are Upstage's own documents, kept under its MIT declaration) and
  the olmOCR README (redistribution with attribution complies with ODC-BY and AI2's guidelines).

- **Puffin brand** (ADR-24, `docs/DESIGN.md` Brand): a puffin mark (favicon and header logo on the
  landing page, docs and benchmark viewer) and a waving puffin mascot with three pages in its beak, shown
  on the landing hero, the 404 page and the README. Every page now has an Open Graph / Twitter
  social card (`website/assets/og.png`). The landing demo shows a response on arrival instead of
  starting blank.

- **First `combined-v3` results** (6 API models × 199 documents, adds a 40-page DP-Bench subset;
  0 failures, $14.65): llamaparse/cost_effective leads (84.35) ahead of llamaparse/agentic (83.49)
  and reducto/r-1 (82.40). It is the new headline in `benchmark/LEADERBOARD.md` and the viewer;
  `combined-v2` keeps the Tesseract baseline row.

- **Design language** (`docs/DESIGN.md`, `website/assets/tokens.css`): paper-and-ink neutrals, one
  scan accent, verdict and proof-mark colours, layout-box hues, a system-font type scale, the
  bounding-box and scan-line signatures; every text token passes WCAG AA in both themes. The docs,
  landing page and benchmark viewer all style through it.
- **Benchmark viewer redesign.** Human document titles ("Headers & footers 3") with raw ids in
  Details; one primary number per view with secondary metrics, methodology and reproduce commands
  in disclosures; a compact leaderboard with per-source columns and an All metrics toggle; a calmer
  score-vs-cost chart with non-overlapping labels; plain-English check rows; a documents list with
  mean score and best model. PDF pages are rendered at build time with pypdfium2, so olmOCR-bench
  and ParseBench pages show without pdf.js.
- **First `combined-v2` results** (7 models × 159 documents, 0 failures, $11.74): llamaparse/cost_effective
  leads (83.79) ahead of llamaparse/agentic (83.05) and reducto/r-1 (81.32); `tesseract/default` is
  the free baseline (48.68, 97.94 on synthetic). `benchmark/LEADERBOARD.md` now has one section per
  dataset with `combined-v2` as the headline. The viewer shows research-only documents
  (OmniDocBench) as scores only, with the command to fetch the data, instead of broken links.
- **Jobs everywhere.** Gateway: `POST /v1/jobs` (parse body + `webhook_url`, 202 with a job id) and
  `GET /v1/jobs/{id}` (pending / succeeded / failed, `output_format` on retrieval); job ids are
  bound to the submitting key, cost is charged once when the job first succeeds, handles persist in
  `state_file` (`server.job_retention_hours`), and an opt-in `POST /v1/webhooks/{provider}` receiver
  (`[webhooks] enabled`, shared secret) settles jobs from provider webhook bodies. New
  `liteocr_jobs_total` metric and `job_id`/`job_status` log fields. Node SDK: `submit`, `retrieve`,
  `handleWebhook` with a typed `Job`.
- **DP-Bench** (Upstage, MIT) adapter: 40 committed single-page PDFs with reading-order transcript
  truth (`python -m benchmark.adapters dpbench`, pinned at `24702c61`), and `combined-v3` =
  combined-v2 + DP-Bench (199 documents). Headers and footers stay in the DP-Bench truth because
  DP-Bench's own NID scores them; each source keeps its publisher's conventions.
- `tesseract/default` runs `tesseract` with `OMP_THREAD_LIMIT=1` unless set, so parallel pages no
  longer oversubscribe the CPU (70 s → 0.7 s for a small page on 4 cores).
- Benchmark results flag a successful call that returned no text as `empty_output: true` (counted in
  `summary.empty_outputs`, shown as `(+N empty)` in the leaderboard), and `bench rescore
  --keep-missing` keeps the recorded scores of documents whose outputs are not committed.
- **Scorer v2** (`scorer_version: 2` in result JSON). HTML `<table>` output is scored like markdown
  tables (it used to score 0, which ranked Reducto r-1 last); a new `teds_grid` metric (TEDS on the
  row/cell grid); rule matching ignores spaces next to punctuation (ParseBench rule text is
  tokenised); `bag_of_sentences` matches sentences fuzzily (0.8) and passes at 0.8 (1.0 was dead
  signal); olmOCR `max_diffs` tolerances are honoured; HTML entities are decoded; and results carry
  an explicit `summary.headline` and per-document `headline`, so `char_similarity` means literal
  character similarity again. Python and Node `Metrics` gain `teds_grid` / `tedsGrid`, and the
  viewer's rule checker follows the same rules.
- **`liteocr bench rescore`** re-scores a run from its saved outputs with the current scorer, no
  network calls, keeping measured latency and cost. Both committed runs are re-scored and
  `benchmark/LEADERBOARD.md` regenerated: on combined-v1, reducto/r-1 moves from last (87.19) to
  second (91.37); llamaparse/agentic stays first (93.08).
- `docs/benchmarks/findings.md`: why four models returned nothing for
  `parsebench/text_multicolumns_2col` (a Form XObject with a ±2^1023 `/BBox` that 32-bit
  renderers clip to empty; fixing the file fixes both Reducto and Extend, no option does).
- **Self-hosted engines, no key, $0/page.** `tesseract/default` (local `tesseract` binary, PDFs via
  `pdftoppm`; native OCR with word/line boxes and confidences), `docling/default` (docling-serve v1
  async API; layout, tables, OCR) and `paddleocr/default` (PaddleOCR/PaddleX serving: `/ocr` and
  PP-StructureV3 `/layout-parsing`; docs-only). Tesseract and Docling are live-verified locally.
  `liteocr providers` shows `local` for them; `--json` and Python `providers()` include
  `self_hosted`.
- README, docs site and landing page cover the TypeScript SDK, the gateway, jobs/webhooks,
  self-hosted engines and the new benchmark sources; the docs navigation gains the gateway,
  the self-hosted provider pages, the academic-benchmark survey and the adapter notes.
- **Async jobs API and webhooks.** Split a parse into submit and retrieve so the caller owns the
  waiting (long documents, batches, webhook-driven pipelines). Rust: `submit_parse`,
  `retrieve_parse` / `retrieve_parse_with`, `parse_webhook`, `resolve_webhook`, `JobHandle`
  (serialisable, never holds a key), `JobStatus`, and `DocumentRequest.webhook_url`. Python:
  `liteocr.submit` / `asubmit`, `retrieve` / `aretrieve`, `handle_webhook` / `ahandle_webhook` and
  `liteocr.Job`. Covers Reducto, Extend and LlamaParse (live-verified); `webhook_url` maps to
  Reducto `async.webhook` and the LlamaParse `webhook_url` field, and is rejected for Extend, which
  only has workspace-level webhooks. SPEC §15.
- **Benchmark viewer: verify every claim.** A per-document inspector at `#/<run>/<model>/<doc>` shows
  the page itself (pdf.js for PDFs, pinned with SRI, PNG fallback), every model's output side by
  side, a word diff against the truth, and a pass/fail rule checklist for rules documents that is
  checked against the recorded score. Layout-box overlays by block type appear when a run saved
  unified responses. The leaderboard ranks by `summary.headline` when present and adds p90 latency,
  a score-vs-cost/latency scatter with the Pareto frontier, and per-source and source × category
  breakdowns. Every view is a shareable link, with keyboard navigation (`j`/`k`, `m`, `1`–`4`,
  `d`, `o`, `?`), the product site's design and theme switch, and a mobile layout.
- `liteocr bench run --save-outputs` also writes the unified response as `<doc>.json` next to
  `<doc>.md`, which the viewer uses for its layout overlay.
- **More public benchmarks in the combined dataset.** `olmocr` (40 AI2 olmOCR-bench PDFs, 205
  rules, ODC-BY-1.0) and `omnidocbench` (40 pages across 10 document types, English and Chinese;
  index only — images and truth are fetched at a pinned revision by
  `python -m benchmark.adapters omnidocbench` because the data is research-only), joined with
  synthetic-v1 and ParseBench as `combined-v2` (159 documents; `combined-v1` is unchanged).
  Upstream tests that cannot be expressed faithfully (math, baseline, positional absences,
  vertical table neighbours) are skipped and counted in `conversion-stats.json`, never dropped
  silently. A dataset self-check test proves every converted rule is satisfiable. Survey of
  academic benchmarks with licences at pinned revisions: `docs/benchmarks/academic-benchmarks.md`.
- **Gateway server: `liteocr serve` (`crates/liteocr-server`).** An HTTP gateway in front of every
  provider, the LiteOCR equivalent of the LiteLLM proxy: `POST /v1/parse|ocr|extract` (JSON or
  multipart, `output_format`, `fallbacks`), `GET /v1/models`, `/v1/usage`, `/health` and
  Prometheus `/metrics`. A `liteocr.toml` config defines model aliases with ordered or round-robin
  fallback, provider keys as `env:` references, and virtual keys with model allow-lists, monthly
  USD budgets and per-minute rate limits. Every request is logged as one JSON line (never document
  content, provider error text or secrets), and all errors share one JSON body. Clients cannot
  override `api_key`/`base_url` or read local files. Ships a multi-stage distroless `Dockerfile`
  and `examples/server/liteocr.toml`; see [`docs/SERVER.md`](docs/SERVER.md).
- **Node.js / TypeScript SDK.** The `liteocr` npm package in `js/` runs on a napi-rs addon over the
  same Rust core (`crates/liteocr-node`): async `parse` / `ocr` / `extract` (with `fallbacks`),
  `Router`, camelCase typed responses (`index.d.ts`), `LiteOCRError` subclasses mapped from the core
  `ErrorKind`, and the pricing, model and scoring helpers. CI builds the addon and runs the
  typecheck and `node:test` suite; the release workflow builds prebuilt `.node` binaries as
  artifacts (npm publishing not wired yet, so build from source for now). Docs at `/docs/typescript/`.
- **`liteocr bench run --resume`**: every finished call is appended to `<out>.partial.jsonl`, so an
  interrupted or partly failed run continues where it stopped and only re-runs missing or failed
  (model, document) pairs. `--dry-run` prints the plan (calls, pages, list-price estimate) without
  calling any provider; `--max-cost <usd>` aborts before the first call when the estimate is higher;
  `--retries N` re-issues documents after retryable errors (off by default, since a retried job may
  be billed twice). Result documents record `provider_job_id`, `cache_hit`, `attempts`,
  `started_at` and `error_kind`, and the run ends with a summary line (calls, failures, cost, wall time).
- **Native-format compatibility (`output_format`).** A response can be rendered in a
  provider's own JSON shape instead of the unified one, so an integration already written
  against Reducto, Extend or LlamaParse can switch the underlying provider without
  rewriting its parsing code: `ParseResponse::to_format("reducto")` in Rust
  (`liteocr_core::compat::{Format, render_parse, render_extract}`), with
  `DocumentRequest.output_format` carrying and validating the choice. What is guaranteed
  is structural fidelity — key set, chunk/page and block counts, content strings, block-type
  vocabulary, coordinate units and billed pages — not byte equality; the always-null fields
  and the lossy type mappings are enumerated in [`docs/COMPAT.md`](docs/COMPAT.md), and each
  provider's fixture is round-tripped through its own renderer in the test suite. Extract-mode
  rendering is best effort. See ADR-13.
- **`output_format` in the Python SDK and the CLI.** `liteocr.parse(..., output_format="reducto")`
  (and `aparse`, `extract`, `aextract`, plus the matching `Router` methods) returns the vendor's
  own JSON as a `dict` instead of the dataclass; `output_format=None` (the default) or
  `"liteocr"` keeps the unified shape. The value is validated in the core before any network call
  — an unknown name raises `BadRequestError` listing `liteocr | reducto | extend | llamaparse` —
  the rendering happens in Rust (`liteocr._core.render_parse` / `render_extract`), and success
  callbacks still receive the dataclass. Overloads type the return (`None` → dataclass, `str` →
  `dict`), `liteocr.output_formats()` lists the accepted values, and
  `examples/switch_provider_keep_format.py` shows a provider swap with the parsing code untouched.
  The CLI gains `--output-format <vendor>` on `parse` (with `--format json`; ignored with a
  warning on stderr otherwise) and on `extract`, and `liteocr providers --json` now emits
  `{"providers": [...], "output_formats": [...]}` instead of a bare array.

### Changed

- Internal: the vision-LLM providers (Gemini, OpenAI, Anthropic) share `providers/vlm.rs`; behaviour
  and request bodies are unchanged.
- Adapter HTML→markdown table conversion no longer doubles backslashes, so LaTeX in table cells reaches
  the scorer as a parser would print it.
- **Modes.** Every call now names a mode — `parse` (markdown + typed blocks), `ocr`
  (plain text with line/word boxes) or `extract` (a JSON object from a schema, with
  per-field confidence and citations) — and providers can only be swapped within a
  mode. The Python SDK exposes `parse`/`aparse`, `ocr`/`aocr` (now plain text, not
  markdown) and `extract`/`aextract`, the new `TextResponse` / `ExtractResponse`
  dataclasses, a mode-bound `Router(models, mode=...)`, and mode arguments on
  `list_models`, `resolve_model`, `set_pricing` and `estimate_cost`; pricing is now
  per page *per mode*. The CLI gains `liteocr ocr` and `liteocr extract`, and
  `liteocr providers` shows modes, per-mode prices and a `--mode` filter. The old
  `liteocr.ocr` (which returned markdown) is now `liteocr.parse`, and `OcrResponse`
  is now `ParseResponse`. Which models serve which mode is reported by
  `liteocr.list_models(mode)` and `liteocr providers --mode <mode>`.
- **README model table.** The "Model names" section is regenerated from the registry and
  `pricing.json`: every provider group carries its env var and verification status
  (live-verified vs docs-only), every model its modes, the default per mode and the list price
  per page for `parse · ocr · extract`.

### Fixed

- Extend `provider_options={"responseType": "url"}` is now sent as the `GET /parse_runs/{id}` query
  parameter; it used to go in the request body, so the presigned-output path never triggered.
  Reducto and Extend url-typed results are now covered by live-captured fixtures and loopback tests.

## Initial development - 2026-09-11

Initial release.

### Added

- **Rust core (`liteocr-core`)** — a unified request/response contract for
  document parsing: `OcrRequest` accepting a path, bytes or URL, and an
  `OcrResponse` with `pages`, `blocks` (normalised block types and 0..1
  bounding boxes), document-level `markdown`/`text`, `usage`, `cost_usd` and
  `latency_ms`, identical across every provider. `#![forbid(unsafe_code)]`.
- **Providers** — Reducto, Extend and LlamaParse, each talked to over plain
  HTTPS with no vendored SDK, addressed by `"<provider>/<model>"` model
  strings (`reducto/standard`, `extend/parse_performance`,
  `llamaparse/cost_effective`, …) with a per-provider default and a
  `provider_options` pass-through escape hatch.
- **Router** — ordered fallbacks and round-robin load balancing across models,
  with per-model success/failure and latency stats; only provider, rate-limit
  and timeout errors trigger fallback.
- **Pricing and cost tracking** — an embedded, overridable `pricing.json`
  price table mapping model → per-page (or per-credit) USD, used to compute
  `cost_usd` on every response.
- **Reliability primitives** — retries with exponential backoff on 429/5xx and
  network errors, per-call timeouts, and whole-call deadlines that cover
  upload plus polling.
- **Benchmark metrics** — character similarity (the primary score), CER, WER,
  word-level F1/recall, reading-order agreement and a table-restricted score,
  all computed in Rust over NFKC-normalised text.
- **CLI (`liteocr`)** — `parse` (markdown/text/json output, optional raw
  provider payload), `providers` (models, pricing and API-key status), and
  `bench run` / `bench report` / `bench score` for running the benchmark,
  scoring results and generating the leaderboard.
- **Python SDK (`liteocr`)** — `ocr()`, the async `aocr()`, `Router`, typed
  dataclasses (`OcrResponse`, `Page`, `Block`, `BBox`, `Usage`), the full error
  hierarchy, `set_pricing()`, `list_models()`, and success/failure callbacks.
  Fully typed, ships `py.typed`, distributed as abi3 wheels for CPython 3.9+.
- **`synthetic-v1` benchmark dataset** (v1.1.0) — 39 deterministically generated
  documents across 13 categories (plain, headings, invoice, table, two-column,
  noisy scan, low resolution, multi-page PDF, skewed, dense, faded, receipt,
  complex table) with exact markdown ground truth, plus the generator that
  produces them byte-for-byte.
- **First leaderboard** (`benchmark/LEADERBOARD.md`) from a run over seven models
  across the three providers; `bench run` disables provider result caches by
  default (`--allow-cache` to opt out) so latency reflects real work.

[Unreleased]: https://github.com/ajinkyashejul/puffinparse/compare/v0.1.6...HEAD
[0.1.6]: https://github.com/ajinkyashejul/puffinparse/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/ajinkyashejul/puffinparse/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/ajinkyashejul/puffinparse/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/ajinkyashejul/puffinparse/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/ajinkyashejul/puffinparse/compare/v0.1.0...v0.1.2
[0.1.1]: https://github.com/ajinkyashejul/puffinparse/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ajinkyashejul/puffinparse/releases/tag/v0.1.0
