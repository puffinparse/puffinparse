# CLI

The `puffinparse` binary wraps the same Rust core as the SDK. One subcommand per mode — `parse`, `ocr`,
`extract` — plus `providers`, `bench`, `serve` (the [gateway](../server/)) and `mcp` (an MCP server for coding agents).

```bash
cargo build --release -p puffinparse-cli    # ./target/release/puffinparse
puffinparse --help
puffinparse --version
```

Logging is controlled by the `PUFFINPARSE_LOG` environment variable (`error` by default; try
`PUFFINPARSE_LOG=debug` to see every HTTP step). All diagnostics go to stderr, so stdout stays clean
for piping.

**Exit codes:** `0` success · `1` any error · `2` unsupported model or bad input.

## Shared options

`parse`, `ocr` and `extract` all take the same document options. The first argument is a local file
path or an `http(s)://` URL.

| Flag | Default | Description |
|---|---|---|
| `-m`, `--model <MODEL>` | `reducto` | `"<provider>/<model>"`. It must support the subcommand's mode; a bare provider name picks that provider's default model **for the mode**. |
| `-p`, `--pages <PAGES>` | — | 1-based page selection, e.g. `1-3,7`. |
| `-l`, `--language <LANG>` | — | Language hint (ISO 639-1), forwarded when supported. |
| `--options <JSON>` | — | Provider-specific options as a JSON object, merged verbatim into the provider request. |
| `--raw` | off | Include the provider's raw payload (JSON output only). |
| `--timeout <SECONDS>` | `300` | Whole-call deadline: upload + polling + result download. |
| `--max-retries <N>` | `2` | Retries on transient errors (429 / 5xx / network) with jittered backoff. |
| `--api-key <KEY>` | `$PUFFINPARSE_API_KEY` | Override the key; otherwise the provider's own env var is used. Never echoed. |
| `--base-url <URL>` | — | Override the provider base URL. |

After a non-JSON run, a one-line summary goes to **stderr**:

```
[reducto/standard] 3 page(s) in 2841 ms, est. $0.0450
```

## `puffinparse parse`

`parse` mode: layout-aware markdown and typed blocks.

```bash
puffinparse parse <INPUT> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-f`, `--format <FORMAT>` | `markdown` | One of `markdown`, `text`, `json`. |
| `--output-format <VENDOR>` | — | Render the JSON in a provider's **own** response shape: `reducto`, `extend`, `llamaparse`, or `puffinparse` for the unified one. |

`--format json` prints the whole `ParseResponse` as pretty JSON (and no summary line).

`--output-format` is the native-format compatibility layer: whatever provider ran the call, the
response is rendered into the named vendor's JSON, so a script that already parses Reducto's or
Extend's output keeps working after a model swap. It only applies to `--format json` — with
`markdown` or `text` output the flag is ignored and a warning goes to stderr. An unknown name
fails before any network call (exit code `2`) and the message lists the valid values.
[`docs/COMPAT.md`](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/COMPAT.md) documents
exactly what is guaranteed (key set, counts, content, block vocabulary, coordinate units, billed
pages) and what is always `null`.

```bash
puffinparse parse invoice.pdf -m extend/parse_light
puffinparse parse scan.png -m llamaparse/agentic -f json --raw > out.json
puffinparse parse doc.pdf -m reducto/r-1 --pages 1-3 -f text
puffinparse parse https://example.com/doc.pdf -m reducto/standard \
    --options '{"settings": {"return_ocr_data": true}}'
puffinparse parse big.pdf -m extend/parse_auto --timeout 900 --max-retries 4

# Extend's engine, Reducto's response shape
puffinparse parse invoice.pdf -m extend/parse_light -f json --output-format reducto \
    | jq '.result.chunks[0].blocks[0].bbox'
```

## `puffinparse ocr`

`ocr` mode: plain text with line and word boxes.

```bash
puffinparse ocr <INPUT> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-f`, `--format <FORMAT>` | `text` | `text` for the plain text, or `json` for the full `TextResponse` (text + lines + words). |

```bash
puffinparse ocr scan.png -m reducto/r-1
puffinparse ocr scan.png -m llamaparse/fast -f json | jq '.pages[0].words[:5]'
```

Providers without a native OCR endpoint serve this mode from their parse output; the JSON response
then carries `metadata.puffinparse_derived_from == "parse"`.

## `puffinparse extract`

`extract` mode: pull a JSON object out of a document with a schema. Output is always pretty JSON.

```bash
puffinparse extract <INPUT> --schema <FILE|JSON> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-s`, `--schema <FILE\|JSON>` | *required* | JSON Schema for the object to extract: a path to a `.json` file, or inline JSON (anything starting with `{`). |
| `--instructions <TEXT>` | — | Extra natural-language guidance for the extractor. |
| `--citations` | off | Ask for per-field citations (page, box, source text) where the provider supports them. |
| `--output-format <VENDOR>` | — | Render the extract envelope in `reducto`, `extend` or `llamaparse` shape instead of the unified one (best effort — see `docs/COMPAT.md` §7). `extract` always prints JSON, so it always applies. |

```bash
puffinparse extract invoice.pdf -s invoice.schema.json -m reducto/extract --citations
puffinparse extract invoice.pdf -s '{"type":"object","properties":{"total":{"type":"number"}}}' \
    --instructions 'Totals are inclusive of tax.' | jq '.data.total'
puffinparse extract invoice.pdf -s invoice.schema.json --output-format reducto | jq '.result'
```

A model that does not serve `extract` fails before any network call (exit code `2`); use
`puffinparse providers --mode extract` to see the candidates.

## `puffinparse providers`

List providers, models, modes, pricing, and whether an API key is configured.

| Flag | Default | Description |
|---|---|---|
| `--mode <MODE>` | — | Only show models that serve this mode (`parse`, `ocr`, `extract`). |
| `--json` | off | Emit JSON instead of a table. |

```bash
puffinparse providers
puffinparse providers --mode extract
```

```
┌───────────────────┬────────────┬─────────┬─────┬───────────────┬──────────────────────────┐
│ Model             │ Modes      │ Default │ Key │ $/page        │ Description              │
...
Modes: parse (markdown + blocks), ocr (plain text + boxes), extract (JSON schema).
Keys are read from: REDUCTO_API_KEY, EXTEND_API_KEY, LLAMA_API_KEY, ...
Native output formats (--output-format, json only): puffinparse | reducto | extend | llamaparse
```

`Default` marks each provider's default model, `Key` shows `✓` / `✗` for a non-empty environment
variable, and `$/page` lists the price of every mode the model serves (one number when `--mode` is
given).

`--json` emits an object with two keys:

- `providers` — one entry per provider with `name`, `display_name`, `env_var`, `key_configured`,
  `base_url`, `docs` and a `models` array of `{model, default, description, modes, per_page_usd}`,
  where `per_page_usd` is keyed by mode;
- `output_formats` — the vendor shapes this build can render (`--output-format`, and
  `output_format=` in the SDK).

Handy for scripting, or for an agent picking a model.

```bash
puffinparse providers --json | jq -r '.providers[].models[] | select(.per_page_usd.parse < 0.005) | .model'
puffinparse providers --mode ocr --json | jq -r '.providers[].models[].model'
puffinparse providers --json | jq -r '.output_formats[]'
```

## `puffinparse mcp`

Run a local [Model Context Protocol](https://modelcontextprotocol.io) server on stdin / stdout for
coding agents such as Claude Code, Cursor and Codex. The client starts the process itself; client
setup is on the [agents page](/agents/#mcp-server).

```bash
puffinparse mcp [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `--models <MODEL>` | every model | Allow-list: `provider/model`, `provider/*` or a bare provider name. Repeatable or comma-separated. An unknown name fails at startup. |
| `--root <DIR>` | off | Only read local files inside this directory. Paths and `file://` URLs are canonicalized with symlinks followed; anything outside is refused as a tool error. URLs are unaffected. |
| `--timeout <SECONDS>` | `300` | Whole-call deadline for each provider call. |
| `--max-retries <N>` | `2` | Retries on transient errors. |

Tools: `parse`, `ocr`, `extract`, `list_models` and `compare`, which runs one document through up to
eight models concurrently and reports each one's latency, cost, output length and an excerpt.
Provider keys come from the environment, exactly as for the other subcommands. No tool accepts a key
and no result contains one. Documents go only to the provider of the model being called.

The server speaks MCP revision `2026-07-28` (stateless requests, `server/discover`) and the
handshake revisions `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05` (`initialize`) that
current clients use. stdout carries only protocol messages; `PUFFINPARSE_LOG` logging goes to
stderr. The server exits when stdin closes.

```bash
puffinparse mcp --models reducto,llamaparse,tesseract --root ~/Documents/invoices
```

## `puffinparse bench`

Run and report the open OCR benchmark (it uses `parse` mode). Three subcommands: `run`, `report`,
`score`.

### `puffinparse bench run`

Run models over a dataset and write a result JSON.

| Flag | Default | Description |
|---|---|---|
| `-d`, `--dataset <DIR>` | *required* | Dataset directory containing `manifest.json`. |
| `-m`, `--models <MODEL>...` | *required* | Models to evaluate. Repeatable / space-separated. |
| `-o`, `--out <PATH>` | `benchmark/results/<date>-<dataset>.json` | Output JSON path. |
| `-c`, `--concurrency <N>` | `4` | Concurrent requests per model. |
| `--filter <SUBSTRING>` | — | Only run documents whose id contains this substring. |
| `--limit <N>` | — | Limit the number of documents. |
| `--timeout <SECONDS>` | `300` | Per-call timeout. |
| `--save-outputs <DIR>` | — | Save each model's raw markdown per document, at `<DIR>/<model>/<doc_id>.md`. |
| `--case-sensitive` | off | Score case-sensitively (normalisation lowercases by default). |
| `--allow-cache` | off | Allow provider-side result caches. Off by default so latency reflects real work. |

Every model string is validated before any network call. Progress is drawn on stderr per model,
followed by a summary line and any per-document failures; the result JSON is written to `--out` and
the leaderboard table is printed to stdout.

The result file records the `run_id`, `created_at`, the PuffinParse version, the dataset name, version,
document count and **SHA-256 of the manifest plus every input and truth file**, the normalisation
options, and for each model every document's metrics, latency, cost and error plus a summary
(accuracy, `latency_p50_ms`, `latency_p95_ms`, `latency_per_page_ms`, `total_pages`,
`total_cost_usd`, `cost_per_1k_pages_usd`, `by_category`).

```bash
puffinparse bench run \
    --dataset benchmark/datasets/synthetic-v1 \
    --models reducto/standard reducto/r-1 extend/parse_performance extend/parse_light \
             llamaparse/fast llamaparse/cost_effective llamaparse/agentic \
    --concurrency 4 --save-outputs benchmark/runs/outputs

puffinparse bench run -d benchmark/datasets/synthetic-v1 -m reducto/r-1 --filter table --limit 5
```

### `puffinparse bench report`

Render one or more result JSON files as a leaderboard.

| Argument / flag | Default | Description |
|---|---|---|
| `<RESULTS>...` | *required* | Result JSON files. Globs are expanded by your shell. |
| `-f`, `--format <FORMAT>` | `markdown` | `markdown` (alias `md`) or `json`. |

Rows from every file are pooled and sorted by **Overall** descending. A per-category breakdown is
appended when the results span more than one category. An unknown format exits with an error.

```bash
puffinparse bench report benchmark/results/*.json > benchmark/LEADERBOARD.md
puffinparse bench report benchmark/results/*.json -f json | jq '.[].summary.overall'
```

### `puffinparse bench score`

Score a single prediction file against a truth file. No network access, fully deterministic.

```bash
puffinparse bench score <PREDICTION> <TRUTH>
```

Prints the `Metrics` object as pretty JSON: `char_similarity`, `cer`, `wer`, `word_recall`,
`word_precision`, `word_f1`, `order_score`, `table_score`, `pred_chars`, `truth_chars`.

```bash
puffinparse parse doc.pdf -m reducto/r-1 > pred.md
puffinparse bench score pred.md truth.md
```

The same function is available from Python as `puffinparse.score(prediction, truth)`.

## See also

- [Getting started](/getting-started/)
- [Benchmark](/benchmark/) — methodology, metrics and caveats
- [Leaderboard](/benchmark/leaderboard/) — the current results
- [Python SDK](/python/)
