# Security Policy

## Supported versions

PuffinParse is pre-1.0. Security fixes land on `main` and ship in the next release;
only the latest released version is supported.

| Version | Supported |
|---|---|
| 0.1.x | ✅ |
| < 0.1 | ❌ |

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately with GitHub's private vulnerability reporting, which is
enabled for this repository:

1. Go to https://github.com/ajinkyashejul/puffinparse/security/advisories/new
   (repository → **Security** → **Report a vulnerability**).
2. Describe the issue, the affected version or commit, and — if you can — a
   minimal reproduction and the impact you believe it has.

You will get an acknowledgement within 5 business days and an assessment with a
fix timeline within 10. We will keep you updated while we work on a fix, credit
you in the advisory and `CHANGELOG.md` unless you prefer otherwise, and publish
the advisory once a fixed release is out. Please give us a reasonable window to
ship a fix before disclosing publicly.

If GitHub advisories are unavailable to you, open a public issue that says only
that you have a security report and asks a maintainer to make contact — no
details — and we will take it from there.

## What is in scope

- The Rust crates (`puffinparse-core`, `puffinparse-cli`, `puffinparse-python`,
  `puffinparse-node`, `puffinparse-server`), the Python package `puffinparse` and
  the Node package `puffinparse`.
- The self-hosted gateway (`puffinparse serve`): authentication with virtual
  keys, budget and rate-limit enforcement, and anything that could leak a
  provider key or document content through responses, logs or metrics.
- The release and CI workflows in `.github/workflows/`, and the published
  artifacts (PyPI wheels/sdist, GitHub Release binaries).

Out of scope: vulnerabilities in the third-party providers themselves (report
those to the provider directly), and issues that
require an already-compromised machine or a malicious local Rust/Python
dependency you introduced.

## How PuffinParse handles credentials

- **Provider API keys are only read from the environment** (each provider's
  own variable, such as `REDUCTO_API_KEY`; `.env.example` lists them all), passed
  explicitly as the `api_key` argument, or, for the gateway, referenced as
  `env:` values in its config file. PuffinParse never reads them from anywhere else, never writes them to
  disk, and never sends them anywhere but the provider's own base URL.
- **Keys are never logged.** `PUFFINPARSE_LOG=debug` traces requests, retries and
  polling, but `Authorization` headers and key values are redacted; errors carry
  provider, status code, message and request/job id only. If you ever see a key
  in log output, in an error message, or in a serialized response, that is
  a vulnerability — please report it.
- Document bytes are sent only to the selected provider. PuffinParse has no
  telemetry and makes no network calls other than to the provider you choose
  (and, for a URL input to a provider that cannot fetch URLs, to that URL; see
  below).
- Recorded test fixtures under `crates/puffinparse-core/tests/fixtures/` (and the
  sample responses in `docs/providers/`) must be redacted; never commit a fixture
  containing a real key, token, signed URL, dashboard link, or run, job, file or
  project id tied to a live account. Replace ids with obviously fake ones of the
  same format (see the fixtures README).
- Committed benchmark results are the one deliberate exception: each document
  keeps its `provider_job_id` so a run can be audited against the provider's
  dashboard (an id grants no access without the account's key). Dashboard and
  studio links in the saved raw responses (`benchmark/results/outputs/*/*.json`)
  are replaced; the scored `.md` outputs are never edited.
- Releases are published to PyPI with trusted publishing (OIDC), so no
  long-lived PyPI token exists in this repository's secrets.

## Document URLs

A URL input is handed to the provider when its API accepts URLs (Reducto, Extend,
LlamaParse parse, Mistral, Azure, Datalab, Mathpix, Landing AI, OpenDocRouter).
Every other provider (Tesseract, Docling, PaddleOCR, vLLM, Unstructured, Textract,
Gemini, the OpenAI and Anthropic vision models, Upstage, Document AI, LlamaParse
extract) needs the bytes, so PuffinParse downloads the URL itself, in your
process. Whoever chooses that URL chooses where your machine connects, so the
download (`puffinparse_core::fetch`) is restricted by default, in the SDKs, the
CLI and the gateway alike:

- `http` and `https` only, no `user:password@` in the URL;
- the host is resolved by PuffinParse and loopback, private (RFC 1918),
  link-local (including the cloud metadata address `169.254.169.254`), CGNAT
  (`100.64.0.0/10`), unique-local (`fc00::/7`), unspecified, multicast,
  broadcast, documentation and reserved addresses, and IPv6 forms embedding any
  of them, are refused; the connection goes only to an address that passed the
  check, so DNS rebinding cannot swap one in afterwards;
- proxy environment variables are ignored for these downloads;
- at most 5 redirects, each target checked the same way;
- at most 50 MiB (`PUFFINPARSE_MAX_DOWNLOAD_MB`), read within the request's
  `timeout`;
- error messages carry the HTTP status, never the response body.

Docling and PaddleOCR used to pass URLs on to their server; they now download them
the same way and send bytes, so a self-hosted server on your network is never asked
to fetch a URL either.

For a trusted setup that needs private addresses (a document server on your
LAN), set `PUFFINPARSE_ALLOW_PRIVATE_URLS=1` in the process environment. There is
deliberately no per-request option for it. The gateway ignores the variable and
uses its own settings instead (`server.fetch_document_urls`,
`server.allow_private_document_urls`, `server.max_download_mb`; see
[docs/SERVER.md](docs/SERVER.md#hardening)).
