"""The benchmark data as a read-only JSON API: the OpenAPI 3.1 description and the RFC 9727
API catalog that ``website/build.py`` publishes at ``/openapi.json`` and
``/.well-known/api-catalog``.

The "API" is the static JSON the results viewer build (``benchmark/site/build.py``) already
emits under ``/benchmark-results/data/``; nothing here adds an endpoint. The schemas below
describe exactly those files and were derived from them (every key that appears in every
committed file is ``required``, the rest are optional). ``website/qa.py`` validates every
published index, run and manifest (and a sample of model outputs) against these schemas, so
a change to the result format that is not reflected here fails CI.

Standard library only: imported by both ``website/build.py`` and ``website/qa.py``.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional

OPENAPI_PATH = "openapi.json"
API_CATALOG_PATH = ".well-known/api-catalog"
# RFC 9727 section 4: the catalog is a Linkset (RFC 9264) with this profile.
API_CATALOG_PROFILE = "https://www.rfc-editor.org/info/rfc9727"
API_CATALOG_TYPE = f'application/linkset+json; profile="{API_CATALOG_PROFILE}"'
# The IANA-registered OpenAPI media type, used as the `type` hint on service-desc links.
OPENAPI_TYPE = "application/vnd.oai.openapi+json;version=3.1"
# The docs page that describes the data API (the RFC 8631 `service-doc`).
DOCS_SLUG = "benchmark/data-api"

# ------------------------------------------------------------------------------------- schemas


def _ref(name: str) -> dict[str, Any]:
    return {"$ref": f"#/components/schemas/{name}"}


def _map(values: dict[str, Any], description: str) -> dict[str, Any]:
    return {"type": "object", "description": description, "additionalProperties": values}


STR: dict[str, Any] = {"type": "string"}
INT: dict[str, Any] = {"type": "integer"}
NUM: dict[str, Any] = {"type": "number"}
BOOL: dict[str, Any] = {"type": "boolean"}
STR_LIST: dict[str, Any] = {"type": "array", "items": STR}


def _nullable(schema: dict[str, Any]) -> dict[str, Any]:
    return {**schema, "type": [schema["type"], "null"]}


SCORE = {"type": "number", "description": "0..1, higher is better."}
ERROR_RATE = {"type": "number", "description": "0..1 (can exceed 1), lower is better."}

METRIC_PROPS: dict[str, Any] = {
    "headline": {**SCORE, "description": "The number models are ranked by (0..1)."},
    "char_similarity": SCORE,
    "cer": {**ERROR_RATE, "description": "Character error rate."},
    "wer": {**ERROR_RATE, "description": "Word error rate."},
    "word_f1": SCORE,
    "order_score": {**SCORE, "description": "Reading-order agreement."},
    "table_score": {**SCORE, "description": "Table similarity (documents with tables only)."},
    "teds_grid": {**SCORE, "description": "Tree-edit-distance similarity of table grids."},
    "rule_pass_rate": {**SCORE, "description": "Share of assertions passed (rules documents only)."},
    "overall": {"type": "number", "description": "`headline` x 100."},
}

SCHEMAS: dict[str, Any] = {
    "Index": {
        "type": "object",
        "description": "Every run, every dataset and the labels the viewer shows. The leaderboard "
        "of a run is `runs[i].models`, already sorted by `summary.headline`, best first.",
        "required": ["generated_at", "repo", "base", "datasets", "labels", "runs"],
        "properties": {
            "generated_at": {"type": "string", "format": "date-time"},
            "repo": {"type": "string", "format": "uri"},
            "base": {"type": "string", "description": "URL prefix the viewer was built for."},
            "datasets": _map(_ref("DatasetSummary"), "Dataset name -> summary."),
            "labels": _ref("Labels"),
            "runs": {
                "type": "array",
                "description": "Newest first (`created_at`, then `run_id`).",
                "items": _ref("RunIndexEntry"),
            },
        },
    },
    "DatasetSummary": {
        "type": "object",
        "required": ["name", "documents", "categories"],
        "properties": {
            "name": STR,
            "version": _nullable(STR),
            "description": _nullable(STR),
            "license": _nullable({"type": "string", "description": "SPDX id, or `mixed` for unions."}),
            "generator": _nullable(STR),
            "sources": {
                "type": ["array", "null"],
                "description": "The source datasets of a combined dataset.",
                "items": _ref("DatasetSource"),
            },
            "attribution": _nullable(STR),
            "documents": INT,
            "kinds": _map(INT, "Document kind (`transcript`, `rules`) -> count."),
            "categories": STR_LIST,
            "missing": {"type": "boolean", "description": "Present (true) when the manifest was not found."},
        },
    },
    "DatasetSource": {
        "type": "object",
        "required": ["name", "version", "license", "path", "documents", "manifest_sha256"],
        "properties": {
            "name": STR,
            "version": STR,
            "license": STR,
            "path": {"type": "string", "description": "Relative to the combined dataset's manifest."},
            "documents": INT,
            "manifest_sha256": STR,
            "attribution": STR,
        },
    },
    "Labels": {
        "type": "object",
        "required": ["sources", "categories"],
        "properties": {
            "sources": _map(STR, "Source id (`olmocr`) -> display name (`olmOCR-bench`)."),
            "categories": _map(STR, "Category id (`table_tests`) -> display name (`Tables`)."),
        },
    },
    "RunDataset": {
        "type": "object",
        "required": ["name", "version", "documents", "sha256"],
        "properties": {
            "name": STR,
            "version": STR,
            "documents": INT,
            "sha256": {
                "type": "string",
                "description": "Covers the manifest and every input, truth and rule file.",
            },
        },
    },
    "Normalize": {
        "type": "object",
        "properties": {"case_insensitive": BOOL, "strip_markdown": BOOL, "strip_punctuation": BOOL},
    },
    "RunIndexEntry": {
        "type": "object",
        "description": "A run without its per-document records (those are in `file`).",
        "required": ["run_id", "dataset", "categories", "models", "outputs", "file"],
        "properties": {
            "run_id": STR,
            "created_at": _nullable({"type": "string", "format": "date-time"}),
            "puffinparse_version": _nullable(STR),
            "scorer_version": _nullable({"type": "integer", "description": "Absent/null means scorer v1."}),
            "dataset": _ref("RunDataset"),
            "normalize": _ref("Normalize"),
            "categories": STR_LIST,
            "models": {
                "type": "array",
                "description": "Sorted by `summary.headline`, best first: this is the leaderboard.",
                "items": _ref("RankedModel"),
            },
            "outputs": _map(_ref("OutputInventory"), "Model slug -> which saved outputs exist."),
            "file": {"type": "string", "description": "The full run, relative to `/benchmark-results/`."},
        },
    },
    "RankedModel": {
        "type": "object",
        "required": ["model", "slug", "summary"],
        "properties": {
            "model": {"type": "string", "description": "`<provider>/<model>`, e.g. `reducto/r-1`."},
            "slug": {
                "type": "string",
                "description": "`model` with `/` replaced by `_` (output directory name).",
            },
            "summary": _ref("ModelSummary"),
        },
    },
    "OutputInventory": {
        "type": "object",
        "required": ["json", "missing"],
        "properties": {
            "json": {
                **STR_LIST,
                "description": "Document ids that also have a `<doc>.json` unified response.",
            },
            "missing": {**STR_LIST, "description": "Document ids with no published `<doc>.md`."},
        },
    },
    "ModelSummary": {
        "type": "object",
        "required": ["documents", "failed", "overall"],
        "properties": {
            "documents": INT,
            "failed": INT,
            **METRIC_PROPS,
            "latency_p50_ms": INT,
            "latency_p95_ms": INT,
            "latency_per_page_ms": NUM,
            "total_pages": INT,
            "total_cost_usd": NUM,
            "cost_per_1k_pages_usd": NUM,
            "empty_outputs": INT,
            "by_category": _map(
                _ref("CategorySummary"), "Category id -> the same metrics over that category."
            ),
        },
    },
    "CategorySummary": {
        "type": "object",
        "required": ["documents", "failed", "overall"],
        "properties": {"documents": INT, "failed": INT, **METRIC_PROPS},
    },
    "Run": {
        "type": "object",
        "description": "One benchmark result file as committed under `benchmark/results/` "
        "(docs/SPEC.md section 10.4), with every per-document score.",
        "required": ["run_id", "dataset", "models"],
        "properties": {
            "run_id": STR,
            "created_at": {"type": "string", "format": "date-time"},
            "puffinparse_version": STR,
            "scorer_version": INT,
            "rescored_at": {"type": "string", "format": "date-time"},
            "rescore_kept_docs": INT,
            "dataset": _ref("RunDataset"),
            "normalize": _ref("Normalize"),
            "models": {"type": "array", "items": _ref("RunModel")},
        },
    },
    "RunModel": {
        "type": "object",
        "required": ["model", "docs", "summary"],
        "properties": {
            "model": STR,
            "summary": _ref("ModelSummary"),
            "docs": {"type": "array", "items": _ref("DocumentScore")},
        },
    },
    "DocumentScore": {
        "type": "object",
        "description": "One model on one document. A failed call has `error` (and `error_kind`) "
        "instead of `metrics` / `headline` / `cost_usd`.",
        "required": ["id", "category", "kind", "table_only", "pages", "latency_ms"],
        "properties": {
            "id": {"type": "string", "description": "Document id; may contain `/` (`dpbench/0103...`)."},
            "category": STR,
            "kind": {"type": "string", "description": "`transcript` (scored against truth) or `rules`."},
            "table_only": BOOL,
            "pages": INT,
            "metrics": _ref("DocumentMetrics"),
            "headline": SCORE,
            "latency_ms": INT,
            "cost_usd": NUM,
            "empty_output": BOOL,
            "error": STR,
            "error_kind": STR,
            "provider_job_id": STR,
            "cache_hit": _nullable(BOOL),
            "attempts": INT,
            "started_at": {"type": "string", "format": "date-time"},
        },
    },
    "DocumentMetrics": {
        "type": "object",
        "required": ["char_similarity", "cer", "wer", "word_f1"],
        "properties": {
            **{k: v for k, v in METRIC_PROPS.items() if k not in ("headline", "overall")},
            "word_recall": SCORE,
            "word_precision": SCORE,
            "pred_chars": INT,
            "truth_chars": INT,
            "rules_passed": INT,
            "rules_total": INT,
        },
    },
    "Manifest": {
        "type": "object",
        "description": "A dataset manifest, plus the display names the viewer build adds "
        "(`title`, `ordinal`, `source_label`, `category_label`) and page previews.",
        "required": ["name", "version", "documents"],
        "properties": {
            "name": STR,
            "version": STR,
            "description": STR,
            "license": STR,
            "generator": STR,
            "sources": {"type": "array", "items": _ref("DatasetSource")},
            "notes": STR_LIST,
            "documents": {"type": "array", "items": _ref("ManifestDocument")},
        },
    },
    "ManifestDocument": {
        "type": "object",
        "description": "Every path is relative to the manifest's own URL.",
        "required": ["id", "file", "truth", "pages", "category", "tags", "title"],
        "properties": {
            "id": STR,
            "file": {"type": "string", "description": "The input document."},
            "truth": {"type": "string", "description": "Ground-truth markdown; empty for `rules` documents."},
            "rules": {"type": "string", "description": "Machine-checkable assertions (`kind: rules`)."},
            "pages": INT,
            "category": STR,
            "tags": STR_LIST,
            "kind": STR,
            "source_id": STR,
            "sha256": STR,
            "license": STR,
            "attribution": STR,
            "preview": {"type": "string", "description": "Page-1 image."},
            "previews": STR_LIST,
            "title": STR,
            "ordinal": INT,
            "source_label": STR,
            "category_label": STR,
        },
    },
    "ParseResponse": {
        "type": "object",
        "description": "The unified PuffinParse parse response the model returned (docs/SPEC.md "
        "section 4). Blocks carry normalised (0..1) bounding boxes.",
        "required": ["id", "provider", "model", "pages", "markdown", "usage"],
        "properties": {
            "id": STR,
            "provider": STR,
            "model": STR,
            "provider_job_id": STR,
            "markdown": STR,
            "text": STR,
            "pages": {"type": "array", "items": _ref("Page")},
            "usage": {"type": "object", "required": ["pages"], "properties": {"pages": INT, "credits": NUM}},
            "cost_usd": _nullable(NUM),
            "latency_ms": INT,
            "created_at": {"type": "string", "format": "date-time"},
            "metadata": {"type": "object", "description": "Provider-specific extras."},
        },
    },
    "Page": {
        "type": "object",
        "required": ["page_number", "blocks"],
        "properties": {
            "page_number": INT,
            "markdown": STR,
            "text": STR,
            "width": NUM,
            "height": NUM,
            "blocks": {"type": "array", "items": _ref("Block")},
        },
    },
    "Block": {
        "type": "object",
        "required": ["type", "content", "page_number"],
        "properties": {
            "type": {
                "type": "string",
                "description": "`text`, `title`, `section_header`, `table`, `list`, ...",
            },
            "content": STR,
            "text": STR,
            "page_number": INT,
            "confidence": NUM,
            "bbox": {
                "type": "object",
                "required": ["x0", "y0", "x1", "y1"],
                "properties": {"x0": NUM, "y0": NUM, "x1": NUM, "y1": NUM},
            },
        },
    },
}

# ------------------------------------------------------------------------------------- the spec


def pick_examples(data_dir: Path) -> Optional[dict[str, str]]:
    """Real path-parameter values from a built ``data/`` directory: the newest run, its
    top-ranked model, a document that has both a markdown and a JSON output, and the run's
    dataset. ``None`` when the viewer was not built."""
    index_path = data_dir / "index.json"
    if not index_path.is_file():
        return None
    index = json.loads(index_path.read_text(encoding="utf-8"))
    for run in index.get("runs", []):
        dataset = str(run.get("dataset", {}).get("name", ""))
        if dataset not in index.get("datasets", {}):
            continue
        for model in run.get("models", []):
            inventory = run.get("outputs", {}).get(model["slug"], {})
            with_json = [d for d in inventory.get("json", []) if d not in inventory.get("missing", [])]
            if with_json:
                return {
                    "run_id": str(run["run_id"]),
                    "model_slug": str(model["slug"]),
                    "doc_id": sorted(with_json)[0],
                    "dataset": dataset,
                }
    return None


def _param(name: str, description: str, examples: Optional[dict[str, str]]) -> dict[str, Any]:
    param: dict[str, Any] = {
        "name": name,
        "in": "path",
        "required": True,
        "description": description,
        "schema": {"type": "string"},
    }
    if examples and name in examples:
        param["example"] = examples[name]
    return param


def _json_response(schema: str, description: str) -> dict[str, Any]:
    return {
        "200": {"description": description, "content": {"application/json": {"schema": _ref(schema)}}},
        "404": {"description": "No such file."},
    }


def openapi_spec(
    server: str,
    data_path: str,
    docs_url: str,
    examples: Optional[dict[str, str]] = None,
) -> dict[str, Any]:
    """The OpenAPI 3.1 document. ``server`` is the absolute site root (no trailing slash),
    ``data_path`` the data directory below it (``/benchmark-results/data``), ``docs_url`` the
    absolute URL of the data API docs page."""
    run_id = _param("run_id", "A `run_id` from `index.json` `runs[]`.", examples)
    slug = _param(
        "model_slug",
        "A model slug (`reducto_r-1`): `runs[].models[].slug`, i.e. the model string with `/` "
        "replaced by `_`.",
        examples,
    )
    doc = _param(
        "doc_id",
        "A document id from the run. Ids of combined datasets contain one literal `/` "
        "(`dpbench/01030000000016`): send it unescaped, it is a directory on the server.",
        examples,
    )
    dataset = _param("dataset", "A dataset name: a key of `index.json` `datasets`.", examples)
    tag_runs, tag_outputs, tag_datasets = "Runs", "Model outputs", "Datasets"
    return {
        "openapi": "3.1.0",
        "info": {
            "title": "PuffinParse benchmark data",
            "version": "1.0.0",
            "summary": "Read-only JSON for the open PuffinParse benchmark: leaderboard, runs, "
            "per-document scores, model outputs and datasets.",
            "description": "Static files published with the results viewer: plain GET, no "
            "authentication, no rate limit beyond the CDN, CORS open. Start at `index.json`; the "
            "newest run on the newest `combined-v*` dataset is the headline leaderboard. The data "
            "changes only when a new benchmark run is committed to the repository. Result JSON is "
            "MIT licensed; every dataset document carries its own licence (see the manifests).",
            "contact": {"name": "PuffinParse", "url": "https://github.com/ajinkyashejul/puffinparse/issues"},
        },
        "externalDocs": {"description": "Data API guide with curl examples", "url": docs_url},
        "servers": [{"url": server or "/"}],
        "tags": [
            {"name": tag_runs, "description": "The leaderboard and every scored run."},
            {"name": tag_outputs, "description": "What each model returned for each document."},
            {"name": tag_datasets, "description": "Inputs, ground truth and licences."},
        ],
        "paths": {
            f"{data_path}/index.json": {
                "get": {
                    "operationId": "getIndex",
                    "tags": [tag_runs],
                    "summary": "Leaderboard: every run with per-model summaries",
                    "description": "Each run's `models` are sorted by `summary.headline`, best first. "
                    "Prefer the newest run whose dataset is `combined-v*`.",
                    "responses": _json_response("Index", "The run index."),
                }
            },
            f"{data_path}/runs/{{run_id}}.json": {
                "get": {
                    "operationId": "getRun",
                    "tags": [tag_runs],
                    "summary": "One full run, with per-document scores",
                    "description": "`models[].docs[]` holds every document's metrics, latency and cost.",
                    "parameters": [run_id],
                    "responses": _json_response("Run", "The full result file."),
                }
            },
            f"{data_path}/outputs/{{run_id}}/{{model_slug}}/{{doc_id}}.md": {
                "get": {
                    "operationId": "getOutputMarkdown",
                    "tags": [tag_outputs],
                    "summary": "The markdown a model produced for a document (what was scored)",
                    "description": "Absent for documents listed in `outputs[slug].missing` "
                    "(failed calls and research-only sources that are never redistributed).",
                    "parameters": [run_id, slug, doc],
                    "responses": {
                        "200": {
                            "description": "Markdown.",
                            "content": {"text/markdown": {"schema": {"type": "string"}}},
                        },
                        "404": {"description": "No saved output."},
                    },
                }
            },
            f"{data_path}/outputs/{{run_id}}/{{model_slug}}/{{doc_id}}.json": {
                "get": {
                    "operationId": "getOutputResponse",
                    "tags": [tag_outputs],
                    "summary": "The unified ParseResponse (blocks and boxes) for a document",
                    "description": "Only for document ids listed in `outputs[slug].json`.",
                    "parameters": [run_id, slug, doc],
                    "responses": _json_response("ParseResponse", "The unified response."),
                }
            },
            f"{data_path}/datasets/{{dataset}}/manifest.json": {
                "get": {
                    "operationId": "getManifest",
                    "tags": [tag_datasets],
                    "summary": "A dataset manifest: documents, categories, licences",
                    "description": "`file`, `truth`, `rules` and `preview(s)` are relative to this "
                    "manifest's URL (combined datasets point at `../<source>/...`). Documents from "
                    "research-only sources are listed but their files are not published.",
                    "parameters": [dataset],
                    "responses": _json_response("Manifest", "The manifest."),
                }
            },
        },
        "components": {"schemas": SCHEMAS},
    }


def api_catalog(catalog_url: str, index_url: str, openapi_url: str, docs_url: str) -> dict[str, Any]:
    """The RFC 9727 API catalog: a Linkset (RFC 9264) listing the one API this site publishes,
    with its RFC 8631 service-desc (OpenAPI) and service-doc (the docs page)."""
    return {
        "linkset": [
            {
                "anchor": catalog_url,
                "item": [{"href": index_url, "title": "PuffinParse benchmark data"}],
            },
            {
                "anchor": index_url,
                "service-desc": [{"href": openapi_url, "type": OPENAPI_TYPE}],
                "service-doc": [{"href": docs_url, "type": "text/html"}],
            },
        ]
    }
