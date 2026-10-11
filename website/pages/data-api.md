# Benchmark data API

Every number in the [leaderboard](../../benchmark/LEADERBOARD.md) and the
[results viewer](https://puffinparse.com/benchmark-results/) is published as static JSON, and that
JSON is a small read-only API: plain `GET`, no key, CORS open, cached by the CDN. It changes only
when a new benchmark run is committed to the repository.

| | |
|---|---|
| Base URL | `https://puffinparse.com/benchmark-results/data/` |
| OpenAPI 3.1 | [`https://puffinparse.com/openapi.json`](https://puffinparse.com/openapi.json) |
| API catalog (RFC 9727) | [`https://puffinparse.com/.well-known/api-catalog`](https://puffinparse.com/.well-known/api-catalog) |

Every HTML page on this site links the OpenAPI description (`rel="service-desc"`), this page
(`rel="service-doc"`) and the catalog (`rel="api-catalog"`) from its `<head>`.

## Endpoints

| Path (below the base URL) | What it is |
|---|---|
| `index.json` | The leaderboard: every run, with dataset info and one `summary` per model. |
| `runs/{run_id}.json` | One full run: every model's score, latency and cost on every document. |
| `outputs/{run_id}/{model_slug}/{doc_id}.md` | The markdown a model returned for a document (what was scored). |
| `outputs/{run_id}/{model_slug}/{doc_id}.json` | The unified `ParseResponse` (blocks with bounding boxes), when saved. |
| `datasets/{dataset}/manifest.json` | A dataset: documents, categories, licences; truth and input paths. |

- `model_slug` is the model string with `/` replaced by `_` (`reducto/r-1` -> `reducto_r-1`); it is
  also in `index.json` as `runs[].models[].slug`.
- `doc_id` of a combined dataset contains one `/` (`dpbench/01030000000016`). Send it as is: it is
  a directory on the server.
- `index.json` `runs[].outputs[<model_slug>]` lists which documents have a `.json` response
  (`json`) and which have no published output (`missing`: failed calls, and documents from
  research-only datasets that are scored but never redistributed).
- Paths inside a manifest (`file`, `truth`, `rules`, `preview`) are relative to the manifest URL.

The full schemas, with every field, are in [`/openapi.json`](https://puffinparse.com/openapi.json).

## Examples

The headline leaderboard is the newest run on the newest `combined-v*` dataset. `runs` is already
sorted newest first, and each run's `models` best first:

```bash
curl -s https://puffinparse.com/benchmark-results/data/index.json \
  | jq '[.runs[] | select(.dataset.name | startswith("combined-"))][0]
        | {run_id, dataset: .dataset.name, models: [.models[]
          | {model, overall: .summary.overall, p50_ms: .summary.latency_p50_ms,
             usd_per_1k_pages: .summary.cost_per_1k_pages_usd}]}'
```

Per-document scores for one model in a run:

```bash
RUN=run-20260925T090851Z
curl -s "https://puffinparse.com/benchmark-results/data/runs/$RUN.json" \
  | jq '.models[] | select(.model == "reducto/r-1") | .docs[]
        | {id, category, headline, latency_ms, cost_usd}'
```

What that model returned for one document, as markdown and as the unified response:

```bash
curl -s "https://puffinparse.com/benchmark-results/data/outputs/$RUN/reducto_r-1/dpbench/01030000000016.md"
curl -s "https://puffinparse.com/benchmark-results/data/outputs/$RUN/reducto_r-1/dpbench/01030000000016.json" \
  | jq '.pages[0].blocks[] | {type, bbox}'
```

The dataset behind a run, with each document's licence:

```bash
curl -s https://puffinparse.com/benchmark-results/data/datasets/combined-v3/manifest.json \
  | jq '.documents[] | {id, category, license, truth}'
```

## Field notes

- Scores are only comparable within one run (one dataset revision). `summary.headline` is 0-1 and
  `summary.overall` is the same number x 100; documents tagged `table-only` are headlined by
  `table_score`, `rules` documents by `rule_pass_rate`. The metric definitions are in the
  [benchmark methodology](../../benchmark/README.md).
- A failed call has `error` (and `error_kind`) instead of `metrics`, `headline` and `cost_usd`.
- Result files are MIT licensed. Every dataset document carries its own `license` and
  `attribution`; keep them when you redistribute.
- The format of a result file is specified in [docs/SPEC.md](../../docs/SPEC.md) (section 10.4).
