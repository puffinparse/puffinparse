#!/usr/bin/env python3
"""Site QA for a fresh ``website/build.py`` output: the agent-facing contract of puffinparse.com.

    python website/build.py --out /tmp/site --site-url https://puffinparse.com --check
    python website/qa.py /tmp/site

Checks, all offline and standard library only (``openapi-spec-validator`` is used as well when
it is importable, as it is in CI):

* vercel.json has a Markdown-negotiation route (``Accept: text/markdown``) for every docs page in
  nav.json, landing on an ``index.md`` that exists, plus the API-catalog route;
* llms.txt has the llmstxt.org structure (H1, summary blockquote, H2 sections), every local link
  in it resolves, and every docs page and the data API are listed;
* every HTML page carries JSON-LD that parses, and the RFC 8631 / RFC 9727 discovery links;
* robots.txt names the AI crawlers and points at a sitemap that exists;
* /openapi.json is a structurally valid OpenAPI 3.1 document, every path (with its example
  parameters) is a file in the build, and the published JSON validates against its schemas;
* /.well-known/api-catalog is a valid RFC 9727 Linkset whose links resolve, served (per
  vercel.json) as ``application/linkset+json``.

Exit status 1 on any problem, with one line per problem.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.parse
from pathlib import Path
from typing import Any, Optional

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "website"))

import data_api  # noqa: E402  (a sibling module, not an installed package)

BENCH = "benchmark-results"
# The crawlers the site explicitly welcomes; robots.txt must name each of them.
AI_CRAWLERS = (
    "GPTBot",
    "ChatGPT-User",
    "OAI-SearchBot",
    "ClaudeBot",
    "Claude-User",
    "Claude-SearchBot",
    "PerplexityBot",
    "Google-Extended",
    "Applebot-Extended",
    "CCBot",
)
JSONLD_RE = re.compile(r'<script type="application/ld\+json">(.*?)</script>', re.S)
LINK_TAG_RE = re.compile(r"<link\b[^>]*>", re.I)
ATTR_RE = re.compile(r'([a-zA-Z-]+)="([^"]*)"')
MD_LINK_RE = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
MD_ACCEPT = "(.*)text/markdown(.*)"
# Model outputs are thousands of files; validating this many keeps the check under a second.
OUTPUT_SAMPLE = 150


class QA:
    def __init__(self, out: Path, vercel: Path, nav: Path) -> None:
        self.out = out
        self.vercel = json.loads(vercel.read_text(encoding="utf-8"))
        self.nav = json.loads(nav.read_text(encoding="utf-8"))
        self.problems: list[str] = []
        self.counts: dict[str, int] = {}
        spec_path = out / data_api.OPENAPI_PATH
        self.spec: dict[str, Any] = {}
        if spec_path.is_file():
            self.spec = json.loads(spec_path.read_text(encoding="utf-8"))
        servers = self.spec.get("servers") or [{"url": ""}]
        self.server = urllib.parse.urlsplit(str(servers[0].get("url", "")))

    def fail(self, message: str) -> None:
        self.problems.append(message)

    def count(self, what: str, n: int = 1) -> None:
        self.counts[what] = self.counts.get(what, 0) + n

    def local(self, url: str) -> Optional[Path]:
        """The build file an absolute or site-relative URL maps to (None if off-site)."""
        parts = urllib.parse.urlsplit(url)
        if parts.scheme and parts.netloc != self.server.netloc:
            return None
        path = urllib.parse.unquote(parts.path)
        base = self.server.path.rstrip("/")
        if base and path.startswith(base + "/"):
            path = path[len(base) :]
        rel = path.lstrip("/")
        return self.out / (rel + "index.html" if rel.endswith("/") or not rel else rel)

    def resolves(self, url: str, where: str) -> None:
        target = self.local(url)
        if target is not None and not target.is_file():
            self.fail(f"{where}: link {url} -> missing {target.relative_to(self.out)}")

    # ------------------------------------------------------------------------------ checks

    def docs_pages(self) -> list[dict[str, Any]]:
        return [p for s in self.nav["sections"] for p in s["pages"]]

    def check_negotiation(self) -> None:
        routes = self.vercel.get("routes", [])
        by_src = {r.get("src"): r for r in routes}
        for page in self.docs_pages():
            slug = page["slug"].strip("/")
            src = "^/docs/?$" if not slug else f"^/docs/{slug}/?$"
            route = by_src.get(src)
            if route is None:
                self.fail(f"vercel.json: no Markdown route for /docs/{slug + '/' if slug else ''}")
                continue
            if route.get("has") != [{"type": "header", "key": "accept", "value": MD_ACCEPT}]:
                self.fail(f"vercel.json: route {src} does not match on Accept: text/markdown")
            if not str(route.get("headers", {}).get("Content-Type", "")).startswith("text/markdown"):
                self.fail(f"vercel.json: route {src} does not set Content-Type text/markdown")
            dest = self.out / str(route.get("dest", "")).lstrip("/")
            if not dest.is_file():
                self.fail(f"vercel.json: route {src} -> missing {route.get('dest')}")
            self.count("markdown routes")
        if "^/$" not in by_src:
            self.fail("vercel.json: no Markdown route for /")
        catalog = next((r for r in routes if r.get("dest") == "/" + data_api.API_CATALOG_PATH), None)
        if catalog is None:
            self.fail("vercel.json: no route for /.well-known/api-catalog")
        elif catalog.get("headers", {}).get("Content-Type") != data_api.API_CATALOG_TYPE:
            self.fail("vercel.json: the api-catalog route does not set the RFC 9727 media type")
        header = next(
            (h for h in self.vercel.get("headers", []) if h.get("source") == "/" + data_api.API_CATALOG_PATH),
            None,
        )
        values = {h["key"]: h["value"] for h in (header or {}).get("headers", [])}
        if values.get("Content-Type") != data_api.API_CATALOG_TYPE:
            self.fail(
                "vercel.json: headers do not serve /.well-known/api-catalog as application/linkset+json"
            )

    def check_llms(self) -> None:
        path = self.out / "llms.txt"
        if not path.is_file():
            self.fail("llms.txt: missing")
            return
        text = path.read_text(encoding="utf-8")
        lines = text.splitlines()
        if not lines or not lines[0].startswith("# "):
            self.fail("llms.txt: does not start with an H1")
        first = next((ln for ln in lines[1:] if ln.strip()), "")
        if not first.startswith("> "):
            self.fail("llms.txt: the H1 is not followed by a summary blockquote")
        sections = [ln[3:].strip() for ln in lines if ln.startswith("## ")]
        for required in ("Docs", "Optional", "Benchmark data"):
            if required not in sections:
                self.fail(f"llms.txt: no '## {required}' section")
        links = MD_LINK_RE.findall(text)
        for url in links:
            self.resolves(url, "llms.txt")
        self.count("llms.txt links", len(links))
        for page in self.docs_pages():
            slug = page["slug"].strip("/")
            md = f"/docs/{slug}/index.md" if slug else "/docs/index.md"
            if not any(urllib.parse.urlsplit(u).path.endswith(md) for u in links):
                self.fail(f"llms.txt: docs page {md} is not listed")
        for needed in (data_api.OPENAPI_PATH, data_api.API_CATALOG_PATH, data_api.DOCS_SLUG):
            if needed not in text:
                self.fail(f"llms.txt: does not link {needed}")

    def html_pages(self) -> list[Path]:
        viewer = self.out / BENCH
        pages = [f for f in sorted(self.out.rglob("*.html")) if viewer not in f.parents]
        if (viewer / "index.html").is_file():
            pages.append(viewer / "index.html")
        return pages

    def check_html(self) -> None:
        for f in self.html_pages():
            rel = f.relative_to(self.out)
            text = f.read_text(encoding="utf-8")
            head = text.split("</head>", 1)[0]
            blocks = JSONLD_RE.findall(text)
            if not blocks and f.name != "404.html":
                self.fail(f"{rel}: no JSON-LD")
            for block in blocks:
                try:
                    data = json.loads(block)
                except ValueError as exc:
                    self.fail(f"{rel}: JSON-LD does not parse ({exc})")
                    continue
                if data.get("@context") != "https://schema.org":
                    self.fail(f"{rel}: JSON-LD @context is not https://schema.org")
            rels: dict[str, str] = {}
            for tag in LINK_TAG_RE.findall(head):
                attrs = dict(ATTR_RE.findall(tag))
                if "rel" in attrs and "href" in attrs:
                    rels.setdefault(attrs["rel"], attrs["href"])
            for rel_name in ("api-catalog", "service-desc", "service-doc"):
                href = rels.get(rel_name)
                if href is None:
                    self.fail(f'{rel}: no <link rel="{rel_name}"> in the head')
                else:
                    self.resolves(href, str(rel))
            self.count("HTML pages")

    def check_robots(self) -> None:
        path = self.out / "robots.txt"
        if not path.is_file():
            self.fail("robots.txt: missing")
            return
        text = path.read_text(encoding="utf-8")
        agents = {m.strip() for m in re.findall(r"^User-agent:\s*(.+)$", text, re.M)}
        for bot in AI_CRAWLERS:
            if bot not in agents:
                self.fail(f"robots.txt: does not name {bot}")
        sitemaps = re.findall(r"^Sitemap:\s*(\S+)$", text, re.M)
        if not sitemaps:
            self.fail("robots.txt: no Sitemap line")
        for url in sitemaps:
            self.resolves(url, "robots.txt")

    # --------------------------------------------------------------------------- OpenAPI

    def check_openapi(self) -> None:
        if not self.spec:
            self.fail(f"{data_api.OPENAPI_PATH}: missing")
            return
        spec = self.spec
        try:  # the full validator when installed (CI installs it)
            from openapi_spec_validator import validate as validate_spec  # type: ignore[import-not-found]
        except ImportError:
            validate_spec = None
        if validate_spec is not None:
            try:
                validate_spec(spec)
                self.count("openapi-spec-validator runs")
            except Exception as exc:  # the validator raises its own exception types
                self.fail(f"{data_api.OPENAPI_PATH}: invalid ({str(exc).splitlines()[0]})")
        if not str(spec.get("openapi", "")).startswith("3.1."):
            self.fail(f"{data_api.OPENAPI_PATH}: 'openapi' is not 3.1.x")
        info = spec.get("info", {})
        if not (info.get("title") and info.get("version")):
            self.fail(f"{data_api.OPENAPI_PATH}: info.title / info.version missing")
        components = spec.get("components", {}).get("schemas", {})
        for ref in re.findall(r'"\$ref":\s*"([^"]+)"', json.dumps(spec)):
            name = ref.rsplit("/", 1)[-1]
            if not ref.startswith("#/components/schemas/") or name not in components:
                self.fail(f"{data_api.OPENAPI_PATH}: dangling $ref {ref}")
        op_ids: set[str] = set()
        viewer_built = (self.out / BENCH / "data" / "index.json").is_file()
        for template, item in spec.get("paths", {}).items():
            names = re.findall(r"\{([^}]+)\}", template)
            for method, op in item.items():
                op_id = op.get("operationId")
                if not op_id or op_id in op_ids:
                    self.fail(f"{data_api.OPENAPI_PATH}: {method} {template} needs a unique operationId")
                op_ids.add(op_id)
                params = {p["name"]: p for p in op.get("parameters", []) if p.get("in") == "path"}
                if set(params) != set(names):
                    self.fail(f"{data_api.OPENAPI_PATH}: {template} path parameters do not match")
                    continue
                if not viewer_built:
                    continue
                path = template
                for name in names:
                    example = params[name].get("example")
                    if example is None:
                        self.fail(f"{data_api.OPENAPI_PATH}: {template} has no example for {name}")
                        break
                    path = path.replace("{" + name + "}", str(example))
                else:
                    target = self.local(self.server.path.rstrip("/") + path)
                    if target is None or not target.is_file():
                        self.fail(f"{data_api.OPENAPI_PATH}: example {path} is not a file in the build")
                    else:
                        self.count("OpenAPI example paths")
                        self.validate_response(op, target, components)
        if viewer_built:
            self.validate_data(components)

    def validate_response(self, op: dict[str, Any], target: Path, components: dict[str, Any]) -> None:
        content = op.get("responses", {}).get("200", {}).get("content", {})
        schema = content.get("application/json", {}).get("schema")
        if schema is not None:
            self.validate_file(target, schema, components)

    def validate_file(self, path: Path, schema: dict[str, Any], components: dict[str, Any]) -> None:
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except ValueError as exc:
            self.fail(f"{path.relative_to(self.out)}: not JSON ({exc})")
            return
        errors: list[str] = []
        validate(data, schema, components, "$", errors)
        for err in errors[:5]:
            self.fail(f"{path.relative_to(self.out)}: {err}")
        self.count("JSON files validated against the schemas")

    def validate_data(self, components: dict[str, Any]) -> None:
        """Every published run, manifest and the index, plus a sample of unified responses."""
        data = self.out / BENCH / "data"
        self.validate_file(data / "index.json", {"$ref": "#/components/schemas/Index"}, components)
        for run in sorted((data / "runs").glob("*.json")):
            self.validate_file(run, {"$ref": "#/components/schemas/Run"}, components)
        for manifest in sorted((data / "datasets").glob("*/manifest.json")):
            self.validate_file(manifest, {"$ref": "#/components/schemas/Manifest"}, components)
        outputs = sorted((data / "outputs").rglob("*.json"))
        step = max(1, len(outputs) // OUTPUT_SAMPLE)
        for output in outputs[::step]:
            self.validate_file(output, {"$ref": "#/components/schemas/ParseResponse"}, components)

    # ----------------------------------------------------------------------- API catalog

    def check_catalog(self) -> None:
        path = self.out / data_api.API_CATALOG_PATH
        if not path.is_file():
            self.fail(f"{data_api.API_CATALOG_PATH}: missing")
            return
        try:
            catalog = json.loads(path.read_text(encoding="utf-8"))
        except ValueError as exc:
            self.fail(f"{data_api.API_CATALOG_PATH}: not JSON ({exc})")
            return
        linkset = catalog.get("linkset") if isinstance(catalog, dict) else None
        if not isinstance(linkset, list) or not linkset:
            self.fail(f"{data_api.API_CATALOG_PATH}: no 'linkset' array (RFC 9264)")
            return
        anchors = [ctx.get("anchor") for ctx in linkset]
        if not any(str(a).endswith("/" + data_api.API_CATALOG_PATH) for a in anchors):
            self.fail(f"{data_api.API_CATALOG_PATH}: no context anchored at the catalog itself")
        relations: set[str] = set()
        for ctx in linkset:
            if not isinstance(ctx.get("anchor"), str):
                self.fail(f"{data_api.API_CATALOG_PATH}: a link context has no anchor")
            for rel, targets in ctx.items():
                if rel == "anchor":
                    continue
                relations.add(rel)
                if not isinstance(targets, list) or not all(isinstance(t.get("href"), str) for t in targets):
                    self.fail(f"{data_api.API_CATALOG_PATH}: '{rel}' must be an array of {{href}} objects")
                    continue
                for t in targets:
                    self.resolves(t["href"], data_api.API_CATALOG_PATH)
        for rel in ("item", "service-desc", "service-doc"):
            if rel not in relations:
                self.fail(f"{data_api.API_CATALOG_PATH}: no '{rel}' link")

    def run(self) -> int:
        self.check_negotiation()
        self.check_llms()
        self.check_html()
        self.check_robots()
        self.check_openapi()
        self.check_catalog()
        for p in self.problems:
            print(f"  qa: {p}", file=sys.stderr)
        summary = ", ".join(f"{n} {what}" for what, n in self.counts.items())
        print(f"site QA: {summary}; {len(self.problems)} problem(s)")
        return 1 if self.problems else 0


# ------------------------------------------------------------------- minimal JSON Schema


def _type_ok(value: Any, kind: str) -> bool:
    if kind == "object":
        return isinstance(value, dict)
    if kind == "array":
        return isinstance(value, list)
    if kind == "string":
        return isinstance(value, str)
    if kind == "boolean":
        return isinstance(value, bool)
    if kind == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if kind == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind == "null":
        return value is None
    return True


def validate(
    value: Any, schema: dict[str, Any], components: dict[str, Any], at: str, errors: list[str]
) -> None:
    """The JSON Schema subset the data API schemas use: $ref, type (or a list of types),
    properties, required, items and additionalProperties-as-a-schema."""
    if len(errors) > 20:
        return
    if "$ref" in schema:
        schema = components[schema["$ref"].rsplit("/", 1)[-1]]
    kinds = schema.get("type")
    if kinds is not None:
        kinds = [kinds] if isinstance(kinds, str) else kinds
        if not any(_type_ok(value, k) for k in kinds):
            errors.append(f"{at}: expected {'|'.join(kinds)}, got {type(value).__name__}")
            return
    if isinstance(value, dict):
        for key in schema.get("required", []):
            if key not in value:
                errors.append(f"{at}: missing required '{key}'")
        props = schema.get("properties", {})
        extra = schema.get("additionalProperties")
        for key, item in value.items():
            if key in props:
                validate(item, props[key], components, f"{at}.{key}", errors)
            elif isinstance(extra, dict):
                validate(item, extra, components, f"{at}[{key!r}]", errors)
    elif isinstance(value, list) and isinstance(schema.get("items"), dict):
        for i, item in enumerate(value):
            validate(item, schema["items"], components, f"{at}[{i}]", errors)


def main(argv: Optional[list[str]] = None) -> int:
    ap = argparse.ArgumentParser(description="QA a built PuffinParse site (offline).")
    ap.add_argument("out", help="the directory website/build.py wrote")
    ap.add_argument("--vercel", default=str(ROOT / "vercel.json"))
    ap.add_argument("--nav", default=str(ROOT / "website" / "nav.json"))
    args = ap.parse_args(argv)
    out = Path(args.out).resolve()
    if not (out / "index.html").is_file():
        print(f"error: {out} is not a site build (no index.html)", file=sys.stderr)
        return 2
    return QA(out, Path(args.vercel), Path(args.nav)).run()


if __name__ == "__main__":
    raise SystemExit(main())
