# PuffinParse for agents

PuffinParse is one API for hosted and local OCR / document-parsing providers (Reducto, Extend,
LlamaParse, Mistral, Azure, Textract, Gemini, OpenAI, Anthropic, Tesseract, Docling and more). A
Rust core with Python and TypeScript SDKs, a CLI and a self-hosted gateway. Every provider returns
the same response shape and the same typed errors, so switching provider is a one-string change.
MIT licensed. Source: https://github.com/ajinkyashejul/puffinparse

This page is also served as plain Markdown at `https://puffinparse.com/agents.md`. The full site
map for agents is `https://puffinparse.com/llms.txt`.

## Install

```bash
pip install puffinparse                       # Python 3.9+, wheel bundles the Rust core
cargo install --git https://github.com/ajinkyashejul/puffinparse puffinparse-cli   # the `puffinparse` CLI
```

Prebuilt CLI archives are attached to each GitHub release. The Node.js package is not on npm yet
and the crates are not on crates.io yet; [Getting started](/getting-started/) shows how to build
them from source.

## Model strings

Every call takes a model string `<provider>/<model>`, for example `reducto/standard`,
`llamaparse/cost_effective` or `mistral/ocr-latest`. A bare provider name (`reducto`) means that
provider's default model for the requested mode. Three modes, each with its own response type:
`parse` (markdown + typed blocks with boxes), `ocr` (text + line/word boxes) and `extract` (JSON
shaped by your schema). List what exists instead of guessing:

```bash
puffinparse providers                         # providers, env vars, models, which keys are set
python -c "import puffinparse; print(puffinparse.list_models('parse'))"
```

## Choosing a model

Use the open benchmark, not vendor claims. The [leaderboard](/benchmark/leaderboard/) ranks every
benchmarked model on accuracy, p50 latency and cost per 1,000 pages; read the
[methodology](/benchmark/) before comparing scores across datasets. Machine-readable data:

- `https://puffinparse.com/benchmark-results/data/index.json`: every run, with dataset info and a
  per-model summary (`overall`, text and table metrics, `latency_p50_ms`, `cost_per_1k_pages_usd`,
  `by_category`).
- `https://puffinparse.com/benchmark-results/data/runs/<run_id>.json`: one full run, with
  per-document scores for every model.

Prefer the newest `combined-v*` run. If a model is not in the benchmark, say so rather than
inferring its quality.

## Python

```python
import puffinparse

doc = puffinparse.parse("invoice.pdf", model="reducto/standard")
print(doc.markdown, doc.usage.pages, doc.cost_usd)

puffinparse.estimate_cost("reducto/standard", 1000)   # USD for 1,000 pages, None if unpriced
```

## TypeScript

```ts
import { parse, estimateCost } from 'puffinparse'

const doc = await parse('invoice.pdf', { model: 'llamaparse/cost_effective' })
console.log(doc.markdown, doc.costUsd)
estimateCost('llamaparse/cost_effective', 1000)       // USD, or null when unpriced
```

## CLI

```bash
puffinparse parse invoice.pdf -m extend/parse_light            # markdown on stdout
puffinparse parse scan.png -m mistral/ocr-latest -f json       # unified JSON
```

## MCP server

`puffinparse mcp` is a local [Model Context Protocol](https://modelcontextprotocol.io) server on
stdio, built into the CLI. It gives an agent five tools for single documents and ten more for
[finding the best model for a folder of your own documents](#find-the-best-model-for-your-documents):

| Tool | What it does |
|---|---|
| `parse` | `file`, `model`, optional `pages`, `output` (`markdown` or `text`), `include_blocks`, `language`, `max_chars`. Returns the document as markdown or text; the structured result carries the model, pages, latency, estimated cost, `total_chars`, `truncated` and an optional block summary. |
| `ocr` | `file`, `model`, optional `pages`, `language`, `max_chars`. Plain text. |
| `extract` | `file`, `model`, `schema` (JSON Schema), optional `instructions`, `citations`, `pages`. Returns the extracted object and per-field confidence / citations. |
| `list_models` | Optional `mode`, `provider`, `include_descriptions`. Every model string with its modes, price per page and whether its provider is ready (key set, or a local engine). No network call. |
| `compare` | `file`, `models` (up to 8), `mode` (`parse` or `ocr`). Runs the models concurrently and reports per model: success or the typed error, pages, latency, cost, output length and an excerpt. |

| Tool | What it does |
|---|---|
| `scan_dataset` | `path`. Counts the documents in a local folder: pages, file types, the largest files, reference transcripts, earlier runs. No network. |
| `recommend_models` | `monthly_pages`, `max_usd_per_1k_pages`, `error_tolerance` (`low`, `medium`, `high`), `priority` (`accuracy`, `balanced`, `cost`, `speed`), `zdr_required`, `local_only`, `needs` (`tables`, `scans`, `forms`, ...), `top_n`. A shortlist from the public benchmark, list prices and each provider's data handling, with the monthly cost and whether the key is set. No network. |
| `run_dataset` | `dataset`, `models`, `max_cost_usd` (required for priced models), optional `pages`, `limit`, `filter`, `via` (`keys` or `gateway`), `dry_run`, `resume`. Runs every model over every document in the background and saves each output; returns the run folder. |
| `run_status`, `cancel_run` | Progress, spend and failures of a run; stop it. `run_status` waits for a running run (up to `wait_seconds`, default 30), so the agent never has to sleep between checks. |
| `analyze_run` | Per model: failures, agreement with the other models, latency, cost, monthly cost at your volume, your reviews, scores against reference transcripts, and a queue of documents worth checking by eye. `write_report` writes `report.md`. |
| `get_output` | One document's output from every model, next to the path of the original. |
| `record_review` | `doc`, `model`, `verdict` (`correct`, `minor_errors`, `major_errors`, `unusable`), `notes`. |
| `open_viewer` | Starts the local viewer for a run and returns its link. |
| `anonymize_dataset` | `dataset`, `out`, optional `terms`, `kinds`. Redacted copies made on your machine (see below). |

`file` is a local path (absolute is safest) or a public http(s) URL. Long output is cut at
`max_chars` (default 40,000) with a note saying how to fetch the rest. Provider failures come back
as tool errors that keep the error kind, the HTTP status and the provider's own message.

What the server does with your data and keys:

- Keys are read from the server's environment (the variables under [API keys](#api-keys)). No tool
  takes a key or a base URL as an argument, and no result contains a key; `list_models` reports
  only whether each one is set.
- A document goes only to the provider of the model the agent calls (local engines keep it on your
  machine or server), and that provider bills you at its price per page. The single-document tools
  write nothing. The dataset tools write only run folders (`<dataset>/.puffinparse/runs/`) and the
  anonymized copies you ask for, and a priced run needs a spending cap (`max_cost_usd`).
- `--models reducto,llamaparse/agentic` limits the models it may call (`provider/model`,
  `provider/*` or a bare provider name). `--root <DIR>` limits local files to that directory, with
  symlinks followed. `--timeout` and `--max-retries` set the per-call limits.

From the next release (0.1.7) the Python package carries the `puffinparse` command, so an agent
can start the server with nothing installed beforehand: `uvx puffinparse mcp` downloads and caches
it on first use, with no global install, PATH change or admin rights. Use `uvx puffinparse mcp`
wherever the examples below say `puffinparse mcp` (with `"command": "uvx", "args": ["puffinparse",
"mcp"]` in a JSON config). `pip install puffinparse` puts the same command on PATH.

### Claude Code

```bash
claude mcp add puffinparse -- puffinparse mcp
claude mcp add puffinparse -- uvx puffinparse mcp      # 0.1.7 and later, nothing to install
claude mcp add --scope user puffinparse -- puffinparse mcp --models reducto,llamaparse
```

If the keys are not exported where Claude Code starts, use a project `.mcp.json` with environment
expansion, so the file names the variables and never holds a key:

```json
{
  "mcpServers": {
    "puffinparse": {
      "type": "stdio",
      "command": "puffinparse",
      "args": ["mcp"],
      "env": { "REDUCTO_API_KEY": "${REDUCTO_API_KEY}", "LLAMA_API_KEY": "${LLAMA_API_KEY}" }
    }
  }
}
```

### Cursor

`.cursor/mcp.json` in the project, or `~/.cursor/mcp.json` for every project:

```json
{
  "mcpServers": {
    "puffinparse": {
      "type": "stdio",
      "command": "puffinparse",
      "args": ["mcp"],
      "env": { "REDUCTO_API_KEY": "${env:REDUCTO_API_KEY}" }
    }
  }
}
```

`"envFile": ".env"` loads the keys from a file instead.

### Codex

```bash
codex mcp add puffinparse -- puffinparse mcp
```

or in `~/.codex/config.toml`:

```toml
[mcp_servers.puffinparse]
command = "puffinparse"
args = ["mcp"]
env_vars = ["REDUCTO_API_KEY", "EXTEND_API_KEY", "LLAMA_API_KEY"]   # forwarded from your shell
tool_timeout_sec = 600
```

Codex stops a tool call after 60 seconds by default. Parsing a long document often takes longer, so
raise `tool_timeout_sec`.

## Find the best model for your documents

Benchmarks rank models on someone else's documents. To pick one for yours, point your agent at a
folder of documents that covers your use case (around 100 is plenty) and ask it to use the
`find_best_model` prompt (optional arguments: `dataset`, and `requirements` such as "20,000 pages a
month, under $10 per 1,000 pages, no ZDR needed" to skip those questions), or just say "find the best parsing model for the documents in ~/docs".
In Claude Code the prompt is `/mcp__puffinparse__find_best_model`. The agent then:

1. Scans the folder (`scan_dataset`) and tells you what is in it.
2. Asks only what it still needs: pages per month, budget per 1,000 pages, how many errors you can
   live with, whether documents may leave your machine or need zero data retention, and what
   matters in them (tables, scans, forms, ...).
3. Offers to anonymize the documents first if they may hold personal data.
4. Shortlists about five models (`recommend_models`) and asks which ones to run.
5. Asks how to pay: your own provider keys, set in the MCP server's environment (never pasted into
   the chat), or a PuffinParse gateway key.
6. Shows the estimate (`run_dataset` with `dry_run`), then runs the models with a spending cap once
   you agree. Everything runs from your machine, straight to each provider.
7. Analyzes the run, reads the originals of the documents where models disagree or failed,
   compares each model's output with the page and records a verdict for each.
8. Recommends a model and a runner-up with the reasons, monthly cost, latency and data handling,
   and gives you a link to the viewer so you can check the results yourself.

**The viewer** (`open_viewer`, or `puffinparse eval view <run>`) is a local page that shows each
document next to every model's output, rendered, as Markdown, or as a diff against another model,
and lets you record your own verdicts. It listens on `127.0.0.1` only, behind a random link, and
serves only the documents in that run.

**Anonymizing** (`anonymize_dataset`, or `puffinparse eval anonymize`) runs Tesseract on your
machine, finds email addresses, phone numbers, card numbers, IBANs and US social security numbers
(dates and other long numbers on request), plus any names or words you list, and writes copies
with those words blacked out. The copies are image-only PDFs, so no hidden text layer survives. It
needs Tesseract and `pdftoppm` (poppler), which also burns the boxes into the page pixels. Detection is pattern-based, so check a few
pages before you rely on it: names are only removed if you list them.

**What the analysis is.** With no reference transcripts, a model is judged by how much it agrees
with the other models on the same document (word and number overlap), by its failures, and by
what the agent and you see when checking documents by hand. Agreement points at problems but is
not accuracy: if most models make the same mistake, the one that got it right looks like the odd
one out, which is why the agent reads the disagreeing documents itself. If you add a reference
transcript next to a document (`invoice.pdf.truth.md` or `invoice.truth.md`), each model is also
scored against it.

**Paying through a gateway.** Instead of a key per provider, `via: "gateway"` (or `--gateway` on
the CLI) sends each document to a PuffinParse gateway (`puffinparse serve`, see
[Server](https://github.com/ajinkyashejul/puffinparse/blob/main/docs/SERVER.md)) with one gateway
key whose budget caps the spend. Set `PUFFINPARSE_GATEWAY_URL` (https, or a localhost address) and
`PUFFINPARSE_GATEWAY_KEY` in the MCP server's environment. When the key's budget runs out, the run
stops and can be resumed later. There is no hosted gateway to buy credits from yet; you can run your
own.

The same workflow without an agent:

```bash
puffinparse eval scan ~/docs
puffinparse eval recommend --monthly-pages 20000 --max-usd-per-1k 10 --needs tables,scans
puffinparse eval run ~/docs -m reducto,llamaparse/cost_effective,tesseract --max-cost 5
puffinparse eval analyze ~/docs/.puffinparse/runs/<run> --write-report
puffinparse eval view ~/docs/.puffinparse/runs/<run>
```

## API keys

Each provider reads its own environment variable; set only the ones you use: `REDUCTO_API_KEY`,
`EXTEND_API_KEY`, `LLAMA_API_KEY`, `MISTRAL_API_KEY`, `AZURE_DOCUMENT_INTELLIGENCE_KEY` +
`AZURE_DOCUMENT_INTELLIGENCE_ENDPOINT`, `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (Textract),
`GEMINI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `MATHPIX_APP_ID` + `MATHPIX_APP_KEY`,
`DATALAB_API_KEY`, `UNSTRUCTURED_API_KEY`, `UPSTAGE_API_KEY`, `LANDINGAI_API_KEY`,
`GOOGLE_DOCUMENTAI_ACCESS_TOKEN` (+ `_PROJECT`, `_LOCATION`, `_PROCESSOR_ID`). Tesseract, Docling
and PaddleOCR run locally or self-hosted and need no key. Full list: [Providers](/providers/).

## Rules for agents

1. Never print, log, echo or commit an API key. Read keys from the environment; do not paste them
   into code, notebooks, shell history or issue text.
2. Check verification status before relying on a provider. **live-verified**: Reducto, Extend,
   LlamaParse. **verified locally**: Tesseract, Docling. Everything else is **docs-only**:
   implemented from the provider's documentation and tested against fixtures, not yet run live.
   Tell the user when you pick a docs-only provider.
3. Every call costs the user money at the provider's price. Call `estimate_cost` (Python) or
   `estimateCost` (TypeScript) before parsing many pages, and ask before large batches.
4. Catch the typed errors (`AuthenticationError`, `RateLimitError`, `UnsupportedModelError`, ...)
   instead of retrying blindly; the provider's message is preserved on the exception.
5. Keep modes apart: providers are interchangeable only within one mode.

## Onboarding prompt

Paste this into your coding agent to set PuffinParse up in a project:

<!-- copy-button: Copy onboarding prompt -->
```text
Add document parsing to this project with PuffinParse (https://puffinparse.com).
1. Read https://puffinparse.com/agents.md and https://puffinparse.com/llms.txt first.
2. Install it: `pip install puffinparse` (Python). For Node.js, build `js/` from a clone of the
   repository (it is not on npm yet).
3. Pick a model string `<provider>/<model>` from the benchmark leaderboard
   (https://puffinparse.com/docs/benchmark/leaderboard/) for my documents and budget, and tell
   me why. Prefer live-verified providers (Reducto, Extend, LlamaParse) or local ones
   (Tesseract, Docling); say so if you choose a docs-only provider.
4. Read the API key from the provider's environment variable (for example REDUCTO_API_KEY).
   Never print, log or commit a key; add the variable name to .env.example only.
5. Before parsing many pages, call estimate_cost / estimateCost and show me the estimate.
6. Handle PuffinParse's typed errors and keep the model string in configuration so the
   provider can be switched without code changes.
```

More: [Getting started](/getting-started/), [Python SDK](/python/), [TypeScript](/typescript/),
[CLI](/cli/), [Gateway](/gateway/).
