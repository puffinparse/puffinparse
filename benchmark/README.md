# PuffinParse Open Benchmark

A reproducible benchmark that ranks OCR / document-parsing providers on **accuracy**,
**latency** and **cost**, using the same unified client as the SDK. Results are committed
to `results/` and rendered into [`LEADERBOARD.md`](LEADERBOARD.md).

## Principles

1. **Exact ground truth.** Documents in `synthetic-v1` are rendered from the same source the
   truth markdown is written from, so there is no annotation noise. The generator is seeded
   and byte-reproducible (`python benchmark/generate_synthetic.py`).
2. **Deterministic metrics.** No LLM judge is needed. Everything is computed in Rust
   (`puffinparse_core::bench`) from the prediction and the truth after normalisation.
3. **Tied to a dataset revision.** Every result file records the SHA-256 of the manifest plus
   every input and truth file, the PuffinParse version, the models and the normalisation options.
4. **Three axes.** Accuracy, latency (p50 / p95 / ms per page as observed from the client, which
   includes upload and polling), and cost per 1,000 pages from the public list price of each model.

## Metrics

Normalisation: NFKC, markdown syntax and HTML tags stripped (headings, emphasis, list bullets,
table pipes and separator rows), HTML entities decoded, curly quotes and dashes straightened,
whitespace collapsed, lowercased (unless `--case-sensitive`).

| Metric | Definition |
|---|---|
| **Overall** | `100 × summary.headline`: the mean over documents of each document's headline metric (`char_similarity`, or `table_score` for `table-only`, or `rule_pass_rate` for `kind: rules`); a failed call scores 0 |
| `char_similarity` | `1 − levenshtein(pred, truth) / max(|pred|, |truth|)`; the summary value is the literal mean, never the headline |
| `cer` | `levenshtein(pred, truth) / |truth|` |
| `wer` | word-level Levenshtein over whitespace tokens `/ |truth words|` |
| `word_recall`, `word_precision`, `word_f1` | bag-of-words overlap |
| `order_score` | Kendall-τ-style fraction of concordant pairs among lines present in both texts (reading order) |
| `table_score` | `char_similarity` restricted to table rows (only when the truth has tables). Markdown pipe tables and HTML `<table>`s (with `colspan`/`rowspan` repeated into every slot, like the ParseBench truth) are both read into rows of cells |
| `teds_grid` | TEDS (tree-edit-distance similarity, Zhong et al. 2020) on the `table > row > cell` grid: `1 − TED / max(nodes)`, cell renames cost their normalised Levenshtein distance. Structure-aware where `table_score` is not: a merged or split row or column costs here even when the text is all there. It is TEDS without `thead`/`tbody` and span attributes, since the truth is markdown; each truth table is compared with its best-matching predicted table |
| `rule_pass_rate` | `passed / total` over a rule-scored document's assertions (only for `kind: rules`) |

## Document kinds

A dataset document declares how it is scored (`kind` in the manifest, default `transcript`;
see [`docs/benchmarks/adapters.md`](../docs/benchmarks/adapters.md)):

| `kind` | Truth | Scored by |
|---|---|---|
| `transcript` | `truth`, a markdown file | the metrics above, headlined by `char_similarity` |
| `rules` | `rules`, a JSON list of machine-checkable assertions (`present`, `absent`, `order`, `table_cell`, `bag_of_sentences`) | `rule_pass_rate = passed / total`, which takes the place of `char_similarity` so the document aggregates with the rest |

Two per-document adjustments follow from that:

- A **`rules` document has no markdown truth.** The runner reads its rule file instead, and a
  document whose rules cannot be read or parsed fails with `rules unreadable: …` — without
  spending a provider call.
- A **`table-only` document** (ParseBench's table split: the truth is the page's table, the
  prediction is the whole page) is headlined by `table_score` instead of `char_similarity`, so
  `Overall` means the same thing for it as for every other document. `char_similarity`, `cer`
  and `wer` are still recorded, and are still systematically bad on those documents by
  construction.

## Running

```bash
cargo build --release -p puffinparse-cli
./target/release/puffinparse bench run \
    --dataset benchmark/datasets/synthetic-v1 \
    --models reducto/standard reducto/r-1 extend/parse_performance extend/parse_light \
             llamaparse/fast llamaparse/cost_effective llamaparse/agentic \
    --concurrency 4 --save-outputs benchmark/runs/outputs
./target/release/puffinparse bench report --by-dataset --intro benchmark/LEADERBOARD.intro.md \
    benchmark/results/*.json > benchmark/LEADERBOARD.md   # or: make leaderboard
```

`--save-outputs` writes each model's markdown per document so mistakes can be inspected. Committed
runs keep them under `results/outputs/<run_id>/<model>/<doc_id>.md`; the static site under
`site/` renders them next to the input and the truth with a word-level diff.
`--filter <substring>` and `--limit N` select a subset of documents. Ids of a combined dataset
carry their source (`synthetic/plain_001`), so the saved output path keeps that directory level
and `--filter synthetic` runs one source.

Before spending money, check the plan and put a ceiling on it:

```bash
./target/release/puffinparse bench run --dataset benchmark/datasets/combined-v1 \
    --models reducto/standard llamaparse/agentic --out benchmark/results/combined.json --dry-run
# | Model | Calls | Skipped (resumed) | Est. pages | $/page | Est. cost | … no provider is called
./target/release/puffinparse bench run … --max-cost 5    # aborts before the first call if the estimate is higher
```

The estimate is manifest `pages` × list price (`pricing.json`), so it is only as good as the
manifest's page counts; `--max-cost` refuses to run a model that has no list price.

Runs survive interruptions. Every finished (model, document) call is appended to
`<out>.partial.jsonl` and flushed immediately; the final JSON is assembled from it at the end and
the log is removed. After a crash, Ctrl-C or a batch of provider failures, rerun the same command
with `--resume` (and the same `--out`: the default path contains today's date). Pairs that already
succeeded — in the partial log or in an existing result JSON — are not called again; missing and
failed ones are. The resumed run keeps the original `run_id`, so `--save-outputs` files of the
first attempt stay valid, and it refuses to mix in records from another dataset revision or
normalisation. `--retries N` re-issues a document after a retryable error (rate limit, 5xx,
timeout, network) with backoff; it is off by default because a retried provider job can be
billed twice.

Each document record is auditable: `provider_job_id` (look the job up in the provider's
dashboard), `cache_hit` (`false` when caches were disabled, the default), `attempts`,
`started_at`, and `error_kind` + `error` for failures. The run ends with a summary line: calls
made, resumed, failed, total cost and wall time.

`bench report` renders the leaderboard table (**TEDS** is the mean `teds_grid` over documents whose truth has a table, a **Rules** column shows the mean rule pass rate,
`–` for datasets with no rule documents), then the per-category breakdown, and — for datasets
whose ids carry a `<source>/` prefix — a per-source breakdown of documents and `Overall`.

### Result JSON for consumers

Each model's `summary` carries `headline` (0–1; rank on this, `overall = 100 × headline`),
`char_similarity` (literal), `table_score`, `teds_grid`, `rule_pass_rate`, and each document its
own `headline`. The run records `scorer_version` (`puffinparse_core::bench::SCORER_VERSION`, currently
`3`). Files without it are scorer v1: read `headline` as `overall / 100`; their
`summary.char_similarity` held the headline, and they have no `teds_grid`. Re-score them offline:

```bash
puffinparse bench rescore benchmark/results/2026-09-11-combined-v1.json \
    --outputs benchmark/results/outputs/run-20260911T111039Z   # [--dataset <dir>] [--out <path>]
```

`rescore` reads the saved per-document outputs, scores them with the current scorer, keeps latency,
cost, pages and errors as measured, and sets `scorer_version` and `rescored_at`. It makes no
network calls and refuses to guess: a missing output for a successful document is an error, unless
`--keep-missing` is given — then that document keeps its recorded score and the count is recorded
as `rescore_kept_docs`. That is how runs over research-only sources (OmniDocBench), whose outputs
are not committed, are re-scored.

Scoring a single pair without any network access:

```bash
puffinparse bench score prediction.md truth.md
```

or from Python: `puffinparse.score(prediction, truth)`.

## Datasets

| Dataset | Docs | Categories | Source |
|---|---|---|---|
| [`synthetic-v1`](datasets/synthetic-v1/README.md) | 39 | plain, invoice, table, two_column, headings, noisy_scan, low_res, multipage, skewed, dense, faded, receipt, complex_table | generated, CC0 |
| [`parsebench`](datasets/parsebench/README.md) | 40 committed (1,009 indexed) | tables (transcript, `table-only`), text pages as rule assertions (`kind: rules`) | LlamaIndex ParseBench, Apache-2.0, pinned upstream commit |
| [`olmocr`](datasets/olmocr/README.md) | 40 committed (824 indexed) | headers_footers, long_tiny_text, multi_column, old_scans, table_tests — all `kind: rules` (205 assertions) | AI2 olmOCR-bench, ODC-BY-1.0, pinned upstream commit |
| [`omnidocbench`](datasets/omnidocbench/README.md) | 40 **indexed, fetched at run time** | 10 document types (book, newspaper, exam paper, slides, notes, …), English + Chinese, transcript | OpenDataLab OmniDocBench, research-only / non-commercial — not redistributed; `python -m benchmark.adapters omnidocbench` materialises it |
| [`dpbench`](datasets/dpbench/README.md) | 40 committed (200 convertible) | table, text, chart, figure, equation, list, index (dominant layout feature); reading-order transcript with headers/footers kept, tables as pipe tables | Upstage DP-Bench, MIT, pinned upstream commit |
| [`combined-v1`](datasets/combined-v1/README.md) | 79 | synthetic-v1 + parsebench, source-prefixed ids (frozen: has committed results) | per source |
| [`combined-v2`](datasets/combined-v2/README.md) | 159 | synthetic-v1 + parsebench + olmocr + omnidocbench (frozen once it has committed results) | per source |
| [`combined-v3`](datasets/combined-v3/README.md) | 199 | combined-v2 + dpbench | per source |

Adding a dataset: create `benchmark/datasets/<name>/manifest.json` with
`{name, version, description, license, documents:[{id, file, truth, pages, category, tags}]}`,
put inputs under `docs/` and truth markdown under `truth/`. Public benchmarks are converted by
adapters (`python -m benchmark.adapters <name>`; see [`docs/benchmarks/adapters.md`](../docs/benchmarks/adapters.md)),
which also introduce `kind: rules` documents scored by machine-checkable assertions instead of a
transcript. The [academic benchmark survey](../docs/benchmarks/academic-benchmarks.md) covers olmOCR-bench,
OmniDocBench, DP-Bench, READoc and others; READoc (a long-document track) is the next candidate.

## Caveats

- Synthetic documents are cleaner than most real-world scans. Treat `synthetic-v1` as a floor
  for basic fidelity, reading order and table structure, not as the last word on hard documents.
- Latency is measured from the client through the public API and includes upload, queueing and
  polling. Run from a different region or under load and numbers will move.
- Prices are list prices. Volume discounts, batch queues and cache hits change real cost.
- **A rule pass rate is a floor, not an accuracy.** ParseBench's assertions are generated from its
  own reference extraction, so a rule's text can carry that extraction's artifacts. Scorer v2
  neutralises the commonest one — punctuation spaced as separate tokens (`this " agreement "`) —
  by ignoring spaces next to punctuation on both sides, but others remain (two lines of the page
  fused into one "sentence", a stray footnote marker), and `present` / `order` still match exact
  substrings, so such a rule fails on correct output. The effect is the same for every model, so
  it moves the absolute number far more than the ranking. The one `bag_of_sentences` rule per
  ParseBench document matches each sentence fuzzily (≥ 0.8 similar) and passes at 80 % of the
  page's sentences: at the adapter's original 1.0 a single fused "sentence" failed it for every
  model (see [`docs/benchmarks/findings.md`](../docs/benchmarks/findings.md)).
- **An empty parse is scored, not failed.** A provider that returns HTTP 200 with no text (Reducto
  and Extend on ParseBench `text_multicolumns_2col`, whose page is one Form XObject with a
  degenerate `/BBox`) scores 0 on that document but is not counted in **Failed**; it is flagged
  `empty_output: true` and shown as `(+N empty)` next to the failure count.
- **olmOCR `max_diffs` is honoured** since scorer v2 (fuzzy `present` / `absent` / `order` /
  `table_cell`, as upstream); its skipped tests (math, positional absences, vertical table
  neighbours, baseline) are counted in `datasets/olmocr/conversion-stats.json`. Its `absent-only`
  documents pass for an empty parse.
- **OmniDocBench must be fetched** before a run (`python -m benchmark.adapters omnidocbench`);
  otherwise its documents fail as file-not-found and the dataset `sha256` does not cover them.
- **DP-Bench truth keeps page headers and footers**, because DP-Bench's own NID scores them;
  OmniDocBench truth drops them, because OmniDocBench does not. A parser that strips page
  furniture loses a little on `dpbench` and nothing on `omnidocbench`.
- **A combined score mixes datasets, licences and document kinds.** Read `combined-v1` /
  `combined-v2` / `combined-v3` next to the per-source table under the leaderboard, not instead of it.
