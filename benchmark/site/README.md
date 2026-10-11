# PuffinParse benchmark site

A static, framework-free viewer for the open PuffinParse benchmark and the place to verify every
claim it makes: every run, every model, every document, every score — with the page itself, each
model's output side by side, a word diff against the truth or a pass/fail checklist of every
assertion, and layout boxes when a unified response was saved.

It is built from the artefacts that are already committed to this repository
(`benchmark/results/*.json`, `benchmark/results/outputs/`, `benchmark/datasets/`) and published as
part of the product site at **`/benchmark-results/`** — `website/build.py` invokes this script (see
*Deployment* below).

```
benchmark/site/
  build.py        assembles dist/ from the committed benchmark artefacts (stdlib only)
  src/            zero-build vanilla-JS viewer: index.html, app.js, styles.css
                  (styled only through website/assets/tokens.css, see docs/DESIGN.md)
  dist/           build output (git-ignored)
```

## Build and preview locally

```bash
uv run --no-project --with pillow --with pypdfium2 python benchmark/site/build.py
python -m http.server -d benchmark/site/dist 8000   # then open http://localhost:8000
```

| Flag | Default | What it does |
|---|---|---|
| `--out DIR` | `benchmark/site/dist` | Output directory. Wiped and rebuilt on every run. |
| `--home-url URL` | empty | Where the header's "PuffinParse" link points (`website/build.py` passes the site root). Empty = the viewer's own leaderboard. |
| `--docs-url URL` | empty | Adds a "Docs" link to the header (`website/build.py` passes `/docs/`). |
| `--base-url PATH` | page-relative | URL prefix the viewer is served from, e.g. `/benchmark-results/`. It is written into `<meta name="puffinparse-base">` and onto the `styles.css` / `app.js` tags; `app.js` prefixes every `data/` URL with it. The default keeps every URL relative to the page, which works at any path served with a trailing slash. |

The build needs nothing but Python 3.9+ (plain `python benchmark/site/build.py` works). Two
optional packages make PDF inputs visible without any client-side PDF library: with
**pypdfium2** and **Pillow** (the `uv` line above; both are pip wheels) every PDF page, up to 4
per document, is rendered 1000 px wide to `<doc>.p<n>.webp` (about 120 KB a page, ~10 MB for
combined-v2); with Pillow alone only page 1 of PuffinParse's own image-only synthetic PDFs can be
extracted (`<doc>.p1.png`). Without either, PDFs are still copied and the viewer falls back to
pdf.js.

The site must be **served over HTTP** — it fetches its data with relative URLs, which browsers
block on `file://`. Routing is hash based, so no rewrite rules are needed either.

## What `build.py` produces

Everything the front-end needs lives under `<out>/data/`, so the viewer in `src/` can be swapped
for any other front-end without changing the build:

| Path | Contents |
|---|---|
| `index.html`, `app.js`, `styles.css` | copied from `src/` (only `index.html` changes, and only when `--base-url` is given) |
| `tokens.css` | the shared design tokens, copied from `website/assets/tokens.css` and linked before `styles.css` |
| `data/index.json` | every run (newest first) with its dataset info, normalisation options, `scorer_version` (when the result has one), category list, per-model summaries and an `outputs` inventory (per model slug: which documents have a unified `json`, which have no markdown — `missing` — so the viewer never probes for absent files); plus a `datasets` map with name, version, licence and description, and `labels` (`sources` / `categories`: human names such as `olmocr` → "olmOCR-bench", `headers_footers` → "Headers & footers") |
| `data/runs/<run_id>.json` | the complete result file for one run: per-model, per-document metrics, latency and cost |
| `data/outputs/<run_id>/<model>/<doc_id>.md` | each model's raw markdown output per document (`/` in a model name becomes `_`, matching `bench run --save-outputs`) |
| `data/outputs/<run_id>/<model>/<doc_id>.json` | the unified `ParseResponse`, copied when `bench run --save-outputs` saved one next to the markdown; its `pages[].blocks[].bbox` drives the layout-box overlay |
| `data/datasets/<name>/manifest.json` | the dataset manifest; each document gains `preview` / `previews` (displayable page images), `title` ("Headers & footers 3": category label + its 1-based index within source and category, in id order), `ordinal`, `source_label` and `category_label` |
| `data/datasets/<name>/truth/<doc_id>.md` | ground-truth markdown |
| `data/datasets/<name>/rules/<doc_id>.json` | the assertions of a `kind: "rules"` document |
| `data/datasets/<name>/docs/<doc_id>.<ext>` | the input documents (PNG/PDF copied as-is) |
| `data/datasets/<name>/docs/<doc_id>.p<n>.webp` | rendered pages of a PDF input (pypdfium2); `.p1.png` when only Pillow is available |

**This layout is a public, read-only API.** On puffinparse.com these files are documented at
`/docs/benchmark/data-api/` and described by the OpenAPI 3.1 document `/openapi.json`, whose
schemas are in `website/data_api.py`. Changing a path or a field here means updating those schemas
too: `website/qa.py` (CI) validates every published index, run and manifest against them.

**Manifests that point outside themselves.** `combined-v1` copies no bytes: every document is a
`../synthetic-v1/…` or `../parsebench/…` path relative to its manifest. The build resolves those
against the manifest and writes each file to the matching place under `data/datasets/`, so the same
relative URL resolves in the browser, and a file (or its rendered PDF preview) shared by two
datasets is written exactly once. Anything that would escape `data/` is refused.

**Documents with `kind: "rules"`** (the ParseBench text split) have no markdown truth: `truth` is
empty and `rules` names an assertion file, which is copied alongside the inputs. The viewer shows
the rule list where a transcript document shows its ground truth, and reports `rule_pass_rate`
wherever a result carries it. Anything the build cannot resolve — a missing input, truth, rule file
or preview — is reported on stderr and skipped; it never fails the build.

`documents[].preview` in the copied manifest always points at something a browser can display
inline (the image itself, or the rendered first page of a PDF), and `previews` lists every
rendered page; both are absent when no preview could be made. The human names live in
`build.py` (`SOURCE_LABELS`, `CATEGORY_LABELS`; anything else is sentence-cased). For `synthetic-v1` the whole directory is about 11 MB, dominated by the input images.

`build.py` picks up any `benchmark/results/*.json` that looks like a result file, so adding a new
run is a matter of committing the result JSON plus its `results/outputs/<run_id>/` directory — no
site change needed.

## The viewer

`src/` is a single-page app in plain ES2018: no framework, no build step (see the ADR "results
viewer stays vanilla"). It follows [`docs/DESIGN.md`](../../docs/DESIGN.md): one primary number
per view, everything secondary one click away in a native `<details>`, human names ("Headers &
footers 3") with the raw id in Details, verdict colour only with a CSS-drawn dot (good ≥ 90,
fair ≥ 70, poor below), best-in-column bold instead of heatmaps. PDF pages are shown from the
build-time page images; **pdf.js 3.11.174** (cdnjs, pinned SRI, worker fetched with the same
integrity check and run from a blob URL; Mozilla, Apache-2.0) is loaded only for a PDF page the
build did not render. Nothing else is fetched from outside `data/`. The header star icon is
GitHub's Octicons `mark-github-16` (MIT); see *Third-party assets* in
[`website/README.md`](../../website/README.md).

Documents tagged `fetch-required` (OmniDocBench, research-only) are listed and scored but never
copied: their page images, truth and provider outputs stay out of `dist/` even in a clone that
fetched them locally.

Every view is a shareable hash link:

- `#/leaderboard?run=…&sort=…&dir=…&x=cost|latency&cols=all` — the model table ranked by
  `summary.headline` when the result file has one (a number, or an object with
  `score`/`overall`/`value`), otherwise `overall`. Default columns: rank, model, score (with a
  thin bar), one column per source when the run has several, $/1k pages, p50 latency, and failed
  / empty outputs when any model has some. **All metrics** (`cols=all`) adds char similarity,
  CER, WER, word F1, order, table, TEDS, rules, p90 (computed in the browser with the CLI's
  nearest-rank rule), p95 and ms/page. Below it: a **score-vs-cost** (or latency) scatter with
  the Pareto frontier and collision-avoiding labels, then collapsed **By category**,
  **Methodology**, **Reproduce this run** and **Run details** disclosures. Per-document scores
  follow the scorer: character similarity, the table score on `table_only` documents, the pass
  rate on rule documents, 0 for a failed call.
- `#/documents?run=…&src=…&cat=…&q=…&pm=1` — every document by title, with its source, mean
  score and best model; **Per-model scores** (`pm=1`) swaps the best-model column for one column
  per model.
- `#/<run_id>/<model_slug>/<doc_id>?tab=…&diff=…&ov=1&page=…&rf=…` — the **inspector**:
  - header: breadcrumb, title, one meta line, `‹ n/N ›` and copy-link; tags, attribution, input
    path, original URL and the raw id (with a copy button) sit in **Details**;
  - model chips (a native select on phones) with each model's score on this document;
  - the page (sticky on wide screens) with a pager for multi-page inputs and, when a
    `<doc>.json` unified response exists for that model, **layout boxes** (`ov=1`, off by
    default) coloured by block type, drawn only over a rendered page;
  - the score, a one-line verdict ("0 of 6 checks pass", "99.1% character match", "Table 91% ·
    TEDS 0.87"), latency and cost, and **All metrics**;
  - tabs: **Checks** (rule documents) or **Diff** (word-level LCS diff, side by side or unified,
    after the scorer's normalisation), **Output**, **Truth** (transcript documents) and
    **Compare models**. Each check is one sentence ("Should not contain “ARTICLE IN PRESS”")
    with a reason line only when it adds information, grouped by type when a document mixes
    types, filterable failing / passing / all. The checks run in the browser through a
    line-for-line JS port of `score_rules` / `normalize` / `markdown_to_text` in `puffinparse-core`
    (scorer v2); the result is compared with the recorded score and flagged if they ever differ
    (the recorded Rust score stays authoritative). Keep the port in step with `SCORER_VERSION`.
  - **Reproduce this score**: the exact `puffinparse parse` / `bench score` (or `bench run
    --filter`) commands and links to every file behind the page.
  - Research-only sources (tag `fetch-required`, e.g. OmniDocBench) show scores only and the
    fetch command; none of their files is requested.
- Old `#/doc/<id>?run=…&model=…` links are redirected to the inspector.

Keyboard: `j`/`k` next/previous document (within the current source/category/filter), `m`/`M`
cycle models, `1`–`4` tabs, `d` split/unified diff, `o` layout boxes, `[`/`]` pages, `g l` /
`g d` leaderboard / documents, `?` help. Light and dark follow the product site (`data-theme`
and the shared `puffinparse-theme` key, with a toggle in the header); the layout works down to phone
width with no horizontal page scroll (wide tables scroll inside their frame).

## Deployment

The viewer ships with the product site, at
[`https://puffinparse.com/benchmark-results/`](https://puffinparse.com/benchmark-results/).
`website/build.py` imports this script and runs it with
`--out <dist>/benchmark-results --base-url /benchmark-results/` as part of every build
(`--no-benchmark` skips it). Nothing in `vercel.json` is specific to the viewer beyond `--with
pillow --with pypdfium2` in the build command, which is what renders the PDF page images.

`.github/workflows/pages.yml` builds the *whole* site the same way, so GitHub Pages would mirror
Vercel — landing page, `/docs/` and `/benchmark-results/` — rather than publishing the viewer on
its own. **A maintainer must enable Pages once**: repository *Settings → Pages → Source: GitHub
Actions*. Until then the build succeeds and the deploy step fails.
