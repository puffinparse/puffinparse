# PuffinParse website

The whole product site: a **landing page at `/`** and the **documentation under `/docs/`**. One
Python script, two stylesheets, one small JavaScript file — no toolchain, no framework, no CDN, no
web fonts.

```
website/
  build.py            the entire build (landing section + docs section + link check + redirects)
  data_api.py         OpenAPI 3.1 + RFC 9727 API catalog for the benchmark data (used by build + qa)
  qa.py               offline QA of a finished build (agent-facing contract), run in CI
  nav.json            the docs site map: sections, pages, sources, descriptions
  landing/index.html  the landing page template; {{PLACEHOLDER}}s filled from repo data
  pages/*.md          the pages written for the site (everything else is repo markdown)
  assets/style.css    the shared design system (docs chrome, prose, tokens)
  assets/landing.css  the landing layer: same tokens, bigger type, one vermilion accent
  assets/app.js       progressive enhancement for both halves
  playground/         the /playground/ page template
  assets/playground.* the playground's script and stylesheet (no dependencies)
  tests/              offline tests for the playground script, and a fake playground API
  dist/               generated, git-ignored
```

The docs do not duplicate documentation: most pages render markdown that already lives in the repo
(`README.md`, `docs/`, `benchmark/`). Only four pages are written for the site, under
`website/pages/` (getting started, the SDK and CLI references, and `agents.md`).

The landing page duplicates nothing either — every number, provider, model and benchmark row on it
is read out of the repository at build time (see *The landing page* below).

## Build

```bash
pip install markdown             # the only dependency; everything else is stdlib
python website/build.py          # -> website/dist/
```

| Flag | Default | What it does |
|---|---|---|
| `--base-url <PATH>` | `/` | URL prefix, for hosting under a subpath (`--base-url /docs`). |
| `--out <DIR>` | `website/dist` | Output directory. Wiped and rebuilt on every run. |
| `--site-url <URL>` | `https://ajinkyashejul.github.io/puffinparse` | Public base URL of the deployed site, used for `<link rel="canonical">` and `sitemap.xml`. Independent of `--base-url`. |
| `--docs-prefix <PATH>` | `docs` | Where the documentation is mounted below `--base-url`. The landing page always owns `--base-url` itself. |
| `--with-benchmark` / `--no-benchmark` | Also build the results viewer into `<out>/benchmark-results/` (default on) |
| `--check` | off | After building, verify that every internal link and `#fragment` resolves — on the landing page as well as the docs — that every page carries one valid JSON-LD block, and that the generated `redirects` and `routes` in `vercel.json` match `nav.json`. |
| `--write-redirects` | off | Rewrite the generated `redirects` and `routes` arrays in `vercel.json` from `nav.json`. Run it after adding, renaming or removing a page. |

```bash
python website/build.py --check                        # build + link check (what CI should run)
python website/build.py --write-redirects              # refresh the old-URL redirect map
python website/build.py --base-url /preview --out /tmp/s  # preview a subpath deployment
```

`website/dist/` is generated and git-ignored.

## Preview

```bash
python -m http.server -d website/dist 8001
# http://localhost:8001
```

`http.server` serves `index.html` for a directory URL, so pretty URLs work locally exactly as they
do in production: `/` is the landing page, `/docs/` the documentation home, `/docs/python/` a page.

## How `nav.json` works

`website/nav.json` is the whole **docs** site map (the landing page is not in it). `sections` is an
ordered list; each has a `title` (used as the sidebar group heading — empty means no heading) and an
ordered list of `pages`:

```json
{
  "slug": "project/spec",
  "title": "Specification",
  "nav_title": "Spec",
  "source": "docs/SPEC.md",
  "description": "One sentence, used for <meta name=description> and llms.txt.",
  "optional": true
}
```

| Key | Required | Meaning |
|---|---|---|
| `slug` | yes | Path below the docs prefix. `""` is the docs index at `/docs/`; `project/spec` becomes `/docs/project/spec/`. |
| `title` | yes | `<h1>`-level name, used in `<title>`, `llms.txt` and search. |
| `nav_title` | no | Shorter label for the sidebar. Defaults to `title`. |
| `source` | yes | Path of the markdown file, relative to the repository root. |
| `description` | yes | One line. Becomes the meta description and the `llms.txt` entry. |
| `optional` | no | Puts the page under `## Optional` in `llms.txt` instead of `## Docs`. |
| `home` | no | Home page: badges are stripped and the hero, leaderboard and agent cards are prepended. |
| `provider_index` | no | Prepends the generated provider cards. |

To add a page: write or pick the markdown, add an entry, rebuild. Nothing else references the list.

**Links are rewritten automatically.** A relative link in any source file is resolved against that
file's directory and then:

- if the target is a page on the site, it becomes the site URL (`docs/SPEC.md` → `/docs/project/spec/`,
  `../benchmark/README.md` → `/docs/benchmark/`);
- otherwise it becomes a GitHub URL on the branch in `nav.json`
  (`LICENSE` → `.../blob/main/LICENSE`, a directory → `.../tree/main/...`);
- a shorthand anchor such as `#adr-9` is expanded to the heading id the site actually generated.

Links inside fenced code blocks are never touched. Pages authored under `website/pages/` may use
absolute docs paths (`/python/`) directly — they are resolved through the same slug table, so they
follow the docs prefix and need no editing if it ever changes.

## What is generated

| Path | What |
|---|---|
| `index.html` | The landing page, from `website/landing/index.html`. |
| `docs/<slug>/index.html` | A docs page. Pretty URL, no extension needed; `docs/index.html` is the docs home. |
| `docs/<slug>/index.md` | The same page as raw markdown, links rewritten, for agents and `curl`. |
| `llms.txt` | [llms.txt](https://llmstxt.org) site map, at the **site root**; entries point at `/docs/<slug>/index.md`. |
| `llms-full.txt` | Every docs page concatenated in nav order, with `# Title` and a source-path comment. |
| `search.json` | Page titles + headings, fetched lazily by the search box. |
| `agents.md` | The `/docs/agents/` page as Markdown at the site root, for a short URL to hand an agent. |
| `sitemap.xml`, `robots.txt` | Standard crawler files, built from `--site-url`. The sitemap lists the landing page and every docs page; `robots.txt` allows everyone and names the major AI crawlers and agents explicitly. |
| `openapi.json` | OpenAPI 3.1 description of the benchmark data under `/benchmark-results/data/` (see *Benchmark data API* below). |
| `.well-known/api-catalog` | RFC 9727 API catalog: a Linkset pointing at the data API, its OpenAPI description and its docs page. |
| `404.html`, `404.md` | Not-found page, styled like the rest, pointing at both halves; `404.md` is what Markdown clients get. |
| `style.css`, `landing.css`, `app.js` | Copied verbatim from `website/assets/`. |
| `playground/index.html`, `playground/samples/` | The playground (below) and its sample documents with their saved model outputs. |

Every HTML page carries `<meta name="description">`, `<link rel="canonical">` and
`<link rel="alternate" type="text/markdown">` pointing at its `index.md`, the API discovery links
`<link rel="api-catalog">`, `rel="service-desc"` (`/openapi.json`) and `rel="service-doc"` (RFC
8631; also on the landing page, the 404 page and the results viewer), plus a "View as Markdown"
link in the footer, and one schema.org JSON-LD block: `SoftwareApplication` + `WebSite` (with a
`SearchAction` on `/docs/?q=`, which opens the docs search) on the landing page, `TechArticle` on
docs pages, and `Dataset` entries (one per redistributed benchmark dataset, with its licence) on the
results viewer.

A fenced block preceded by `<!-- copy-button: Label -->` gets a labelled copy button (the
onboarding prompt on `/docs/agents/`); the marker is dropped from the Markdown output.

Two small blocks in the docs are generated from live repository data rather than prose, so they
cannot drift:

- the docs home **leaderboard strip** reads `benchmark/results/*.json` (best score per model, top 5);
- the **provider cards** on `/docs/providers/` read `crates/puffinparse-core/src/model.rs` and
  `pricing.json` for display names, env vars, model lists and vendor doc links.

Both degrade to nothing if those files are missing.

## The landing page

`website/landing/index.html` is a plain HTML template. `build.py` replaces every `{{NAME}}`
placeholder and writes the result to `dist/index.html`; an unsubstituted placeholder fails the
build. Nothing on the page is hand-maintained prose about counts or results:

| Source | What it feeds |
|---|---|
| `crates/puffinparse-core/src/model.rs` | `read_registry()` parses the `PROVIDERS` table — provider id, display name, every `ModelInfo` and its `modes` expression (`PARSE_OCR`, `Mode::ALL`, `&[Mode::Extract]`). It drives the provider grid (one card per provider: id, name, mode chips, model count), the `15 / 53 / 3` stat band, and the list of model strings the hero cycles through. |
| `docs/providers/README.md` | The status column of the provider table → the *live-verified* / *verified locally* / *docs-only* dot on each card, and the per-status counts in the section intro. |
| `benchmark/results/*.json` | `read_leaderboard(7)` — best score per model → the leaderboard table (the same reader the docs home uses with a limit of 5). |
| `website/nav.json` | Every link on the page is `site.url(slug)`, so the landing follows the docs prefix automatically. |

The three literal blocks left in the template are the mode sketches, the compatibility example and
the `bench run` command — illustrations, not data.

`website/assets/landing.css` layers on the same tokens as `style.css` (it never redefines them) and
adds one vermilion accent, a larger type scale and the landing components. Two moments of motion,
both skipped under `prefers-reduced-motion` and both degrading to a complete static page without
JavaScript: the hero scanner (a sweep across a skeleton document, then the response typed out, then
the next model string) and the leaderboard rows fading in on scroll. The provider tab strip is
CSS-only — stacked radio panels in one grid cell, so switching tabs shifts nothing.

## Redirects and Markdown routes

The docs used to live at the site root, so `vercel.json` carries one permanent redirect per docs
slug (`/python/` → `/docs/python/`; `/` belongs to the landing page and `/llms.txt` stays at the
root). Redirects from the old `*.vercel.app` hostnames are domain-level redirects in the Vercel
dashboard, not in `vercel.json`.

Docs URLs also do content negotiation: a request with `Accept: text/markdown` gets the page's
`index.md` (`Content-Type: text/markdown; charset=utf-8`), anything else gets HTML, and both carry
`Vary: Accept`. An unknown path under `/docs/` gets `404.md` with status 404, and `/` gets the docs
home's Markdown. This is a generated `routes` array, one route per page with a `has` condition on
the `accept` header: `rewrites` cannot do it, because Vercel applies them only after the filesystem
and every docs URL is a real `index.html`. The `Vary` header for HTML responses is a static
`headers` rule.
The first generated route pins `/.well-known/api-catalog` (with or without a trailing slash, so
`trailingSlash` cannot redirect the extension-less file to a 404) and sets its RFC 9727 media type
and `Link` header; the hand-maintained `headers` block says the same for static hosts that honour
it.

Both arrays are generated, not hand-written:

```bash
python website/build.py --write-redirects     # rewrites the "redirects" and "routes" arrays
```

`--check` fails if either array has drifted from `nav.json`, or if a redirect or route points at
a file that does not exist in `dist/`. Run `--write-redirects` and commit `vercel.json` whenever a
page is added, renamed or removed.

Test the negotiation against the live site (or a preview URL):

```bash
curl -sI -H 'Accept: text/markdown' https://puffinparse.com/docs/getting-started/   # text/markdown
curl -sI https://puffinparse.com/docs/getting-started/                              # text/html
curl -s  -H 'Accept: text/markdown' https://puffinparse.com/docs/no-such-page/      # 404, Markdown
curl -sI https://puffinparse.com/agents.md                                          # text/markdown
```

`python -m http.server` does not negotiate; locally, fetch `<page>/index.md` directly.

## Benchmark data API

The results viewer's static JSON (`/benchmark-results/data/index.json`, `runs/<run_id>.json`,
`outputs/<run_id>/<model_slug>/<doc>.{md,json}`, `datasets/<name>/manifest.json`) is documented as
a read-only API: the docs page `website/pages/data-api.md` (`/docs/benchmark/data-api/`), the
OpenAPI 3.1 description `/openapi.json` and the RFC 9727 catalog `/.well-known/api-catalog`
(served as `application/linkset+json; profile="https://www.rfc-editor.org/info/rfc9727"` with a
`Link: <...>; rel="api-catalog"` header, both from `vercel.json`).

The schemas live in `website/data_api.py`, derived from the files the viewer build actually
writes. `build.py` writes both documents after the viewer, so the OpenAPI path-parameter examples
are real (the newest run, its top model, a document with both outputs). A change to the result or
manifest format must update `data_api.py` too; `qa.py` fails otherwise.

## Site QA

```bash
python website/build.py --out /tmp/site --site-url https://puffinparse.com --check
python website/qa.py /tmp/site          # stdlib; uses openapi-spec-validator when installed
```

`qa.py` checks a finished build, offline, in about a second: a Markdown negotiation route in
`vercel.json` for every page in `nav.json` (and the API-catalog route and headers); `llms.txt`
structure (H1, summary blockquote, sections, every local link resolves, every page listed);
JSON-LD parses on every HTML page and each carries the three API discovery links; `robots.txt`
names the AI crawlers and its sitemap exists; `/openapi.json` is valid OpenAPI 3.1, every path
with its example parameters is a file in the build, and the published index, runs, manifests and
a sample of model outputs validate against its schemas; `/.well-known/api-catalog` is a valid
Linkset whose links resolve. The `site` job in `.github/workflows/ci.yml` runs the build with
`--check` and then `qa.py` on every push and pull request.

## Design notes

`website/assets/style.css` is the entire design: system font stack, ~76ch measure, hairline
borders, one accent colour used only for links and the active nav item, tabular numerals in
tables, light/dark from `prefers-color-scheme` with a toggle that remembers the choice. Under
1100px the "On this page" column drops; under 860px the sidebar collapses into a
`<details>` menu.

`website/assets/landing.css` is the only other stylesheet. It defines no new base tokens: it reads
`--bg`, `--fg`, `--line`, `--mono` and friends from `style.css` and adds its own `--lp-*` layer on
top, so a change to the design system moves both halves at once.

`website/assets/app.js` serves both halves and is progressive enhancement only — every page is
fully readable with JavaScript disabled. It adds: copy buttons on code blocks and on the landing's
`pip install` pill, the animated model string in the docs hero, the landing's hero scanner and
leaderboard reveal, scroll-spy on "On this page", the theme toggle, and the client-side search
(`/` or `⌘K` to focus). Everything motion-related is skipped under `prefers-reduced-motion`.

Keep `style.css` under ~250 lines, `landing.css` under ~300, and resist adding a build step.

## Third-party assets

Almost everything here is original: the puffin mascot, the mark, the social card and every line of
CSS and JavaScript were made for PuffinParse. The exceptions, and their notices:

| Asset | Where | Source and licence |
|---|---|---|
| GitHub mark in the header star button | `GH_STAR_ICON` in `build.py`, `benchmark/site/src/index.html` | GitHub [Octicons](https://github.com/primer/octicons) `mark-github-16`, Copyright (c) GitHub Inc., MIT. The notice is kept as an HTML comment next to the icon in the served markup. |
| pdf.js 3.11.174 (results viewer only) | loaded on demand from cdnjs, pinned with SRI | [Mozilla pdf.js](https://github.com/mozilla/pdf.js), Apache-2.0. Not vendored. |
| Vercel Web Analytics script | `/_vercel/insights/script.js`, Vercel builds only | Served by Vercel; not vendored. |
| supabase-js 2.117.3 (playground only, when sign-in is configured) | loaded from jsdelivr, pinned with SRI | [Supabase](https://github.com/supabase/supabase-js), MIT. Not vendored. |
| Cloudflare Turnstile (playground only, when configured) | loaded from challenges.cloudflare.com | Served by Cloudflare, which does not allow pinning it; not vendored. |
| Fonts | `tokens.css` | System font stacks only; no font files are shipped or downloaded. |

Benchmark data shown in the viewer keeps its own licence (see each
`benchmark/datasets/*/README.md`). If you add a third-party asset, add a row here and a credit in
the README's Acknowledgements.

## The playground

`/playground/` lets a visitor run one PDF or image through up to three models and compare the
Markdown side by side. It is static: the page talks to the gateway's playground API
([docs/SERVER.md](../docs/SERVER.md), "Playground API") straight from the browser.

- **Samples** need no API at all. `build.py` copies five redistributable documents from the newest
  `combined-vN` run (`PLAYGROUND_SAMPLES`; CC0, Apache-2.0 and MIT sources only, never
  `fetch-required` ones) into `playground/samples/`, with each hosted model's committed output, so
  showing them calls no provider. `--check` refuses a sample with any other licence.
- **Live runs** are switched on by build-time environment variables (set them in the Vercel
  project, never in the repository; all are public identifiers, not secrets):
  `PUFFINPARSE_PLAYGROUND_API` (must be `PLAYGROUND_API_ORIGIN` in `build.py`, or
  `http://localhost:<port>` for local testing), `PUFFINPARSE_SUPABASE_URL` and
  `PUFFINPARSE_SUPABASE_ANON_KEY` (sign-in for the free tier), `PUFFINPARSE_TURNSTILE_SITE_KEY`.
  Without them the page ships with samples only.
- **Model output is untrusted.** The page renders it with its own small Markdown renderer that
  escapes everything, keeps a short list of inline and table tags without attributes, keeps only
  http(s) links and never loads images. `/playground/` has a stricter CSP in `vercel.json` (no
  `'unsafe-inline'` scripts; the one inline script is allowed by hash), which `--check` verifies
  against the built page.
- Owner decisions that are single constants in `build.py`: `PLAYGROUND_API_ORIGIN` (the API host,
  also in the CSP), `FREE_TIER_MAX_PRICE_PER_PAGE` (the gateway has its own copy, which wins) and
  `PLAYGROUND_SAMPLES`.

Test it offline:

```bash
node --test website/tests/*.test.mjs                       # renderer, API client, poller
python website/tests/fake_playground_api.py --port 8766 &  # a fake gateway, no provider calls
PUFFINPARSE_PLAYGROUND_API=http://localhost:8766 python website/build.py --out /tmp/site --no-benchmark
python -m http.server -d /tmp/site 8001                    # http://localhost:8001/playground/
```

With the fake API, any key works and the key `bad` makes that provider's job fail.

## Deploying

Vercel builds and serves the site from `vercel.json`: `buildCommand` runs this script with
`--site-url https://puffinparse.com`, `outputDirectory` is `website/dist`, and `cleanUrls` +
`trailingSlash` give the pretty URLs. The benchmark results viewer is a separate artifact published
under **`/benchmark-results/`**, so the two never collide.

The URL flags are independent. `--base-url` decides how links are *written* in the HTML,
`--docs-prefix` decides where the docs sit below it, and `--site-url` decides the absolute address
used for canonical links and the sitemap. For a project page served at
`https://user.github.io/puffinparse/`, build with `--base-url /puffinparse --site-url
https://user.github.io/puffinparse`. Every internal URL, including `llms.txt` and the sitemap, follows
from those flags.
