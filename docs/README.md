# PuffinParse documentation

Start here if you are joining the project. Everything a contributor or agent needs to work
without asking around is linked from this page; if something is missing, add it here.

| Document | What it is for |
|---|---|
| [`../README.md`](../README.md) | User-facing overview, install, usage, model table. |
| [`SPEC.md`](SPEC.md) | Product and architecture specification: goals, unified request/response, errors, router, provider mappings, benchmark design, quality bar. Update it when behaviour changes. |
| [`DECISIONS.md`](DECISIONS.md) | Architecture decision records. Read before proposing a change to something listed there; add an ADR when you change direction. |
| [`COMPAT.md`](COMPAT.md) | Native-format compatibility (`output_format="reducto"\|"extend"\|"llamaparse"`): what the vendor-shaped renders guarantee, which fields are always null, the coordinate-units rule, and migration examples. |
| [`SERVER.md`](SERVER.md) | The HTTP gateway (`puffinparse serve`): config file, virtual keys, budgets, rate limits, API, errors, metrics, Docker. |
| [`providers/`](providers/README.md) | Per-provider reference: endpoints, request flow, response mapping, errors, gotchas, passthrough options. |
| [`DESIGN.md`](DESIGN.md) | Design language: tokens, components, writing rules for every surface. |
| [`benchmarks/`](benchmarks/) | Survey of public OCR benchmarks, adapter notes for the combined dataset, and [`findings.md`](benchmarks/findings.md) (what committed runs taught us: scorer v2, provider quirks). |
| [`../benchmark/README.md`](../benchmark/README.md) | Benchmark methodology, metrics, how to run, dataset format. |
| [`../benchmark/LEADERBOARD.md`](../benchmark/LEADERBOARD.md) | Leaderboard generated from committed results with `puffinparse bench report` (one section per dataset). Do not edit the numbers by hand. |
| [`../benchmark/site/README.md`](../benchmark/site/README.md) | The static results viewer (GitHub Pages) and how to build it. |
| [`RELEASING.md`](RELEASING.md) | Tag-driven release to PyPI, npm, crates.io, GHCR and GitHub Releases; one-time trusted-publishing setup. |
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | Setup, checks, how to add or verify a provider or a dataset, PR checklist. [`../AGENTS.md`](../AGENTS.md) is the short version for coding agents. |
| [`../CHANGELOG.md`](../CHANGELOG.md) | Keep-a-Changelog; add a line under Unreleased with every user-visible change. |

## Working in parallel

1. Pick or open an issue on GitHub and comment that you are working on it.
2. Branch from `main` (the only long-lived branch, see ADR-9) and open a pull request. Small,
   reviewable commits; run `make lint test` before pushing.
3. Anything that changes an interface (unified types, model names, manifest format, result JSON)
   is a spec change: update `SPEC.md` in the same commit and add an ADR if it reverses a decision.
4. Provider facts go in `providers/<name>.md`, never only in code comments or chat.
5. Add a CHANGELOG line, and reference the issue in the pull request so it closes on merge.

## Layout cheat sheet

```
crates/puffinparse-core/src/
  types.rs        unified request/response, markdown→text, page grouping
  error.rs        Error + ErrorKind (mirrored by python/puffinparse/exceptions.py)
  model.rs        provider + model registry (the only place models are declared)
  pricing.rs/.json list prices, overridable
  http.rs         shared client, retry/backoff, deadline, polling helper
  jobs.rs         async jobs API types: JobHandle, JobStatus, webhook events (SPEC §15)
  provider.rs     Provider trait + helpers (keys, base URLs, multipart)
  providers/      one file per provider (19) + mod.rs build(); vlm.rs = shared helpers of the
                  vision-LLM providers (gemini, openai, anthropic); local.rs = shared helpers of
                  the self-hosted engines (tesseract, docling, paddleocr, vllm)
  testutil.rs     test-only loopback HTTP server for provider wire tests
  compat/         render a unified response in a vendor's native JSON shape (docs/COMPAT.md)
  router.rs       ordered / round-robin fallbacks, stats
  bench.rs        normalisation + metrics + summaries
  util.rs         deep_merge, page-range parsing
crates/puffinparse-cli/src/   main.rs (parse, ocr, extract, providers, serve), bench.rs (run, report, score, rescore, evidence),
                  mcp/ (stdio MCP server; eval_tools.rs = dataset tools + find_best_model prompt),
                  eval/ (scan, recommend + evidence.json + data_handling.json, run, analyze, view/ local viewer, anonymize)
crates/puffinparse-server/src/ api.rs (routes, auth, fallback loop), config.rs, usage.rs, metrics.rs, log.rs
crates/puffinparse-python/    PyO3 module `puffinparse._core`
python/puffinparse/           public API, types, exceptions; python/tests/
crates/puffinparse-node/      napi-rs addon behind the npm package
js/                       npm package `puffinparse`: index.js + hand-written index.d.ts; js/test/
benchmark/                generate_synthetic.py, datasets/, results/, site/, adapters/ (parsebench, olmocr, omnidocbench, dpbench, combined)
```
