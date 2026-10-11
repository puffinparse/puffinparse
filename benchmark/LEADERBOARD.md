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

## combined-v3 (headline)

| Rank | Model | Overall | Char sim | CER | WER | Word F1 | Order | Table | TEDS | Rules | p50 latency | p95 latency | ms/page | $/1k pages | Failed | Dataset |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | `llamaparse/cost_effective` | **85.63** | 0.815 | 0.478 | 0.567 | 0.802 | 0.929 | 0.866 | 0.866 | 74.4% | 9452 ms | 19494 ms | 11154 | $3.75 | 0/199 | combined-v3 v3.0.0 |
| 2 | `llamaparse/agentic` | **85.28** | 0.808 | 0.504 | 0.632 | 0.808 | 0.945 | 0.885 | 0.868 | 76.3% | 14127 ms | 29269 ms | 15368 | $12.50 | 0/199 | combined-v3 v3.0.0 |
| 3 | `reducto/r-1` | **82.80** | 0.784 | 0.543 | 0.674 | 0.783 | 0.923 | 0.902 | 0.878 | 71.4% | 3459 ms | 9305 ms | 4329 | $10.00 | 0/199 | combined-v3 v3.0.0 |
| 4 | `extend/parse_performance` | **80.77** | 0.770 | 0.562 | 0.754 | 0.772 | 0.920 | 0.885 | 0.856 | 67.4% | 21965 ms | 33103 ms | 25309 | $25.00 | 0/199 | combined-v3 v3.0.0 |
| 5 | `reducto/standard` | **80.29** | 0.762 | 0.561 | 0.699 | 0.759 | 0.915 | 0.879 | 0.859 | 66.9% | 3019 ms | 8443 ms | 3797 | $15.00 | 0/199 (+1 empty) | combined-v3 v3.0.0 |
| 6 | `extend/parse_light` | **79.89** | 0.762 | 0.575 | 0.773 | 0.760 | 0.916 | 0.877 | 0.870 | 66.0% | 32038 ms | 52619 ms | 33067 | $6.25 | 0/199 | combined-v3 v3.0.0 |
| 7 | `tesseract/default` | **56.41** | 0.621 | 0.684 | 1.115 | 0.640 | 0.840 | 0.007 | 0.008 | 45.4% | 5381 ms | 26893 ms | 7749 | $0.00 | 0/199 (+4 empty) | combined-v3 v3.0.0 |

### Overall score by category

| Model | academic_literature | book | chart | colorful_textbook | complex_table | dense | equation | exam_paper | faded | figure | headers_footers | headings | historical_document | index | invoice | list | long_tiny_text | low_res | magazine | multi_column | multipage | newspaper | noisy_scan | note | old_scans | plain | ppt2pdf | receipt | research_report | skewed | table | table_tests | text | two_column |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `llamaparse/cost_effective` | 77.6 | 83.8 | 70.7 | 85.9 | 100.0 | 100.0 | 94.6 | 77.0 | 100.0 | 88.2 | 25.0 | 100.0 | 70.9 | 99.9 | 100.0 | 98.8 | 96.1 | 100.0 | 72.3 | 88.1 | 100.0 | 86.9 | 100.0 | 93.1 | 66.3 | 100.0 | 89.3 | 100.0 | 84.6 | 100.0 | 87.7 | 81.2 | 87.9 | 100.0 |
| `llamaparse/agentic` | 83.4 | 82.8 | 71.3 | 73.9 | 100.0 | 100.0 | 95.0 | 54.9 | 100.0 | 84.9 | 19.8 | 100.0 | 57.3 | 100.0 | 100.0 | 98.9 | 93.6 | 100.0 | 80.4 | 96.9 | 100.0 | 85.7 | 100.0 | 92.3 | 70.6 | 100.0 | 79.6 | 100.0 | 83.8 | 100.0 | 88.6 | 81.2 | 90.5 | 100.0 |
| `reducto/r-1` | 75.0 | 81.2 | 58.5 | 70.4 | 100.0 | 100.0 | 95.6 | 46.7 | 100.0 | 79.8 | 14.6 | 100.0 | 76.4 | 95.9 | 100.0 | 99.6 | 94.7 | 100.0 | 58.2 | 83.1 | 100.0 | 64.3 | 100.0 | 93.4 | 66.3 | 100.0 | 88.7 | 100.0 | 83.4 | 100.0 | 89.2 | 89.6 | 83.6 | 100.0 |
| `extend/parse_performance` | 78.2 | 76.9 | 93.7 | 57.4 | 99.6 | 99.9 | 97.0 | 47.4 | 100.0 | 93.8 | 5.2 | 100.0 | 41.5 | 99.9 | 100.0 | 97.4 | 90.9 | 100.0 | 53.2 | 83.1 | 100.0 | 61.5 | 99.7 | 91.8 | 58.8 | 100.0 | 77.6 | 99.9 | 73.7 | 83.9 | 86.7 | 92.7 | 79.1 | 100.0 |
| `reducto/standard` | 78.9 | 76.6 | 58.3 | 63.1 | 100.0 | 99.9 | 93.2 | 54.3 | 99.9 | 80.3 | 11.5 | 100.0 | 16.8 | 98.0 | 99.9 | 99.5 | 84.1 | 100.0 | 67.3 | 83.1 | 99.9 | 64.4 | 99.9 | 90.7 | 62.6 | 100.0 | 81.0 | 100.0 | 86.8 | 100.0 | 88.7 | 85.4 | 80.1 | 100.0 |
| `extend/parse_light` | 74.5 | 77.0 | 91.7 | 56.4 | 100.0 | 99.9 | 96.8 | 46.6 | 100.0 | 93.8 | 8.3 | 100.0 | 45.1 | 97.7 | 100.0 | 97.1 | 77.7 | 100.0 | 52.7 | 73.8 | 100.0 | 59.4 | 99.4 | 91.4 | 61.3 | 100.0 | 74.3 | 99.9 | 74.0 | 81.0 | 85.8 | 88.5 | 83.2 | 100.0 |
| `tesseract/default` | 14.9 | 50.6 | 66.3 | 44.3 | 99.9 | 99.6 | 93.9 | 32.9 | 99.2 | 85.5 | 28.1 | 100.0 | 5.8 | 71.9 | 100.0 | 95.2 | 80.2 | 99.9 | 59.9 | 61.3 | 99.5 | 50.6 | 97.9 | 2.1 | 38.8 | 100.0 | 50.6 | 97.7 | 45.2 | 80.1 | 31.4 | 0.0 | 69.3 | 100.0 |

### `combined-v3` by source

| Source | Docs | Model | Overall |
|---|---:|---|---:|
| dpbench | 40 | `extend/parse_performance` | 96.05 |
| dpbench | 40 | `extend/parse_light` | 95.45 |
| dpbench | 40 | `llamaparse/cost_effective` | 90.93 |
| dpbench | 40 | `llamaparse/agentic` | 90.31 |
| dpbench | 40 | `reducto/standard` | 88.92 |
| dpbench | 40 | `reducto/r-1` | 87.96 |
| dpbench | 40 | `tesseract/default` | 87.03 |
| olmocr | 40 | `llamaparse/agentic` | 72.42 |
| olmocr | 40 | `llamaparse/cost_effective` | 71.36 |
| olmocr | 40 | `reducto/r-1` | 69.65 |
| olmocr | 40 | `extend/parse_performance` | 66.15 |
| olmocr | 40 | `reducto/standard` | 65.34 |
| olmocr | 40 | `extend/parse_light` | 61.93 |
| olmocr | 40 | `tesseract/default` | 41.67 |
| omnidocbench | 40 | `llamaparse/cost_effective` | 82.14 |
| omnidocbench | 40 | `llamaparse/agentic` | 77.39 |
| omnidocbench | 40 | `reducto/r-1` | 73.77 |
| omnidocbench | 40 | `reducto/standard` | 68.00 |
| omnidocbench | 40 | `extend/parse_performance` | 65.93 |
| omnidocbench | 40 | `extend/parse_light` | 65.15 |
| omnidocbench | 40 | `tesseract/default` | 35.70 |
| parsebench | 40 | `llamaparse/agentic` | 86.64 |
| parsebench | 40 | `llamaparse/cost_effective` | 84.10 |
| parsebench | 40 | `reducto/r-1` | 83.06 |
| parsebench | 40 | `reducto/standard` | 79.70 |
| parsebench | 40 | `extend/parse_light` | 78.92 |
| parsebench | 40 | `extend/parse_performance` | 77.45 |
| parsebench | 40 | `tesseract/default` | 20.73 |
| synthetic | 39 | `llamaparse/cost_effective` | 100.00 |
| synthetic | 39 | `reducto/r-1` | 100.00 |
| synthetic | 39 | `llamaparse/agentic` | 100.00 |
| synthetic | 39 | `reducto/standard` | 99.96 |
| synthetic | 39 | `extend/parse_performance` | 98.70 |
| synthetic | 39 | `extend/parse_light` | 98.48 |
| synthetic | 39 | `tesseract/default` | 97.98 |

## combined-v2

| Rank | Model | Overall | Char sim | CER | WER | Word F1 | Order | Table | TEDS | Rules | p50 latency | p95 latency | ms/page | $/1k pages | Failed | Dataset |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | `llamaparse/cost_effective` | **84.97** | 0.798 | 0.564 | 0.669 | 0.769 | 0.920 | 0.886 | 0.880 | 76.4% | 9535 ms | 19846 ms | 11864 | $3.75 | 0/159 | combined-v2 v2.0.0 |
| 2 | `llamaparse/agentic` | **84.81** | 0.792 | 0.590 | 0.748 | 0.777 | 0.934 | 0.902 | 0.879 | 76.8% | 14269 ms | 26848 ms | 15701 | $12.50 | 0/159 | combined-v2 v2.0.0 |
| 3 | `reducto/r-1` | **81.55** | 0.760 | 0.630 | 0.779 | 0.750 | 0.898 | 0.904 | 0.867 | 71.2% | 3654 ms | 8437 ms | 4841 | $10.00 | 0/159 | combined-v2 v2.0.0 |
| 4 | `reducto/standard` | **78.18** | 0.730 | 0.655 | 0.814 | 0.719 | 0.889 | 0.885 | 0.860 | 66.6% | 3570 ms | 8253 ms | 4364 | $15.00 | 0/159 (+1 empty) | combined-v2 v2.0.0 |
| 5 | `extend/parse_performance` | **77.00** | 0.723 | 0.688 | 0.926 | 0.722 | 0.894 | 0.884 | 0.847 | 67.4% | 9089 ms | 22019 ms | 10592 | $25.00 | 0/159 | combined-v2 v2.0.0 |
| 6 | `extend/parse_light` | **76.23** | 0.716 | 0.699 | 0.934 | 0.712 | 0.890 | 0.873 | 0.850 | 66.0% | 5709 ms | 14298 ms | 7478 | $6.25 | 0/159 | combined-v2 v2.0.0 |
| 7 | `tesseract/default` | **48.68** | 0.557 | 0.814 | 1.344 | 0.572 | 0.786 | 0.007 | 0.007 | 45.4% | 2136 ms | 19769 ms | 3841 | $0.00 | 0/159 (+4 empty) | combined-v2 v2.0.0 |

### Overall score by category

| Model | academic_literature | book | colorful_textbook | complex_table | dense | exam_paper | faded | headers_footers | headings | historical_document | invoice | long_tiny_text | low_res | magazine | multi_column | multipage | newspaper | noisy_scan | note | old_scans | plain | ppt2pdf | receipt | research_report | skewed | table | table_tests | text | two_column |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `llamaparse/cost_effective` | 79.2 | 82.9 | 77.9 | 100.0 | 100.0 | 73.0 | 100.0 | 28.1 | 100.0 | 73.8 | 100.0 | 96.1 | 100.0 | 84.9 | 94.4 | 100.0 | 77.4 | 100.0 | 93.2 | 69.1 | 100.0 | 91.0 | 100.0 | 85.2 | 100.0 | 86.9 | 81.2 | 83.4 | 100.0 |
| `llamaparse/agentic` | 83.5 | 82.6 | 73.4 | 100.0 | 100.0 | 55.0 | 100.0 | 19.8 | 100.0 | 73.2 | 100.0 | 95.4 | 100.0 | 80.3 | 96.9 | 100.0 | 86.8 | 99.9 | 91.9 | 72.6 | 100.0 | 79.7 | 100.0 | 83.8 | 100.0 | 89.3 | 81.2 | 86.3 | 100.0 |
| `reducto/r-1` | 75.0 | 82.6 | 70.5 | 100.0 | 100.0 | 46.7 | 100.0 | 14.6 | 100.0 | 76.3 | 100.0 | 93.6 | 100.0 | 59.3 | 83.1 | 100.0 | 65.9 | 100.0 | 93.4 | 66.3 | 100.0 | 88.7 | 100.0 | 83.4 | 100.0 | 88.6 | 89.6 | 76.0 | 100.0 |
| `reducto/standard` | 78.9 | 76.4 | 69.1 | 100.0 | 99.9 | 54.4 | 99.9 | 11.5 | 100.0 | 17.3 | 99.9 | 84.1 | 100.0 | 67.2 | 83.1 | 99.9 | 64.9 | 99.9 | 90.6 | 60.1 | 100.0 | 81.0 | 100.0 | 86.8 | 100.0 | 86.5 | 85.4 | 71.3 | 100.0 |
| `extend/parse_performance` | 78.4 | 77.0 | 57.4 | 99.6 | 99.9 | 47.6 | 100.0 | 5.2 | 100.0 | 44.3 | 100.0 | 90.9 | 100.0 | 53.2 | 83.1 | 100.0 | 61.5 | 99.7 | 91.7 | 58.8 | 100.0 | 77.6 | 99.9 | 73.8 | 83.9 | 83.5 | 92.7 | 70.7 | 100.0 |
| `extend/parse_light` | 74.7 | 77.3 | 57.9 | 100.0 | 99.9 | 47.6 | 100.0 | 8.3 | 100.0 | 48.5 | 100.0 | 77.7 | 100.0 | 53.2 | 73.8 | 100.0 | 60.3 | 99.4 | 91.4 | 61.3 | 100.0 | 77.4 | 99.9 | 73.4 | 81.0 | 82.3 | 88.5 | 76.8 | 100.0 |
| `tesseract/default` | 15.0 | 50.5 | 44.1 | 99.8 | 99.6 | 33.2 | 99.2 | 26.0 | 100.0 | 6.2 | 100.0 | 75.2 | 99.9 | 59.9 | 67.5 | 99.4 | 50.5 | 97.9 | 2.5 | 39.9 | 100.0 | 50.5 | 97.7 | 43.2 | 80.0 | 10.7 | 0.0 | 55.3 | 100.0 |

### `combined-v2` by source

| Source | Docs | Model | Overall |
|---|---:|---|---:|
| olmocr | 40 | `llamaparse/cost_effective` | 73.79 |
| olmocr | 40 | `llamaparse/agentic` | 73.19 |
| olmocr | 40 | `reducto/r-1` | 69.44 |
| olmocr | 40 | `extend/parse_performance` | 66.15 |
| olmocr | 40 | `reducto/standard` | 64.84 |
| olmocr | 40 | `extend/parse_light` | 61.93 |
| olmocr | 40 | `tesseract/default` | 41.72 |
| omnidocbench | 40 | `llamaparse/cost_effective` | 81.85 |
| omnidocbench | 40 | `llamaparse/agentic` | 79.02 |
| omnidocbench | 40 | `reducto/r-1` | 74.18 |
| omnidocbench | 40 | `reducto/standard` | 68.66 |
| omnidocbench | 40 | `extend/parse_performance` | 66.25 |
| omnidocbench | 40 | `extend/parse_light` | 66.18 |
| omnidocbench | 40 | `tesseract/default` | 35.56 |
| parsebench | 40 | `llamaparse/agentic` | 87.40 |
| parsebench | 40 | `llamaparse/cost_effective` | 84.61 |
| parsebench | 40 | `reducto/r-1` | 83.05 |
| parsebench | 40 | `reducto/standard` | 79.79 |
| parsebench | 40 | `extend/parse_light` | 78.89 |
| parsebench | 40 | `extend/parse_performance` | 77.42 |
| parsebench | 40 | `tesseract/default` | 20.73 |
| synthetic | 39 | `llamaparse/cost_effective` | 100.00 |
| synthetic | 39 | `reducto/r-1` | 100.00 |
| synthetic | 39 | `llamaparse/agentic` | 99.99 |
| synthetic | 39 | `reducto/standard` | 99.96 |
| synthetic | 39 | `extend/parse_performance` | 98.70 |
| synthetic | 39 | `extend/parse_light` | 98.48 |
| synthetic | 39 | `tesseract/default` | 97.94 |

## combined-v1

| Rank | Model | Overall | Char sim | CER | WER | Word F1 | Order | Table | TEDS | Rules | p50 latency | p95 latency | ms/page | $/1k pages | Failed | Dataset |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | `llamaparse/agentic` | **93.26** | 0.824 | 0.911 | 0.866 | 0.867 | 0.942 | 0.922 | 0.859 | 86.4% | 14912 ms | 29307 ms | 16179 | $12.50 | 0/79 | combined-v1 v1.0.0 |
| 2 | `reducto/r-1` | **91.42** | 0.802 | 0.919 | 0.865 | 0.843 | 0.899 | 0.924 | 0.872 | 76.0% | 3532 ms | 6565 ms | 3767 | $10.00 | 0/79 | combined-v1 v1.0.0 |
| 3 | `llamaparse/cost_effective` | **90.56** | 0.805 | 0.899 | 0.851 | 0.855 | 0.911 | 0.886 | 0.850 | 82.2% | 14005 ms | 29394 ms | 13057 | $3.75 | 0/79 | combined-v1 v1.0.0 |
| 4 | `reducto/standard` | **89.93** | 0.795 | 0.944 | 0.899 | 0.829 | 0.888 | 0.913 | 0.868 | 71.3% | 2825 ms | 4670 ms | 2931 | $15.00 | 0/79 (+1 empty) | combined-v1 v1.0.0 |
| 5 | `extend/parse_performance` | **87.80** | 0.784 | 0.925 | 0.883 | 0.825 | 0.905 | 0.889 | 0.853 | 70.7% | 8920 ms | 21986 ms | 10012 | $25.00 | 0/79 | combined-v1 v1.0.0 |
| 6 | `extend/parse_light` | **87.50** | 0.782 | 0.933 | 0.890 | 0.826 | 0.905 | 0.882 | 0.860 | 71.2% | 5913 ms | 21784 ms | 8190 | $6.25 | 0/79 | combined-v1 v1.0.0 |

### Overall score by category

| Model | complex_table | dense | faded | headings | invoice | low_res | multipage | noisy_scan | plain | receipt | skewed | table | text | two_column |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `llamaparse/agentic` | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 99.9 | 100.0 | 100.0 | 100.0 | 88.3 | 86.4 | 100.0 |
| `reducto/r-1` | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 88.6 | 76.0 | 100.0 |
| `llamaparse/cost_effective` | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 82.9 | 82.2 | 100.0 |
| `reducto/standard` | 100.0 | 99.9 | 99.9 | 100.0 | 99.9 | 100.0 | 99.9 | 99.9 | 100.0 | 100.0 | 100.0 | 87.0 | 71.3 | 100.0 |
| `extend/parse_performance` | 99.6 | 99.9 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 99.7 | 100.0 | 99.9 | 80.6 | 83.5 | 70.7 | 100.0 |
| `extend/parse_light` | 100.0 | 99.9 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 99.4 | 100.0 | 99.9 | 81.0 | 82.3 | 71.2 | 100.0 |

### `combined-v1` by source

| Source | Docs | Model | Overall |
|---|---:|---|---:|
| parsebench | 40 | `llamaparse/agentic` | 86.70 |
| parsebench | 40 | `reducto/r-1` | 83.05 |
| parsebench | 40 | `llamaparse/cost_effective` | 81.35 |
| parsebench | 40 | `reducto/standard` | 80.15 |
| parsebench | 40 | `extend/parse_performance` | 77.42 |
| parsebench | 40 | `extend/parse_light` | 76.79 |
| synthetic | 39 | `llamaparse/cost_effective` | 100.00 |
| synthetic | 39 | `reducto/r-1` | 100.00 |
| synthetic | 39 | `llamaparse/agentic` | 99.99 |
| synthetic | 39 | `reducto/standard` | 99.96 |
| synthetic | 39 | `extend/parse_light` | 98.48 |
| synthetic | 39 | `extend/parse_performance` | 98.44 |

## synthetic-v1

| Rank | Model | Overall | Char sim | CER | WER | Word F1 | Order | Table | TEDS | Rules | p50 latency | p95 latency | ms/page | $/1k pages | Failed | Dataset |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | `llamaparse/cost_effective` | **100.00** | 1.000 | 0.000 | 0.000 | 1.000 | 1.000 | 1.000 | 1.000 | – | 9157 ms | 14872 ms | 8910 | $3.75 | 0/39 | synthetic-v1 v1.1.0 |
| 2 | `reducto/r-1` | **100.00** | 1.000 | 0.000 | 0.000 | 1.000 | 1.000 | 1.000 | 1.000 | – | 3262 ms | 8778 ms | 4231 | $10.00 | 0/39 | synthetic-v1 v1.1.0 |
| 3 | `reducto/standard` | **99.96** | 1.000 | 0.000 | 0.001 | 0.999 | 1.000 | 0.998 | 0.986 | – | 2848 ms | 4347 ms | 2669 | $15.00 | 0/39 | synthetic-v1 v1.1.0 |
| 4 | `llamaparse/agentic` | **99.75** | 0.997 | 0.003 | 0.003 | 0.998 | 1.000 | 1.000 | 1.000 | – | 9666 ms | 15199 ms | 10658 | $12.50 | 0/39 | synthetic-v1 v1.1.0 |
| 5 | `extend/parse_performance` | **98.70** | 0.987 | 0.013 | 0.018 | 0.998 | 1.000 | 0.999 | 0.949 | – | 5680 ms | 9077 ms | 6184 | $25.00 | 0/39 | synthetic-v1 v1.1.0 |
| 6 | `extend/parse_light` | **98.48** | 0.985 | 0.015 | 0.022 | 0.998 | 1.000 | 1.000 | 1.000 | – | 5517 ms | 8932 ms | 5364 | $6.25 | 0/39 | synthetic-v1 v1.1.0 |
| 7 | `llamaparse/fast` | **87.58** | 0.876 | 0.124 | 0.144 | 0.926 | 1.000 | 0.161 | 0.170 | – | 5834 ms | 9286 ms | 5995 | $1.25 | 0/39 | synthetic-v1 v1.1.0 |

### Overall score by category

| Model | complex_table | dense | faded | headings | invoice | low_res | multipage | noisy_scan | plain | receipt | skewed | table | two_column |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `llamaparse/cost_effective` | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 |
| `reducto/r-1` | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 |
| `reducto/standard` | 100.0 | 99.9 | 99.9 | 100.0 | 99.9 | 100.0 | 99.9 | 99.9 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 |
| `llamaparse/agentic` | 100.0 | 100.0 | 100.0 | 96.8 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 |
| `extend/parse_performance` | 99.6 | 99.9 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 99.7 | 100.0 | 99.9 | 83.9 | 100.0 | 100.0 |
| `extend/parse_light` | 100.0 | 99.9 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 99.4 | 100.0 | 99.9 | 81.0 | 100.0 | 100.0 |
| `llamaparse/fast` | 99.2 | 99.9 | 99.9 | 100.0 | 100.0 | 100.0 | 100.0 | 99.8 | 100.0 | 99.4 | 11.1 | 100.0 | 29.3 |
