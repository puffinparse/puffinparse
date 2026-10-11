"""A fake playground API for clicking through /playground/ locally, with no provider calls.

It speaks the contract in docs/SERVER.md ("Playground API") closely enough for the page:
GET /v1/playground/config, POST /v1/playground/runs and GET /v1/jobs/{id}. Bring-your-own-key
runs need an ``x-provider-key-<provider>`` header per provider; the key ``bad`` makes that
provider's job fail with ``authentication_error``. Each job succeeds two polls after submit.
The free tier answers ``free_tier_unavailable`` (there is no Supabase locally).

    python website/tests/fake_playground_api.py --port 8766
    PUFFINPARSE_PLAYGROUND_API=http://localhost:8766 python website/build.py --out /tmp/site
"""

from __future__ import annotations

import argparse
import itertools
import json
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

MODELS = [
    ("reducto/standard", 0.015),
    ("reducto/r-1", 0.010),
    ("extend/parse_light", 0.00625),
    ("llamaparse/fast", 0.00125),
    ("llamaparse/cost_effective", 0.00375),
    ("llamaparse/agentic", 0.0125),
]
LIMITS = {
    "max_file_bytes": 4 * 1024 * 1024,
    "models_per_run": 3,
    "pages_per_run": 10,
    "model_pages_per_day": 30,
}
# Untrusted provider output: the page must render the Markdown and neutralise the rest.
MARKDOWN = """# Fake result for {model}

| Item | Qty |
|---|---|
| Widget | 2 |

Some **bold** text, a [safe link](https://example.com) and a [bad one](javascript:alert(1)).

<img src=x onerror="document.title='pwned'"><script>document.title='pwned'</script>
"""

JOBS: dict[str, dict[str, Any]] = {}
IDS = itertools.count(1)


class Handler(BaseHTTPRequestHandler):
    def _send(self, status: int, body: Any, extra: dict[str, str] | None = None) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Access-Control-Allow-Origin", self.headers.get("Origin") or "*")
        self.send_header("Vary", "Origin")
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def _error(self, status: int, kind: str, message: str, **details: Any) -> None:
        self._send(status, {"error": {"type": kind, "message": message, "details": details or None}})

    def do_OPTIONS(self) -> None:
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", self.headers.get("Origin") or "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST")
        self.send_header(
            "Access-Control-Allow-Headers",
            "authorization, x-provider-key-reducto, x-provider-key-extend, x-provider-key-llamaparse",
        )
        self.send_header("Access-Control-Max-Age", "600")
        self.end_headers()

    def do_GET(self) -> None:
        if self.path == "/v1/playground/config":
            return self._send(
                200,
                {
                    "limits": LIMITS,
                    "free_tier": {"available": False, "reason": "paused", "user": None},
                    "byok": {"available": True},
                    "models": [{"id": m, "free_tier": p <= 0.025, "byok": True} for m, p in MODELS],
                },
            )
        match = re.fullmatch(r"/v1/jobs/([a-z0-9-]+)", self.path)
        if not match or match.group(1) not in JOBS:
            return self._error(404, "not_found", "no such job")
        job = JOBS[match.group(1)]
        provider = job["model"].split("/")[0]
        if self.headers.get(f"x-provider-key-{provider}") != job["key"]:
            return self._error(404, "not_found", "no such job")  # jobs are owned by the key that made them
        job["polls"] += 1
        if job["polls"] < 2:
            return self._send(200, {"id": match.group(1), "status": "pending"})
        if job["key"] == "bad":
            return self._send(
                200,
                {
                    "id": match.group(1),
                    "status": "failed",
                    "error": {"type": "authentication_error", "message": f"{provider} rejected the API key"},
                },
            )
        price = dict(MODELS)[job["model"]]
        return self._send(
            200,
            {
                "id": match.group(1),
                "status": "succeeded",
                "result": {
                    "model": job["model"],
                    "markdown": MARKDOWN.format(model=job["model"]),
                    "usage": {"pages": job["pages"]},
                    "cost_usd": round(price * job["pages"], 6),
                    "latency_ms": 1800 + 700 * job["polls"],
                },
            },
        )

    def do_POST(self) -> None:
        if self.path != "/v1/playground/runs":
            return self._error(404, "not_found", "no such route")
        length = int(self.headers.get("Content-Length") or 0)
        if length > LIMITS["max_file_bytes"] + 64 * 1024:
            return self._error(413, "payload_too_large", "file too large")
        body = self.rfile.read(length)
        models = [m.decode() for m in re.findall(rb'name="models"\r\n\r\n([^\r]+)\r\n', body)]
        if not models:
            return self._error(400, "invalid_request", "pick at least one model")
        if len(models) > LIMITS["models_per_run"]:
            return self._error(400, "too_many_models", "too many models", limit=LIMITS["models_per_run"])
        if self.headers.get("Authorization"):
            return self._error(503, "free_tier_unavailable", "the free tier is paused", reason="paused")
        pages = max(1, len(re.findall(rb"/Type\s*/Page(?![a-zA-Z])", body)))
        jobs = []
        for model in models:
            provider = model.split("/")[0]
            key = self.headers.get(f"x-provider-key-{provider}")
            if not key:
                jobs.append(
                    {
                        "model": model,
                        "id": None,
                        "status": "failed",
                        "error": {"type": "missing_provider_key", "message": f"no key for {provider}"},
                    }
                )
                continue
            job_id = f"job-{next(IDS)}"
            JOBS[job_id] = {"model": model, "key": key, "pages": pages, "polls": 0}
            jobs.append({"model": model, "id": job_id, "status": "pending", "error": None})
        run = {"id": f"run-{next(IDS)}", "mode": "byok", "pages": pages, "jobs": jobs, "usage": None}
        self._send(202, run)

    def log_message(self, fmt: str, *args: Any) -> None:  # keys travel in headers, never logged
        print(f"{self.command} {self.path.split('?')[0]} -> {args[1] if len(args) > 1 else ''}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=8766)
    args = parser.parse_args()
    print(f"fake playground API on http://localhost:{args.port}")
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
