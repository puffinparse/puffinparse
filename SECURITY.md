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
  telemetry and makes no network calls other than to the provider you choose.
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
