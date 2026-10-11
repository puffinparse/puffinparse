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
stdio, built into the CLI. It gives an agent five tools:

| Tool | What it does |
|---|---|
| `parse` | `file`, `model`, optional `pages`, `output` (`markdown` or `text`), `include_blocks`, `language`, `max_chars`. Returns the document as markdown or text; the structured result carries the model, pages, latency, estimated cost, `total_chars`, `truncated` and an optional block summary. |
| `ocr` | `file`, `model`, optional `pages`, `language`, `max_chars`. Plain text. |
| `extract` | `file`, `model`, `schema` (JSON Schema), optional `instructions`, `citations`, `pages`. Returns the extracted object and per-field confidence / citations. |
| `list_models` | Optional `mode`, `provider`, `include_descriptions`. Every model string with its modes, price per page and whether its provider is ready (key set, or a local engine). No network call. |
| `compare` | `file`, `models` (up to 8), `mode` (`parse` or `ocr`). Runs the models concurrently and reports per model: success or the typed error, pages, latency, cost, output length and an excerpt. |

`file` is a local path (absolute is safest) or a public http(s) URL. Long output is cut at
`max_chars` (default 40,000) with a note saying how to fetch the rest. Provider failures come back
as tool errors that keep the error kind, the HTTP status and the provider's own message.

What the server does with your data and keys:

- Keys are read from the server's environment (the variables under [API keys](#api-keys)). No tool
  takes a key or a base URL as an argument, and no result contains a key; `list_models` reports
  only whether each one is set.
- Read-only: it reads documents and returns results, nothing else. A document goes only to the
  provider of the model the agent calls (local engines keep it on your machine or server), and that
  provider bills you at its price per page.
- `--models reducto,llamaparse/agentic` limits the models it may call (`provider/model`,
  `provider/*` or a bare provider name). `--root <DIR>` limits local files to that directory, with
  symlinks followed. `--timeout` and `--max-retries` set the per-call limits.

### Claude Code

```bash
claude mcp add puffinparse -- puffinparse mcp
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
