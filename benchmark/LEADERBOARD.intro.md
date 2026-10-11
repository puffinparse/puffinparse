# PuffinParse Leaderboard

Generated from the result files in `benchmark/results/` by `make leaderboard`
(`puffinparse bench report --by-dataset --intro benchmark/LEADERBOARD.intro.md`): this introduction
is hand-written in `LEADERBOARD.intro.md`; every section below is generated. Higher **Overall** is
better (100 = character-exact after normalisation, or every rule passing). Latency is measured
from the client through the public API, including upload and polling, with provider result caches
disabled. Prices are public pay-as-you-go list prices. Methodology and caveats:
[`benchmark/README.md`](README.md). Every document, output, diff and rule check is browsable at
[puffinparse.com/benchmark-results](https://puffinparse.com/benchmark-results/).

**Headline: `combined-v3`** (run 2026-09-25, re-scored with scorer v3 on 2026-10-08) — 199 documents from five sources, each
scored by its own ground truth: `synthetic-v1` (exact transcripts), a ParseBench subset (rules and
table truth), an olmOCR-bench subset (its unit-test style rules, `max_diffs` honoured), an
OmniDocBench subset (reading-order transcripts; English and Chinese) and a DP-Bench subset (Upstage's
document-parsing benchmark: reading-order transcripts and table truth, MIT). Compare models within a
source column rather than across sources. 1,194 API calls, 0 failures, $14.65 at list price.
The free `tesseract/default` baseline (Tesseract 5.5.1, `eng`, one OpenMP thread per process,
10 documents at a time on a 10-core Mac) was added to the same run on 2026-10-08 with
`bench run --resume`: 199 documents, 0 failures, 2 min 42 s wall.
Its latency is local CPU time on a shared machine, not comparable to the API rows.
OmniDocBench is research-only, so its per-page outputs are not committed (scores are), and its 40
documents cannot be re-scored offline: in `combined-v2` and `combined-v3` they keep their scorer v2
scores (`rescore_kept_docs: 280` in the result files, 40 documents × 7 models). On the other 159
documents, all scored by v3, the order is `llamaparse/agentic` 87.26, `llamaparse/cost_effective`
86.51, `reducto/r-1` 85.07, `extend/parse_performance` 84.50, `extend/parse_light` 83.60,
`reducto/standard` 83.38.

**Read close scores as ties.** A paired bootstrap over the 199 documents (95% intervals of the
difference in Overall): `llamaparse/cost_effective` − `llamaparse/agentic` +0.36 [−1.35, +2.14];
`llamaparse/agentic` − `reducto/r-1` +2.48 [+0.32, +4.68]; `reducto/r-1` −
`extend/parse_performance` +2.04 [−0.14, +4.27]; `extend/parse_performance` − `reducto/standard`
+0.48 [−1.33, +2.37]; `reducto/standard` − `extend/parse_light` +0.39 [−1.87, +2.55]. So: the two
LlamaParse models are tied with each other and ahead of the rest; ranks 3–6 are close.

Older runs are kept for comparison: `combined-v2` (159 documents, 2026-09-24), `combined-v1`
(79 documents, 2026-09-11) and `synthetic-v1` (39 documents, 2026-09-11). Every run was re-scored
offline with scorer v3 on 2026-10-08 (`puffinparse bench rescore`; latency and cost as originally
measured). Scorer v3 fixed formatting artefacts that cost points without being reading errors:
single `*italic*` / `_italic_` markers, inline tags that split words (`9<sup>th</sup>` read as
`9 th`), dot leaders in tables of contents, figure descriptions a parser adds inside
`<figure>` or `![…](…)` (transcript truths carry no figure content, as DP-Bench's own scorer
ignores figure regions), and `table_cell` rules that could not match a header row. See
[`docs/benchmarks/findings.md`](../docs/benchmarks/findings.md) for what scorer v2 changed.
