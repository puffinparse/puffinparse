/* PuffinParse benchmark results viewer.
 *
 * Vanilla ES2018, no framework, no build step (ADR: the viewer stays vanilla). Everything is
 * fetched from the `data/` directory written by benchmark/site/build.py, through `url()`; the
 * prefix comes from `<meta name="puffinparse-base">`. PDF pages are shown from the page images the
 * build renders; pdf.js (cdnjs, pinned + SRI) is loaded only for a page the build did not render.
 *
 * Design: docs/DESIGN.md. One primary number per view; everything secondary sits one click
 * away in a native <details>. Documents are named by their human title ("Headers & footers
 * 3"); the raw id stays in the URL and in Details.
 *
 * Routes (hash based, so every view is a shareable link and no rewrite rules are needed):
 *   #/leaderboard?run=<run_id>&sort=<col>&dir=asc|desc&x=cost|latency&cols=all
 *   #/documents?run=<run_id>&src=<source>&cat=<category>&q=<text>&sort=<col>&dir=asc|desc&pm=1
 *   #/<run_id>/<model_slug>/<doc_id…>?tab=rules|diff|output|truth|compare&diff=split|unified
 *                                      &ov=0|1&page=<n>&rf=fail|pass|all&src=…&cat=…&q=…
 *   #/doc/<doc_id>?run=…&model=…   (old links; redirected to the canonical form above)
 *
 * Review mode (ADR-28): `puffinparse eval view` serves this same file for a run over the user's
 * own documents, with the same data API built on the fly. Such a run has no ground truth, so it
 * carries a `review` object (agreement between models, outliers, verdicts) and the views show
 * agreement, failures and cost instead of scores, a diff against another model, and a verdict
 * form that posts to `api/review`. `<meta name="puffinparse-pdfjs">` points pdf.js at the copy
 * bundled in the CLI, so a local run fetches nothing from the network.
 */
(function () {
  "use strict";

  var REPO = "https://github.com/ajinkyashejul/puffinparse";
  var METHODOLOGY = REPO + "/blob/main/benchmark/README.md";
  var DIFF_CELL_CAP = 6000000; // LCS table cells we are willing to allocate
  var RULE_PAGE = 60; // check rows rendered per "show more"
  var BAG_SENTENCE_CAP = 12; // sentences listed per bag_of_sentences rule before "+N more"

  // pdf.js is the one external dependency: pinned, integrity-checked, loaded on demand.
  var PDFJS = {
    lib: "https://cdnjs.cloudflare.com/ajax/libs/pdf.js/3.11.174/pdf.min.js",
    libSri: "sha384-/1qUCSGwTur9vjf/z9lmu/eCUYbpOTgSjmpbMQZ1/CtX2v/WcAIKqRv+U1DUCG6e",
    worker: "https://cdnjs.cloudflare.com/ajax/libs/pdf.js/3.11.174/pdf.worker.min.js",
    workerSri: "sha384-SnzOobpRMLXZ52iJvZm/C0fYw0OQemTXzTjIsdsfMcrCtCEe9qgzxTd3RSklO5x2",
  };

  // Unified block types folded into the eight `--box-*` hues of tokens.css (fixed order).
  var BLOCK_GROUPS = [
    { key: "text", label: "Text", types: ["text"] },
    { key: "heading", label: "Title / heading", types: ["title", "section_header"] },
    { key: "table", label: "Table", types: ["table"] },
    { key: "figure", label: "Figure / caption", types: ["figure", "caption", "picture", "image"] },
    { key: "list", label: "List", types: ["list", "list_item"] },
    { key: "furniture", label: "Header / footer", types: ["header", "footer", "footnote", "page_number"] },
    { key: "formula", label: "Formula", types: ["formula", "equation"] },
    { key: "other", label: "Other", types: ["other"] },
  ];

  // Verdict thresholds (docs/DESIGN.md): good >= 90, fair 70-89.9, poor < 70.
  var GOOD = 90;
  var FAIR = 70;

  function meta(name, fallback) {
    var el = document.querySelector('meta[name="' + name + '"]');
    var value = el && el.getAttribute("content");
    return value || fallback;
  }

  var BASE = (function () {
    var value = meta("puffinparse-base", "./");
    return value.charAt(value.length - 1) === "/" ? value : value + "/";
  })();
  var HOME = meta("puffinparse-home", "");
  var DOCS = meta("puffinparse-docs", "");
  var MODE = meta("puffinparse-mode", "");
  var PDFJS_LOCAL = meta("puffinparse-pdfjs", "");
  var APP_NAME = MODE === "review" ? "PuffinParse run viewer" : "PuffinParse Benchmark";

  var INDEX = null;
  var runCache = {};
  var textCache = {};
  var jsonCache = {};
  var manifestCache = {};
  var lastScrollKey = null;
  var KEYS = {}; // key handlers of the current view
  var SV = null; // the persistent page viewer (kept across model / tab switches)

  var view = document.getElementById("view");

  function url(path) {
    return BASE + path;
  }

  /* ------------------------------------------------------------------ utils */

  function esc(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function isNum(value) {
    return typeof value === "number" && isFinite(value);
  }

  function fixed(value, digits) {
    return isNum(value) ? value.toFixed(digits) : "—";
  }

  function msFmt(value) {
    if (!isNum(value)) return "—";
    return value >= 1000 ? (value / 1000).toFixed(1) + " s" : Math.round(value) + " ms";
  }

  function moneyFmt(value) {
    return isNum(value) ? "$" + value.toFixed(2) : "—";
  }

  var MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

  /** "2026-09-24T21:10:06Z" -> "24 Sep 2026". */
  function dateFmt(iso) {
    var m = /^(\d{4})-(\d{2})-(\d{2})/.exec(String(iso || ""));
    return m ? Number(m[3]) + " " + MONTHS[Number(m[2]) - 1] + " " + m[1] : String(iso || "");
  }

  function slugOf(model) {
    return String(model).replace(/\//g, "_");
  }

  function enc(segment) {
    return encodeURIComponent(segment);
  }

  function encPath(id) {
    return String(id).split("/").map(enc).join("/");
  }

  /** Join and collapse `.`/`..` segments, so combined-dataset paths stay readable. */
  function joinPath(base, rel) {
    var parts = (String(base) + String(rel == null ? "" : rel)).split("/");
    var out = [];
    for (var i = 0; i < parts.length; i++) {
      var part = parts[i];
      if (part === ".") continue;
      if (part === ".." && out.length > 1 && out[out.length - 1] !== "..") {
        out.pop();
        continue;
      }
      out.push(part);
    }
    return out.join("/");
  }

  function pathLabel(rel) {
    return String(rel == null ? "" : rel).replace(/^(?:\.\.\/)+/, "");
  }

  function isRulesDoc(entry) {
    return !!entry && (entry.kind === "rules" || (!entry.truth && !!entry.rules));
  }

  function plural(n, one, many) {
    return n + " " + (n === 1 ? one : many || one + "s");
  }

  /* ------------------------------------------------------------ human names */

  function sentenceCase(raw) {
    var s = String(raw || "").replace(/[_-]+/g, " ").trim();
    return s ? s.charAt(0).toUpperCase() + s.slice(1) : "Uncategorised";
  }

  /** "olmocr" -> "olmOCR-bench", from the labels build.py writes into index.json. */
  function srcLabel(src) {
    var labels = (INDEX && INDEX.labels && INDEX.labels.sources) || {};
    return labels[src] || String(src || "");
  }

  function catLabel(cat) {
    var labels = (INDEX && INDEX.labels && INDEX.labels.categories) || {};
    return labels[cat] || sentenceCase(cat);
  }

  /** "Headers & footers 3" (build.py names every manifest document). */
  function docTitle(entry) {
    if (!entry) return "";
    return entry.title || (entry.category ? catLabel(entry.category) : entry.id);
  }

  /** "llamaparse/cost_effective" -> {p: "llamaparse", v: "cost_effective"}. */
  function modelParts(model) {
    var s = String(model);
    var cut = s.indexOf("/");
    return cut > 0 ? { p: s.slice(0, cut), v: s.slice(cut + 1) } : { p: "", v: s };
  }

  /* ------------------------------------------------------------ verdicts */

  function verdictOf(value) {
    if (!isNum(value)) return "";
    return value >= GOOD ? "good" : value >= FAIR ? "fair" : "poor";
  }

  /** The CSS-drawn verdict dot; the number next to it carries the value. */
  function vdot(value) {
    var v = verdictOf(value);
    return v ? '<span class="vd v-' + v + '" title="' + v + '" aria-hidden="true"></span>' : "";
  }

  function scoreHtml(value, digits) {
    return '<span class="score">' + vdot(value) + '<span class="n">' + esc(fixed(value, digits)) + "</span></span>";
  }

  function barHtml(value) {
    var w = isNum(value) ? Math.max(0, Math.min(100, value)) : 0;
    return '<span class="bar" aria-hidden="true"><i style="width:' + w.toFixed(1) + '%"></i></span>';
  }

  /* ------------------------------------------------------------ fetching */

  function fetchOk(target) {
    return fetch(target, { cache: "no-cache" }).then(function (resp) {
      if (!resp.ok) throw new Error("Could not load " + target + " (HTTP " + resp.status + ")");
      return resp;
    });
  }

  function getJSON(target) {
    if (jsonCache[target]) return jsonCache[target];
    var promise = fetchOk(target).then(function (resp) {
      return resp.json();
    });
    jsonCache[target] = promise;
    promise.catch(function () {
      delete jsonCache[target];
    });
    return promise;
  }

  function getText(target) {
    if (textCache[target]) return textCache[target];
    var promise = fetchOk(target).then(function (resp) {
      return resp.text();
    });
    textCache[target] = promise;
    promise.catch(function () {
      delete textCache[target];
    });
    return promise;
  }

  function getRun(runId) {
    if (runCache[runId]) return Promise.resolve(runCache[runId]);
    return getJSON(url("data/runs/" + enc(runId) + ".json")).then(function (data) {
      analyse(data);
      runCache[runId] = data;
      return data;
    });
  }

  function getManifest(name) {
    if (manifestCache[name]) return Promise.resolve(manifestCache[name]);
    return getJSON(url("data/datasets/" + enc(name) + "/manifest.json")).then(function (data) {
      manifestCache[name] = data;
      return data;
    });
  }

  /** Assertion file of a `kind: "rules"` document. null means "cannot show". */
  function getRules(rulesUrl) {
    return getJSON(rulesUrl).then(
      function (data) {
        return Array.isArray(data) ? data : (data && data.rules) || null;
      },
      function () {
        return null;
      }
    );
  }

  /* --------------------------------------------------------------- scoring */

  /** The number a model is ranked by: `summary.headline` when present, else `overall`. */
  function headlineOf(summary) {
    summary = summary || {};
    var head = summary.headline;
    var value = head;
    var label = null;
    var title = null;
    if (head && typeof head === "object") {
      value = [head.score, head.overall, head.value].filter(isNum)[0];
      label = head.label || head.name || head.metric || null;
      title = head.description || head.title || null;
    }
    if (isNum(value)) {
      if (value <= 1 && isNum(summary.overall) && summary.overall > 1) value *= 100;
      return { value: value, headline: true, label: label || "Score", title: title };
    }
    return { value: summary.overall, headline: false, label: "Score", title: null };
  }

  /** Per-document primary score on the 0–100 scale, as the scorer aggregates it. */
  function docScore(rec) {
    if (!rec) return null;
    if (rec.error) return 0;
    if (isNum(rec.headline)) return rec.headline <= 1 ? rec.headline * 100 : rec.headline;
    var m = rec.metrics || {};
    if (rec.table_only && isNum(m.table_score)) return m.table_score * 100;
    return isNum(m.char_similarity) ? m.char_similarity * 100 : null;
  }

  function scoreBasis(rec) {
    if (!rec) return "";
    var m = rec.metrics || {};
    if (isNum(m.rule_pass_rate)) return "share of checks that pass";
    if (rec.table_only && isNum(m.table_score)) return "table score (table-only document)";
    return "character similarity";
  }

  /** 0..1 -> "91" or "99.9": one decimal only where rounding would overstate the value. */
  function pct(fraction) {
    var v = fraction * 100;
    return Math.abs(v - Math.round(v)) < 0.05 ? String(Math.round(v)) : v.toFixed(1);
  }

  /** The one-line plain-English reading of a document score. */
  function verdictLine(rec, rulesTotal) {
    if (!rec) return "Not scored in this run";
    if (rec.error) return "The call failed";
    var m = rec.metrics || {};
    if (isNum(m.rule_pass_rate)) {
      var total = isNum(m.rules_total) ? m.rules_total : rulesTotal;
      var passed = isNum(m.rules_passed) ? m.rules_passed : isNum(total) ? Math.round(m.rule_pass_rate * total) : null;
      if (isNum(passed) && isNum(total)) return passed + " of " + plural(total, "check") + " pass";
      return fixed(m.rule_pass_rate * 100, 0) + "% of checks pass";
    }
    if (rec.table_only && isNum(m.table_score)) {
      return "Table " + pct(m.table_score) + "%" + (isNum(m.teds_grid) ? " · TEDS " + fixed(m.teds_grid, 2) : "");
    }
    if (isNum(m.char_similarity)) return fixed(m.char_similarity * 100, 1) + "% character match";
    return "";
  }

  function sourceOf(docId, fallback) {
    var cut = String(docId).indexOf("/");
    return cut > 0 ? String(docId).slice(0, cut) : fallback;
  }

  /** Nearest-rank percentile, identical to `percentile` in crates/puffinparse-cli/src/bench.rs. */
  function percentile(sorted, p) {
    if (!sorted.length) return null;
    var idx = Math.round((sorted.length - 1) * p);
    return sorted[Math.min(idx, sorted.length - 1)];
  }

  function bucket() {
    return { sum: 0, docs: 0, failed: 0 };
  }

  function addTo(target, score, rec) {
    target.docs += 1;
    if (rec.error) target.failed += 1;
    target.sum += isNum(score) ? score : 0;
  }

  /** Derived numbers the result file does not carry: p90, per-source and source×category. */
  function analyse(run) {
    var fallback = (run.dataset && run.dataset.name) || "dataset";
    var sources = [];
    var catsBySource = {};
    (run.models || []).forEach(function (model) {
      var lat = [];
      var bySource = {};
      var byCat = {};
      (model.docs || []).forEach(function (rec) {
        var src = sourceOf(rec.id, fallback);
        var cat = rec.category || "?";
        if (sources.indexOf(src) === -1) sources.push(src);
        catsBySource[src] = catsBySource[src] || [];
        if (catsBySource[src].indexOf(cat) === -1) catsBySource[src].push(cat);
        var score = docScore(rec);
        addTo((bySource[src] = bySource[src] || bucket()), score, rec);
        var key = src + "\u0000" + cat;
        addTo((byCat[key] = byCat[key] || bucket()), score, rec);
        if (!rec.error && isNum(rec.latency_ms)) lat.push(rec.latency_ms);
      });
      lat.sort(function (a, b) {
        return a - b;
      });
      var summary = model.summary || {};
      model.stats = {
        p50: isNum(summary.latency_p50_ms) ? summary.latency_p50_ms : percentile(lat, 0.5),
        p90: isNum(summary.latency_p90_ms) ? summary.latency_p90_ms : percentile(lat, 0.9),
        p95: isNum(summary.latency_p95_ms) ? summary.latency_p95_ms : percentile(lat, 0.95),
        head: headlineOf(summary),
        bySource: bySource,
        byCat: byCat,
      };
    });
    Object.keys(catsBySource).forEach(function (src) {
      catsBySource[src].sort(function (a, b) {
        return catLabel(a).localeCompare(catLabel(b));
      });
    });
    run.sources = sources.sort(function (a, b) {
      return srcLabel(a).localeCompare(srcLabel(b));
    });
    run.catsBySource = catsBySource;
    if (isReview(run)) {
      run.reviewByDoc = {};
      (run.review.per_doc || []).forEach(function (d) {
        run.reviewByDoc[d.id] = d;
      });
    }
  }

  function mean(b) {
    return b && b.docs ? b.sum / b.docs : null;
  }

  function isReview(run) {
    return !!(run && run.review);
  }

  /**
   * How a review run ranks its models, best first: the share of checked documents judged correct
   * (minor errors count half), then agreement with the other models, then successful calls.
   */
  function reviewRank(model) {
    var r = model.review || {};
    var t = r.reviews || {};
    return [
      t.reviewed ? (t.correct + 0.5 * t.minor_errors) / t.reviewed : -1,
      isNum(r.agreement) ? r.agreement : -1,
      r.ok || 0,
    ];
  }

  function byScore(models) {
    if (models.length && "review" in models[0]) {
      return models.slice().sort(function (a, b) {
        var x = reviewRank(a);
        var y = reviewRank(b);
        for (var i = 0; i < x.length; i++) if (x[i] !== y[i]) return y[i] - x[i];
        return 0;
      });
    }
    return models.slice().sort(function (a, b) {
      return (b.stats.head.value || 0) - (a.stats.head.value || 0);
    });
  }

  /* ----------------------------------------------------------------- router */

  function parseHash() {
    var raw = location.hash.replace(/^#/, "") || "/leaderboard";
    var split = raw.indexOf("?");
    var pathPart = split === -1 ? raw : raw.slice(0, split);
    var queryPart = split === -1 ? "" : raw.slice(split + 1);
    var parts = pathPart.split("/").filter(Boolean).map(function (p) {
      try {
        return decodeURIComponent(p);
      } catch (e) {
        return p;
      }
    });
    return { parts: parts, params: new URLSearchParams(queryPart) };
  }

  function query(values) {
    var q = new URLSearchParams();
    Object.keys(values || {}).forEach(function (key) {
      var value = values[key];
      if (value !== null && value !== undefined && value !== "") q.set(key, value);
    });
    var s = q.toString();
    return s ? "?" + s : "";
  }

  function listHref(path, runId, values) {
    var all = { run: runId };
    Object.keys(values || {}).forEach(function (k) {
      all[k] = values[k];
    });
    return "#/" + path + query(all);
  }

  function docHref(runId, model, docId, values) {
    return "#/" + enc(runId) + "/" + enc(slugOf(model)) + "/" + encPath(docId) + query(values);
  }

  function go(hash, replace) {
    if (replace) {
      history.replaceState(null, "", hash);
      render();
    } else {
      location.hash = hash;
    }
  }

  function findRun(runId) {
    var runs = (INDEX && INDEX.runs) || [];
    for (var i = 0; i < runs.length; i++) if (runs[i].run_id === runId) return runs[i];
    return null;
  }

  function resolveRun(params) {
    return findRun(params.get("run")) || (INDEX.runs || [])[0];
  }

  function setNav(active, runId) {
    ["leaderboard", "documents"].forEach(function (name) {
      var link = document.getElementById("nav-" + name);
      if (!link) return;
      link.setAttribute("href", listHref(name, runId));
      if (name === active) link.setAttribute("aria-current", "page");
      else link.removeAttribute("aria-current");
    });
  }

  function showError(err) {
    view.removeAttribute("aria-busy");
    view.innerHTML =
      '<div class="error"><h1>Something went wrong</h1><p>' +
      esc(err && err.message ? err.message : err) +
      "</p>" +
      (MODE === "review"
        ? '<p class="caption">This page is served by <code>puffinparse eval view</code>. If you stopped it, start it again ' +
          "for the run folder and open the new address it prints.</p></div>"
        : "") +
      (MODE === "review" ? "" : '<p class="caption">This page is static: it reads the JSON and markdown written by ' +
      "<code>benchmark/site/build.py</code>. Serve <code>dist/</code> over HTTP (for example " +
      "<code>python -m http.server -d benchmark/site/dist 8000</code>) rather than opening the " +
      "file directly.</p></div>");
  }

  /* ------------------------------------------------------- shared fragments */

  function runSelector(run) {
    var options = (INDEX.runs || [])
      .map(function (item) {
        var label = (item.dataset.name || "?") + " · " + dateFmt(item.created_at);
        return (
          '<option value="' + esc(item.run_id) + '"' + (item.run_id === run.run_id ? " selected" : "") +
          ' title="' + esc(item.run_id) + '">' + esc(label) + "</option>"
        );
      })
      .join("");
    return (
      '<label class="run-pick"><span class="sr-only">Run</span><select id="run-select" title="Benchmark run">' +
      options +
      "</select></label>"
    );
  }

  function wireRunSelect(root, path) {
    var select = root.querySelector("#run-select");
    if (!select) return;
    select.addEventListener("change", function () {
      go(listHref(path, select.value));
    });
  }

  function codeBlock(command) {
    return (
      '<div class="code-block"><button class="copy-btn" type="button" data-copy="' +
      esc(command) +
      '">Copy</button><pre><code>' +
      esc(command) +
      "</code></pre></div>"
    );
  }

  function disclosure(summary, body, opts) {
    opts = opts || {};
    return (
      '<details class="disclosure' + (opts.cls ? " " + opts.cls : "") + '"' + (opts.open ? " open" : "") +
      (opts.id ? ' id="' + esc(opts.id) + '"' : "") + "><summary>" + summary + "</summary>" +
      '<div class="disclosure-body">' + body + "</div></details>"
    );
  }

  function copyText(text, button, label) {
    var done = function () {
      button.textContent = "Copied";
      button.classList.add("done");
      setTimeout(function () {
        button.textContent = label;
        button.classList.remove("done");
      }, 1500);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () {
        button.textContent = "Press ⌘/Ctrl+C";
      });
      return;
    }
    var area = document.createElement("textarea");
    area.value = text;
    document.body.appendChild(area);
    area.select();
    try {
      document.execCommand("copy");
      done();
    } finally {
      document.body.removeChild(area);
    }
  }

  function wireCopyButtons(root) {
    root.querySelectorAll(".copy-btn").forEach(function (button) {
      button.addEventListener("click", function () {
        copyText(button.getAttribute("data-copy") || "", button, button.getAttribute("data-label") || "Copy");
      });
    });
  }

  /** Keep a disclosure's open state across re-renders of the same view (per session). */
  var OPEN = {};
  function wireDisclosures(root) {
    root.querySelectorAll("details[id]").forEach(function (d) {
      if (OPEN[d.id]) d.open = true;
      d.addEventListener("toggle", function () {
        OPEN[d.id] = d.open;
      });
    });
  }

  /**
   * Generic sortable table. `columns` describe cells; `rows` are opaque records. A column with
   * `best: "max" | "min"` bolds its best value (docs/DESIGN.md: best in bold, never a heatmap).
   */
  function sortableTable(options) {
    var columns = options.columns;
    var rows = options.rows.slice();
    var sortKey = options.sortKey;
    var sortDir = options.sortDir;
    var column = null;
    columns.forEach(function (candidate) {
      if (candidate.key === sortKey) column = candidate;
    });
    if (column && column.value) {
      var sign = sortDir === "asc" ? 1 : -1;
      rows.sort(function (a, b) {
        var av = column.value(a);
        var bv = column.value(b);
        if (av === null || av === undefined) return 1;
        if (bv === null || bv === undefined) return -1;
        if (typeof av === "string" || typeof bv === "string") {
          return String(av).localeCompare(String(bv)) * sign;
        }
        return (av - bv) * sign;
      });
    }
    var bests = {};
    columns.forEach(function (col) {
      if (!col.best || rows.length < 2) return;
      var scored = rows.filter(function (r) {
        return isNum(col.value(r));
      });
      if (!scored.length) return;
      var vals = scored.map(col.value);
      var best = col.best === "min" ? Math.min.apply(null, vals) : Math.max.apply(null, vals);
      var leader = scored[vals.indexOf(best)];
      // Bold only a unique best, judged on the value as displayed: two cells that both read
      // "100.0" are a tie even if the raw scores differ (docs/DESIGN.md, Table).
      var shown = function (r) {
        return String(col.cell(r).html).replace(/<[^>]*>/g, "");
      };
      var top = shown(leader);
      var ties = scored.filter(function (r) {
        return shown(r) === top;
      }).length;
      if (ties === 1) bests[col.key] = leader;
    });

    var head = columns
      .map(function (col) {
        var isSorted = col.key === sortKey;
        var cls = ((col.num ? "num " : "") + (col.headCls || "")).trim();
        var attrs =
          (cls ? ' class="' + cls + '"' : "") +
          (isSorted ? ' aria-sort="' + (sortDir === "asc" ? "ascending" : "descending") + '"' : "");
        var label = col.labelHtml || esc(col.label);
        if (!col.value) {
          return '<th scope="col"' + attrs + ">" + label + "</th>";
        }
        return (
          '<th scope="col"' + attrs + '><button type="button" class="sort-btn" data-sort="' + esc(col.key) + '"' +
          (col.asc ? ' data-asc="1"' : "") + ' title="' + esc(col.title || "Sort by " + col.label) + '">' +
          (col.num ? '<span class="sort-arrow" aria-hidden="true">' + (isSorted ? (sortDir === "asc" ? "↑" : "↓") : "") + "</span>" : "") +
          label +
          (col.num ? "" : '<span class="sort-arrow" aria-hidden="true">' + (isSorted ? (sortDir === "asc" ? "↑" : "↓") : "") + "</span>") +
          "</button></th>"
        );
      })
      .join("");

    var body = rows
      .map(function (row, position) {
        var cells = columns
          .map(function (col) {
            var cell = col.cell(row, position);
            var best = bests[col.key] === row;
            var cls = ((col.num ? "num " : "") + (col.cls || "") + " " + (cell.cls || "") + (best ? " best" : "")).trim();
            var tag = col.rowHeader ? "th" : "td";
            return (
              "<" + tag + (col.rowHeader ? ' scope="row"' : "") + (cls ? ' class="' + esc(cls) + '"' : "") +
              (cell.title ? ' title="' + esc(cell.title) + '"' : "") + ">" + cell.html + "</" + tag + ">"
            );
          })
          .join("");
        var rowCls = options.rowClass ? options.rowClass(row, position) : "";
        return "<tr" + (rowCls ? ' class="' + esc(rowCls) + '"' : "") + ">" + cells + "</tr>";
      })
      .join("");

    return (
      '<div class="table-wrap"><table' + (options.cls ? ' class="' + esc(options.cls) + '"' : "") +
      (options.id ? ' id="' + esc(options.id) + '"' : "") + ">" +
      (options.caption ? '<caption class="sr-only">' + options.caption + "</caption>" : "") +
      "<thead><tr>" + head + "</tr></thead><tbody>" +
      (body || '<tr><td colspan="' + columns.length + '" class="empty">No documents match.</td></tr>') +
      "</tbody></table></div>"
    );
  }

  function wireSorting(root, hrefFor) {
    root.querySelectorAll("button[data-sort]").forEach(function (button) {
      button.addEventListener("click", function () {
        var key = button.getAttribute("data-sort");
        var params = parseHash().params;
        var dir;
        if (params.get("sort") === key) dir = params.get("dir") === "asc" ? "desc" : "asc";
        else dir = button.getAttribute("data-asc") === "1" ? "asc" : "desc";
        go(hrefFor(key, dir), true);
      });
    });
  }

  /* ------------------------------------------------------------ leaderboard */

  var ASC_BY_DEFAULT = ["model", "cer", "wer", "p50", "p90", "p95", "per_page", "cost", "failed"];

  function leaderboardColumns(run, opts) {
    function sum(key) {
      return function (row) {
        return row.summary[key];
      };
    }
    function numCol(key, label, digits, title, fmt, getter, best) {
      var get = getter || sum(key);
      return {
        key: key,
        label: label,
        num: true,
        title: title,
        asc: ASC_BY_DEFAULT.indexOf(key) !== -1,
        best: best === undefined ? (ASC_BY_DEFAULT.indexOf(key) !== -1 ? "min" : "max") : best,
        value: get,
        cell: function (row) {
          var v = get(row);
          return { html: esc(fmt ? fmt(v) : fixed(v, digits)) };
        },
      };
    }
    var cols = [
      {
        key: "rank",
        label: "#",
        cell: function (row, position) {
          return { html: String(position + 1), cls: "rank" };
        },
      },
      {
        key: "model",
        label: "Model",
        asc: true,
        rowHeader: true,
        cls: "sticky-col",
        headCls: "sticky-col",
        value: function (row) {
          return row.model;
        },
        cell: function (row) {
          var first = (row.docs || [])[0];
          return {
            html:
              '<a class="model-id" href="' + esc(first ? docHref(run.run_id, row.model, first.id, {}) : "#") +
              '" title="Inspect ' + esc(row.model) + ' document by document">' + esc(row.model) + "</a>",
          };
        },
      },
      {
        key: "score",
        label: "Score",
        num: true,
        best: "max",
        title: opts.headTitle,
        value: function (row) {
          return row.stats.head.value;
        },
        cell: function (row) {
          var v = row.stats.head.value;
          var inner = '<span class="scorebar">' + scoreHtml(v, 1) + barHtml(v) + "</span>";
          return { html: row === opts.leader ? '<span class="bbox">' + inner + "</span>" : inner, cls: "score-cell" };
        },
      },
    ];
    if ((run.sources || []).length > 1) {
      run.sources.forEach(function (src) {
        var n = (run.models[0] && run.models[0].stats.bySource[src]) || { docs: 0 };
        cols.push({
          key: "src:" + src,
          label: srcLabel(src),
          labelHtml: esc(srcLabel(src)),
          num: true,
          best: "max",
          cls: "hide-narrow",
          headCls: "hide-narrow src-h",
          title: "Mean score on the " + n.docs + " " + srcLabel(src) + " documents. Each source is scored on its own ground truth; compare models within a column.",
          value: function (row) {
            return mean(row.stats.bySource[src]);
          },
          cell: function (row) {
            var b = row.stats.bySource[src];
            return { html: esc(fixed(mean(b), 1)), title: b && b.failed ? b.failed + " failed" : "" };
          },
        });
      });
    }
    cols.push(
      numCol("cost", "$/1k pages", 2, "Public list price per 1,000 pages", moneyFmt, sum("cost_per_1k_pages_usd")),
      numCol("p50", "p50", 0, "Median client-measured latency, caches disabled", msFmt, function (row) {
        return row.stats.p50;
      })
    );
    if (opts.anyFailed) {
      cols.push({
        key: "failed",
        // Empty outputs (a 200 with no text) are not failed calls; say which the column counts.
        label: opts.anyCallFailed ? (opts.anyEmpty ? "Failed / empty" : "Failed") : "Empty",
        num: true,
        asc: true,
        value: function (row) {
          return (row.summary.failed || 0) + (row.summary.empty_outputs || 0);
        },
        cell: function (row) {
          var failed = row.summary.failed || 0;
          var empty = row.summary.empty_outputs || 0;
          return {
            html: esc(failed + empty ? String(failed + empty) : "0"),
            cls: failed + empty ? "v-poor" : "muted",
            title: failed + " failed calls, " + empty + " empty outputs, of " + row.summary.documents,
          };
        },
      });
    }
    if (opts.all) {
      cols.push(
        numCol("char_similarity", "Char sim", 3, "Mean character similarity"),
        numCol("cer", "CER", 3, "Character error rate — lower is better"),
        numCol("wer", "WER", 3, "Word error rate — lower is better"),
        numCol("word_f1", "Word F1", 3),
        numCol("order_score", "Order", 3, "Reading-order agreement of shared lines"),
        numCol("table_score", "Table", 3, "Similarity restricted to table rows (markdown or HTML tables)"),
        numCol("teds_grid", "TEDS", 3, "TEDS on the row/cell grid: table structure plus cell content")
      );
      if (opts.rulesRun) {
        cols.push(
          numCol("rule_pass_rate", "Rules %", 1, "Mean pass rate over rule-scored documents", function (v) {
            return isNum(v) ? (v * 100).toFixed(1) : "—";
          })
        );
      }
      cols.push(
        numCol("p90", "p90", 0, "90th percentile latency (from the per-document latencies)", msFmt, function (row) {
          return row.stats.p90;
        }),
        numCol("p95", "p95", 0, "95th percentile latency", msFmt, function (row) {
          return row.stats.p95;
        }),
        numCol("per_page", "ms/page", 0, "Total latency / pages", null, sum("latency_per_page_ms"))
      );
    }
    return cols;
  }

  /** Categories as rows (grouped by source), models as columns: plain numbers, best bold. */
  function categoryTable(run, models) {
    var head = models
      .map(function (m) {
        var parts = modelParts(m.model);
        return (
          '<th scope="col" class="num"><span class="mhead" title="' + esc(m.model) + '"><span class="p">' + esc(parts.p) +
          '</span><span class="v">' + esc(parts.v) + "</span></span></th>"
        );
      })
      .join("");
    var body = (run.sources || [])
      .map(function (src) {
        var cats = run.catsBySource[src] || [];
        var group =
          (run.sources.length > 1
            ? '<tr><th scope="rowgroup" class="cat-group sticky-col" colspan="' + (models.length + 1) + '">' + esc(srcLabel(src)) + "</th></tr>"
            : "");
        return (
          group +
          cats
            .map(function (cat) {
              var key = src + "\u0000" + cat;
              var vals = models.map(function (m) {
                return mean(m.stats.byCat[key]);
              });
              var clean = vals.filter(isNum);
              var best = clean.length ? Math.max.apply(null, clean) : null;
              var distinct = clean.filter(function (v) {
                return fixed(v, 1) === fixed(best, 1);
              }).length === 1;
              var n = (models[0] && models[0].stats.byCat[key]) || { docs: 0 };
              return (
                '<tr><th scope="row" class="sticky-col"><a href="' + esc(listHref("documents", run.run_id, { src: src, cat: cat })) + '">' +
                esc(catLabel(cat)) + '</a> <span class="muted">' + n.docs + "</span></th>" +
                vals
                  .map(function (v) {
                    return '<td class="num' + (distinct && v === best ? " best" : "") + '">' + esc(fixed(v, 1)) + "</td>";
                  })
                  .join("") +
                "</tr>"
              );
            })
            .join("")
        );
      })
      .join("");
    return (
      '<div class="table-wrap"><table class="cat-table"><caption class="sr-only">Mean score per category and model</caption>' +
      '<thead><tr><th scope="col" class="sticky-col">Category</th>' + head + "</tr></thead><tbody>" + body + "</tbody></table></div>" +
      '<p class="caption">Mean per-document score. The number after a category is its document count; open a category to list its documents.</p>'
    );
  }

  /* ---------------------------------------------------------------- scatter */

  function niceTicks(lo, hi, count) {
    var span = hi - lo || 1;
    var step = Math.pow(10, Math.floor(Math.log10(span / count)));
    var err = span / count / step;
    if (err >= 7.5) step *= 10;
    else if (err >= 3.5) step *= 5;
    else if (err >= 1.5) step *= 2;
    var ticks = [];
    for (var v = Math.ceil(lo / step) * step; v <= hi + step * 1e-9; v += step) ticks.push(+v.toFixed(10));
    return ticks;
  }

  function boxesOverlap(a, b) {
    return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
  }

  function distToBox(box, x, y) {
    var dx = Math.max(box.x - x, 0, x - (box.x + box.w));
    var dy = Math.max(box.y - y, 0, y - (box.y + box.h));
    return Math.sqrt(dx * dx + dy * dy);
  }

  function scatterChart(run, models, xMode) {
    var xLabel = xMode === "latency" ? "p50 latency, seconds" : "List price, $ per 1,000 pages";
    var points = models
      .map(function (m) {
        var x = xMode === "latency" ? (isNum(m.stats.p50) ? m.stats.p50 / 1000 : null) : m.summary.cost_per_1k_pages_usd;
        return { model: m, x: x, y: m.stats.head.value };
      })
      .filter(function (p) {
        return isNum(p.x) && isNum(p.y);
      });
    if (!points.length) return '<p class="empty">No ' + (xMode === "latency" ? "latency" : "cost") + " data in this run.</p>";

    // Drawn at the width it will be shown at (up to 640), so labels stay 11px on a phone.
    var W = Math.round(Math.max(340, Math.min(640, view.clientWidth || 640)));
    var H = W < 500 ? 280 : 300;
    var M = { l: 40, r: 16, t: 12, b: 40 };
    var xMax = Math.max.apply(null, points.map(function (p) { return p.x; }));
    var yMin = Math.min.apply(null, points.map(function (p) { return p.y; }));
    var yMax = Math.max.apply(null, points.map(function (p) { return p.y; }));
    var xTicks = niceTicks(0, xMax * 1.08 || 1, 5);
    // The last tick must reach past the right-most point, or that point sits off the axis.
    if (xTicks.length > 1 && xTicks[xTicks.length - 1] < xMax) {
      xTicks.push(+(xTicks[xTicks.length - 1] + xTicks[1] - xTicks[0]).toFixed(10));
    }
    var x1 = xTicks[xTicks.length - 1] || 1;
    var y0 = Math.max(0, Math.floor((yMin - Math.max(2, (yMax - yMin) * 0.15)) / 5) * 5);
    var y1 = Math.min(100, Math.ceil((yMax + Math.max(1, (yMax - yMin) * 0.1)) / 5) * 5);
    if (y1 <= y0) y1 = y0 + 5;
    var yTicks = niceTicks(y0, y1, 4);
    function sx(v) {
      return M.l + (v / x1) * (W - M.l - M.r);
    }
    function sy(v) {
      return H - M.b - ((v - y0) / (y1 - y0)) * (H - M.t - M.b);
    }

    // Pareto frontier: no other model is both cheaper (or faster) and better.
    var sorted = points.slice().sort(function (a, b) {
      return a.x - b.x || b.y - a.y;
    });
    var best = -Infinity;
    sorted.forEach(function (p) {
      p.front = p.y > best;
      if (p.front) best = p.y;
    });
    var frontPath = sorted
      .filter(function (p) {
        return p.front;
      })
      .map(function (p, i) {
        return i === 0 ? "M" + sx(p.x) + "," + sy(p.y) : "H" + sx(p.x) + "V" + sy(p.y);
      })
      .join("");

    // Direct labels with simple collision avoidance: try positions around the dot, keep the
    // first that clears every dot and every label placed so far (higher scores place first).
    var CHAR = 6.7;
    var LH = 13;
    var dots = points.map(function (p) {
      return { x: sx(p.x) - 6, y: sy(p.y) - 6, w: 12, h: 12, p: p };
    });
    // The frontier's segments are obstacles too, so no label is struck through by the line.
    var placed = [];
    var frontPts = sorted.filter(function (p) {
      return p.front;
    });
    for (var f = 1; f < frontPts.length; f++) {
      var ax = sx(frontPts[f - 1].x);
      var ay = sy(frontPts[f - 1].y);
      var bx = sx(frontPts[f].x);
      var by = sy(frontPts[f].y);
      placed.push({ x: ax, y: ay - 1.5, w: bx - ax, h: 3 });
      placed.push({ x: bx - 1.5, y: by, w: 3, h: ay - by });
    }
    var labels = points
      .slice()
      .sort(function (a, b) {
        return b.y - a.y;
      })
      .map(function (p) {
        var cx = sx(p.x);
        var cy = sy(p.y);
        var w = String(p.model.model).length * CHAR;
        var candidates = [
          [cx + 10, cy - LH / 2, "start"],
          [cx - 10 - w, cy - LH / 2, "end"],
          [cx + 8, cy - LH - 7, "start"],
          [cx + 8, cy + 7, "start"],
          [cx - 8 - w, cy - LH - 7, "end"],
          [cx - 8 - w, cy + 7, "end"],
          [cx + 10, cy - LH / 2 - 15, "start"],
          [cx + 10, cy - LH / 2 + 15, "start"],
          [cx - 10 - w, cy - LH / 2 - 15, "end"],
          [cx - 10 - w, cy - LH / 2 + 15, "end"],
          [cx - w / 2, cy - LH - 10, "middle"],
          [cx - w / 2, cy + 10, "middle"],
        ];
        var choice = null;
        for (var i = 0; i < candidates.length && !choice; i++) {
          var c = candidates[i];
          var box = { x: c[0], y: c[1], w: w, h: LH };
          if (box.x < 0 || box.x + box.w > W || box.y < 0 || box.y + box.h > H - M.b + 4) continue;
          var clash = placed.some(function (o) {
            return boxesOverlap(box, o);
          }) || dots.some(function (d) {
            return d.p !== p && boxesOverlap(box, d);
          });
          // A label must sit nearer its own point than any other, or it names the wrong one.
          var own = distToBox(box, cx, cy);
          var misleading = dots.some(function (d) {
            return d.p !== p && distToBox(box, d.x + 6, d.y + 6) < own + 2;
          });
          if (!clash && !misleading) choice = { box: box, anchor: c[2] };
        }
        // No room anywhere (a narrow screen): leave the point unlabelled rather than overlap.
        if (!choice) return { p: p, cx: cx, cy: cy, tx: null };
        placed.push(choice.box);
        var tx = choice.anchor === "start" ? choice.box.x : choice.anchor === "end" ? choice.box.x + w : choice.box.x + w / 2;
        return { p: p, cx: cx, cy: cy, tx: tx, ty: choice.box.y + LH - 3, anchor: choice.anchor };
      });

    var grid =
      yTicks
        .map(function (t) {
          return (
            '<line class="grid" x1="' + M.l + '" x2="' + (W - M.r) + '" y1="' + sy(t) + '" y2="' + sy(t) + '"/>' +
            '<text class="tick" x="' + (M.l - 8) + '" y="' + (sy(t) + 4) + '" text-anchor="end">' + t + "</text>"
          );
        })
        .join("") +
      xTicks
        .map(function (t) {
          return (
            '<text class="tick" x="' + sx(t) + '" y="' + (H - M.b + 16) + '" text-anchor="middle">' +
            (xMode === "latency" ? t : "$" + t) + "</text>"
          );
        })
        .join("");

    var top = byScore(models)[0];
    var unlabelled = labels
      .filter(function (l) {
        return l.tx === null;
      })
      .map(function (l) {
        return l.p.model.model;
      });
    var marks = labels
      .map(function (l) {
        var p = l.p;
        var tip =
          p.model.model + " — score " + fixed(p.y, 1) + " · $" + fixed(p.model.summary.cost_per_1k_pages_usd, 2) +
          "/1k pages · p50 " + msFmt(p.model.stats.p50) + (p.front ? " · on the frontier" : "");
        return (
          '<g class="pt' + (p.front ? " front" : "") + (p.model === top ? " top" : "") + '" tabindex="0" role="img" aria-label="' +
          esc(tip) + '" data-tip="' + esc(tip) + '">' +
          '<circle class="hit" cx="' + l.cx + '" cy="' + l.cy + '" r="12"/>' +
          '<circle class="dot" cx="' + l.cx + '" cy="' + l.cy + '" r="4.5"/>' +
          (l.tx === null
            ? ""
            : '<text class="lbl" x="' + l.tx.toFixed(1) + '" y="' + l.ty.toFixed(1) + '" text-anchor="' + l.anchor + '">' + esc(p.model.model) + "</text>") +
          "</g>"
        );
      })
      .join("");

    return (
      '<div class="chart"><svg viewBox="0 0 ' + W + " " + H + '" role="group" aria-label="' +
      esc(xMode === "latency" ? "Score against median latency, one point per model" : "Score against list price, one point per model") + '">' +
      grid +
      '<line class="axis" x1="' + M.l + '" x2="' + (W - M.r) + '" y1="' + (H - M.b) + '" y2="' + (H - M.b) + '"/>' +
      '<path class="frontier" d="' + frontPath + '"/>' +
      marks +
      '<text class="axis-label" x="' + (M.l + (W - M.l - M.r) / 2) + '" y="' + (H - 6) + '" text-anchor="middle">' + esc(xLabel) + "</text>" +
      '<text class="axis-label" transform="translate(11 ' + (M.t + (H - M.t - M.b) / 2) + ') rotate(-90)" text-anchor="middle">Score</text>' +
      '</svg><div class="tip" role="status" hidden></div></div>' +
      (unlabelled.length ? '<p class="caption">Unlabelled for space: ' + esc(unlabelled.join(", ")) + ". Hover or tap a point for its details.</p>" : "")
    );
  }

  function wireChart(root) {
    var chart = root.querySelector(".chart");
    if (!chart) return;
    var tip = chart.querySelector(".tip");
    function show(g) {
      var box = chart.getBoundingClientRect();
      var dot = g.querySelector(".dot").getBoundingClientRect();
      tip.textContent = g.getAttribute("data-tip");
      tip.hidden = false;
      var left = dot.left - box.left + dot.width / 2;
      tip.style.left = Math.max(8, Math.min(left, box.width - tip.offsetWidth - 8)) + "px";
      tip.style.top = Math.max(0, dot.top - box.top - tip.offsetHeight - 10) + "px";
    }
    function hide() {
      tip.hidden = true;
    }
    chart.querySelectorAll(".pt").forEach(function (g) {
      g.addEventListener("mouseenter", function () {
        show(g);
      });
      g.addEventListener("focus", function () {
        show(g);
      });
      g.addEventListener("mouseleave", hide);
      g.addEventListener("blur", hide);
    });
  }

  function methodologyHtml(run, datasetInfo) {
    return (
      '<div class="prose"><p>Scores are deterministic text comparisons, no LLM judge. Predictions and ground truth are ' +
      "normalised (NFKC, markdown syntax stripped, quotes and dashes straightened, whitespace collapsed" +
      (run.normalize && run.normalize.case_insensitive ? ", lowercased" : "") +
      "), then compared:</p><ul>" +
      "<li><strong>Score</strong> is the mean per-document score, 0–100: character similarity " +
      "<code>1 − levenshtein / max(len)</code>; the table score on a table-only document; the share of checks that pass on a rules document. A failed call scores 0.</li>" +
      "<li><strong>CER / WER</strong> are edit rates over characters and whitespace tokens. <strong>Order</strong> is Kendall-τ-style agreement on the order of lines present in both texts.</li>" +
      "<li><strong>Table</strong> is character similarity restricted to table rows (markdown or HTML); <strong>TEDS</strong> is tree-edit-distance similarity on the row/cell grid.</li>" +
      "<li><strong>Checks</strong> follow each source benchmark's own assertions (present, absent, order, table cell, sentences), ignoring spaces next to punctuation and honouring upstream edit tolerances.</li>" +
      "<li><strong>Latency</strong> is client-measured through each provider's public API: it includes upload, queueing and polling, and every run is made with provider result caches disabled. <strong>$/1k pages</strong> uses public pay-as-you-go list prices.</li></ul>" +
      "<p>" +
      (datasetInfo && (datasetInfo.sources || (datasetInfo.kinds || {}).rules)
        ? "Each document keeps the ground truth its own source ships: an exact transcript, or machine-checkable assertions for pages that have no reference transcript. "
        : "Ground truth is exact by construction: the documents are rendered from the same source the truth markdown is written from. ") +
      "Sources are scored on their own ground truth, so compare models within a source rather than across sources.</p>" +
      (datasetInfo && datasetInfo.license ? "<p>Dataset licence: " + esc(String(datasetInfo.license).replace(/`/g, "")) + ".</p>" : "") +
      '<p>Full definitions and caveats: <a href="' + METHODOLOGY + '" rel="noopener noreferrer">benchmark/README.md</a>.</p></div>'
    );
  }

  function runDetailsHtml(run, runInfo) {
    var normalize = run.normalize || {};
    var flags = [];
    if (normalize.case_insensitive) flags.push("case-insensitive");
    if (normalize.strip_markdown) flags.push("markdown stripped");
    if (normalize.strip_punctuation) flags.push("punctuation stripped");
    return (
      '<dl class="kv">' +
      "<dt>Run</dt><dd class=\"mono\">" + esc(run.run_id) + "</dd>" +
      "<dt>Started</dt><dd>" + esc(run.created_at) + (run.rescored_at ? " · re-scored " + esc(run.rescored_at) : "") + "</dd>" +
      "<dt>Dataset</dt><dd>" + esc(run.dataset.name) + " v" + esc(run.dataset.version) + " · " + esc(run.dataset.documents) + " documents</dd>" +
      (run.dataset.sha256 ? '<dt>SHA-256</dt><dd class="mono" title="Of the manifest, every input, truth and rule file">' + esc(run.dataset.sha256) + "</dd>" : "") +
      "<dt>Normalisation</dt><dd>" + esc(flags.join(", ") || "none") + "</dd>" +
      "<dt>Versions</dt><dd>PuffinParse " + esc(run.puffinparse_version) + (run.scorer_version ? " · scorer " + esc(run.scorer_version) : "") + "</dd>" +
      '<dt>Result file</dt><dd><a href="' + esc(url(runInfo.file)) + '" rel="noopener">' + esc(runInfo.file) + "</a></dd>" +
      "</dl>"
    );
  }

  function viewLeaderboard(runInfo, params) {
    document.title = (MODE === "review" ? "Models · " : "Leaderboard · ") + APP_NAME;
    setNav("leaderboard", runInfo.run_id);
    return getRun(runInfo.run_id).then(function (run) {
      if (isReview(run)) return reviewModels(runInfo, run, params);
      var sortKey = params.get("sort") || "score";
      var sortDir = params.get("dir") || (ASC_BY_DEFAULT.indexOf(sortKey) !== -1 ? "asc" : "desc");
      var xMode = params.get("x") === "latency" ? "latency" : "cost";
      var all = params.get("cols") === "all";
      var rulesRun = (run.models || []).some(function (m) {
        return isNum((m.summary || {}).rule_pass_rate);
      });
      var anyFailed = (run.models || []).some(function (m) {
        return (m.summary.failed || 0) + (m.summary.empty_outputs || 0) > 0;
      });
      var anyCallFailed = (run.models || []).some(function (m) {
        return (m.summary.failed || 0) > 0;
      });
      var anyEmpty = (run.models || []).some(function (m) {
        return (m.summary.empty_outputs || 0) > 0;
      });
      var ordered = byScore(run.models);
      var leader = ordered[0];
      var head0 = leader ? leader.stats.head : {};
      var headTitle =
        head0.title ||
        "Mean per-document score, 0–100: character similarity; table score on table-only documents; share of checks passed on rule documents. A failed call scores 0.";
      var datasetInfo = (INDEX.datasets || {})[run.dataset.name];
      var sources = run.sources || [];

      var reproduce =
        "puffinparse bench run \\\n    --dataset benchmark/datasets/" + run.dataset.name +
        " \\\n    --models " + run.models.map(function (m) { return m.model; }).join(" ") +
        " \\\n    --concurrency 4 --save-outputs benchmark/results/outputs/<run_id>";

      var summary =
        esc(run.dataset.name) + " · " + esc(run.dataset.documents) + " documents" +
        (sources.length > 1 ? " from " + sources.length + " benchmarks" : "") +
        " · " + plural(run.models.length, "model") + " · " + esc(dateFmt(run.created_at));

      view.innerHTML =
        '<header class="page-head"><div><h1>Leaderboard</h1><p class="lede">' + summary + "</p></div>" + runSelector(runInfo) + "</header>" +
        '<div class="table-tools"><p class="caption">Ranked by score, the mean per-document score out of 100. Open a model to inspect it document by document.</p>' +
        '<button type="button" class="toggle" id="all-metrics" aria-pressed="' + all + '">All metrics</button></div>' +
        sortableTable({
          id: "leaderboard",
          cls: "lb-table",
          columns: leaderboardColumns(run, { rulesRun: rulesRun, anyFailed: anyFailed, anyCallFailed: anyCallFailed, anyEmpty: anyEmpty, all: all, leader: leader, headTitle: headTitle }),
          rows: run.models,
          sortKey: sortKey,
          sortDir: sortDir,
          caption: "Models ranked by score",
          rowClass: function (row) {
            return row === leader ? "leader" : "";
          },
        }) +
        '<section class="section"><div class="section-head"><h2>Score vs ' + (xMode === "latency" ? "latency" : "cost") + "</h2>" +
        '<div class="seg" role="group" aria-label="X axis">' +
        '<button type="button" data-x="cost" aria-pressed="' + (xMode === "cost") + '">Cost</button>' +
        '<button type="button" data-x="latency" aria-pressed="' + (xMode === "latency") + '">Latency</button>' +
        "</div></div>" +
        scatterChart(run, ordered, xMode) +
        '<p class="caption">Up and to the left is better. The dashed line joins the models no other model beats on both axes.</p></section>' +
        '<div class="bottom-disclosures">' +
        disclosure("By category <span class=\"count\">" + Object.keys(ordered[0] ? ordered[0].stats.byCat : {}).length + "</span>", categoryTable(run, ordered), { id: "lb-cat" }) +
        disclosure("Methodology", methodologyHtml(run, datasetInfo), { id: "lb-method" }) +
        disclosure(
          "Reproduce this run",
          '<p class="note">Install the CLI, set the provider keys you want to test, then run:</p>' + codeBlock(reproduce) +
            '<p class="caption">Score one prediction offline with <code>puffinparse bench score prediction.md truth.md</code>. Every document page has the exact commands for that document.</p>',
          { id: "lb-repro" }
        ) +
        disclosure("Run details", runDetailsHtml(run, runInfo), { id: "lb-run" }) +
        "</div>";

      wireSorting(view, function (key, dir) {
        return listHref("leaderboard", run.run_id, { sort: key, dir: dir, x: params.get("x"), cols: params.get("cols") });
      });
      wireCopyButtons(view);
      wireRunSelect(view, "leaderboard");
      wireChart(view);
      wireDisclosures(view);
      view.querySelector("#all-metrics").addEventListener("click", function () {
        go(listHref("leaderboard", run.run_id, { sort: params.get("sort"), dir: params.get("dir"), x: params.get("x"), cols: all ? null : "all" }), true);
      });
      view.querySelectorAll("button[data-x]").forEach(function (b) {
        b.addEventListener("click", function () {
          go(listHref("leaderboard", run.run_id, { sort: params.get("sort"), dir: params.get("dir"), cols: params.get("cols"), x: b.getAttribute("data-x") }), true);
        });
      });
      KEYS = {};
      return "leaderboard|" + run.run_id;
    });
  }

  /* -------------------------------------------------------------- documents */

  /** The documents of `manifest` that `run` scored, filtered by src/cat/q. */
  function docList(run, manifest, params) {
    var src = params.get("src") || "";
    var cat = params.get("cat") || "";
    var q = (params.get("q") || "").toLowerCase();
    var scored = {};
    ((run.models[0] || {}).docs || []).forEach(function (d) {
      scored[d.id] = true;
    });
    return (manifest.documents || []).filter(function (doc) {
      if (!scored[doc.id]) return false;
      if (src && sourceOf(doc.id, run.dataset.name) !== src) return false;
      if (cat && doc.category !== cat) return false;
      if (q) {
        var hay = [doc.id, docTitle(doc), doc.category, doc.category_label, doc.source_label, (doc.tags || []).join(" ")].join(" ").toLowerCase();
        if (hay.indexOf(q) === -1) return false;
      }
      return true;
    });
  }

  function recordsByDoc(run) {
    var byDoc = {};
    run.models.forEach(function (model) {
      (model.docs || []).forEach(function (doc) {
        (byDoc[doc.id] = byDoc[doc.id] || {})[model.model] = doc;
      });
    });
    return byDoc;
  }

  /** Sort key that orders documents by source, category and ordinal. */
  function titleKey(doc, run) {
    var n = String(doc.ordinal || 0);
    while (n.length < 5) n = "0" + n;
    return [doc.source_label || srcLabel(sourceOf(doc.id, run.dataset.name)), doc.category_label || catLabel(doc.category), n, doc.id].join("\u0000");
  }

  function viewDocuments(runInfo, params) {
    document.title = "Documents · " + APP_NAME;
    setNav("documents", runInfo.run_id);
    return Promise.all([getRun(runInfo.run_id), getManifest(runInfo.dataset.name)]).then(function (loaded) {
      var run = loaded[0];
      var manifest = loaded[1];
      if (isReview(run)) return reviewDocuments(runInfo, run, manifest, params);
      var src = params.get("src") || "";
      var cat = params.get("cat") || "";
      var perModel = params.get("pm") === "1";
      var models = byScore(run.models);
      var byDoc = recordsByDoc(run);
      var filters = { src: src, cat: cat, q: params.get("q") };
      var sources = run.sources || [];
      var multiSource = sources.length > 1;

      var rows = docList(run, manifest, params).map(function (doc) {
        var scores = {};
        var sum = 0;
        var count = 0;
        var top = null;
        var ties = 0;
        models.forEach(function (model) {
          var rec = byDoc[doc.id] && byDoc[doc.id][model.model];
          var value = docScore(rec);
          scores[model.model] = { value: value, error: rec && rec.error };
          if (isNum(value)) {
            sum += value;
            count += 1;
            if (top === null || value > top.value + 1e-9) {
              top = { model: model.model, value: value };
              ties = 1;
            } else if (Math.abs(value - top.value) <= 1e-9) ties += 1;
          }
        });
        return { doc: doc, scores: scores, mean: count ? sum / count : null, top: top, ties: ties };
      });

      var columns = [
        {
          key: "id",
          label: "Document",
          asc: true,
          rowHeader: true,
          cls: "doc-cell" + (perModel ? " sticky-col" : ""),
          headCls: perModel ? "sticky-col" : "",
          value: function (row) {
            return titleKey(row.doc, run);
          },
          cell: function (row) {
            return {
              html:
                '<a href="' + esc(docHref(run.run_id, models[0].model, row.doc.id, filters)) + '">' + esc(docTitle(row.doc)) + "</a>" +
                (multiSource && !src ? '<span class="src">' + esc(row.doc.source_label || srcLabel(sourceOf(row.doc.id, run.dataset.name))) + "</span>" : ""),
              title: row.doc.id,
            };
          },
        },
        {
          key: "mean",
          label: "Mean score",
          num: true,
          title: "Mean score across every model in this run",
          value: function (row) {
            return row.mean;
          },
          cell: function (row) {
            return { html: '<span class="scorebar">' + scoreHtml(row.mean, 1) + barHtml(row.mean) + "</span>" };
          },
        },
      ];
      if (!perModel) {
        columns.push({
          key: "best",
          label: "Best model",
          cls: "best-cell hide-narrow",
          headCls: "hide-narrow",
          cell: function (row) {
            if (!row.top) return { html: "—" };
            if (row.ties === models.length && models.length > 1) return { html: '<span class="muted">all ' + row.ties + " tied</span>" };
            return {
              html:
                '<a class="model-id" href="' + esc(docHref(run.run_id, row.top.model, row.doc.id, filters)) + '">' + esc(row.top.model) + "</a>" +
                (row.ties > 1 ? ' <span class="muted">+' + (row.ties - 1) + "</span>" : ""),
              title: row.ties > 1 ? row.ties + " models share the best score" : "",
            };
          },
        });
      } else {
        models.forEach(function (model) {
          var parts = modelParts(model.model);
          columns.push({
            key: "m:" + model.model,
            label: model.model,
            labelHtml: '<span class="mhead"><span class="p">' + esc(parts.p) + '</span><span class="v">' + esc(parts.v) + "</span></span>",
            num: true,
            title: "Score of " + model.model + " on each document",
            value: function (row) {
              return row.scores[model.model].value;
            },
            cell: function (row) {
              var entry = row.scores[model.model];
              if (entry.error) return { html: "failed", cls: "low", title: entry.error };
              if (!isNum(entry.value)) return { html: "—", cls: "muted" };
              return {
                html: '<a href="' + esc(docHref(run.run_id, model.model, row.doc.id, filters)) + '">' + esc(fixed(entry.value, 1)) + "</a>",
                cls: "pm-cell" + (entry.value < FAIR ? " low" : ""),
              };
            },
          });
        });
      }

      var cats = src
        ? run.catsBySource[src] || []
        : Object.keys(
            sources.reduce(function (acc, s) {
              (run.catsBySource[s] || []).forEach(function (c) {
                acc[c] = 1;
              });
              return acc;
            }, {})
          ).sort(function (a, b) {
            return catLabel(a).localeCompare(catLabel(b));
          });

      function options(list, current, allLabel, labelOf) {
        return (
          '<option value="">' + esc(allLabel) + "</option>" +
          list
            .map(function (name) {
              return '<option value="' + esc(name) + '"' + (name === current ? " selected" : "") + ">" + esc(labelOf(name)) + "</option>";
            })
            .join("")
        );
      }

      var sortKey = params.get("sort") || "id";
      var sortDir = params.get("dir") || (sortKey === "id" ? "asc" : "desc");
      var total = ((run.models[0] || {}).docs || []).length;

      view.innerHTML =
        '<header class="page-head"><div><h1>Documents</h1><p class="lede">' +
        (rows.length === total ? plural(total, "document") : rows.length + " of " + plural(total, "document")) +
        " · open one to see the page, each model's output and why it scored what it did</p></div></header>" +
        '<div class="filters" role="search">' +
        runSelector(runInfo) +
        (multiSource
          ? '<label><span class="sr-only">Source</span><select id="src-select">' + options(sources, src, "All sources", srcLabel) + "</select></label>"
          : "") +
        '<label><span class="sr-only">Category</span><select id="cat-select">' + options(cats, cat, "All categories", catLabel) + "</select></label>" +
        '<label class="grow"><span class="sr-only">Filter</span><input id="doc-search" type="search" placeholder="Filter" value="' +
        esc(params.get("q") || "") + '" /></label>' +
        '<span class="spacer"></span>' +
        '<button type="button" class="toggle" id="per-model" aria-pressed="' + perModel + '">Per-model scores</button>' +
        "</div>" +
        sortableTable({
          cls: "docs-table",
          columns: columns,
          rows: rows,
          sortKey: sortKey,
          sortDir: sortDir,
          caption: "Documents with their mean score across models",
        });

      function refilter(values) {
        var next = { src: src, cat: cat, q: params.get("q"), sort: params.get("sort"), dir: params.get("dir"), pm: params.get("pm") };
        Object.keys(values).forEach(function (k) {
          next[k] = values[k];
        });
        go(listHref("documents", run.run_id, next), true);
      }
      wireSorting(view, function (key, dir) {
        return listHref("documents", run.run_id, { src: src, cat: cat, q: params.get("q"), sort: key, dir: dir, pm: params.get("pm") });
      });
      wireRunSelect(view, "documents");
      var srcSelect = view.querySelector("#src-select");
      if (srcSelect)
        srcSelect.addEventListener("change", function () {
          refilter({ src: srcSelect.value, cat: "" });
        });
      var catSelect = view.querySelector("#cat-select");
      catSelect.addEventListener("change", function () {
        refilter({ cat: catSelect.value });
      });
      view.querySelector("#per-model").addEventListener("click", function () {
        refilter({ pm: perModel ? null : "1" });
      });
      var search = view.querySelector("#doc-search");
      var timer = null;
      search.addEventListener("input", function () {
        clearTimeout(timer);
        timer = setTimeout(function () {
          refilter({ q: search.value });
          var again = document.getElementById("doc-search");
          if (again) {
            again.focus();
            again.setSelectionRange(again.value.length, again.value.length);
          }
        }, 220);
      });
      KEYS = {};
      return "documents|" + run.run_id;
    });
  }

  /* ---------------------------------------------------------- word-level diff */

  function tokenize(text) {
    var tokens = [];
    var re = /\S+/g;
    var match;
    var last = 0;
    while ((match = re.exec(text)) !== null) {
      tokens.push({ word: match[0], pre: text.slice(last, match.index) });
      last = match.index + match[0].length;
    }
    return { tokens: tokens, tail: text.slice(last) };
  }

  /**
   * Comparison key: mirrors the benchmark's normalisation closely enough to be fair. Inline
   * HTML tags are dropped (the scorer strips them), and a token that is only markdown syntax
   * (`|`, `---`, `|-|-|`, `#`) gets the empty key: it is shown but never counted as a change.
   */
  function tokenKey(word) {
    var key = word.replace(/<\/?[A-Za-z][^>]*>/g, "").toLowerCase();
    if (/^[|:\-#*_>`~+]*$/.test(key)) return "";
    if (key.normalize) key = key.normalize("NFKC");
    key = key.replace(/[‘’]/g, "'").replace(/[“”]/g, '"').replace(/[–—]/g, "-");
    var stripped = key.replace(/^[#*_>`~|+-]+/, "").replace(/[#*_`~|]+$/, "");
    return stripped || key;
  }

  /** Ordered edit script between two key arrays. Returns [{t:'eq'|'del'|'ins', ai, bi}]. */
  function diffKeys(a, b) {
    var n = a.length;
    var m = b.length;
    var start = 0;
    while (start < n && start < m && a[start] === b[start]) start++;
    var end = 0;
    while (end < n - start && end < m - start && a[n - 1 - end] === b[m - 1 - end]) end++;
    var ops = [];
    var i;
    for (i = 0; i < start; i++) ops.push({ t: "eq", ai: i, bi: i });
    var an = n - end - start;
    var bn = m - end - start;
    if (an > 0 || bn > 0) {
      if ((an + 1) * (bn + 1) > DIFF_CELL_CAP) {
        for (i = 0; i < an; i++) ops.push({ t: "del", ai: start + i });
        for (i = 0; i < bn; i++) ops.push({ t: "ins", bi: start + i });
      } else {
        var width = bn + 1;
        var lcs = new Int32Array((an + 1) * width);
        var x, y;
        for (x = an - 1; x >= 0; x--) {
          for (y = bn - 1; y >= 0; y--) {
            if (a[start + x] === b[start + y]) lcs[x * width + y] = lcs[(x + 1) * width + y + 1] + 1;
            else {
              var down = lcs[(x + 1) * width + y];
              var right = lcs[x * width + y + 1];
              lcs[x * width + y] = down >= right ? down : right;
            }
          }
        }
        x = 0;
        y = 0;
        while (x < an && y < bn) {
          if (a[start + x] === b[start + y]) {
            ops.push({ t: "eq", ai: start + x, bi: start + y });
            x++;
            y++;
          } else if (lcs[(x + 1) * width + y] >= lcs[x * width + y + 1]) {
            ops.push({ t: "del", ai: start + x });
            x++;
          } else {
            ops.push({ t: "ins", bi: start + y });
            y++;
          }
        }
        for (; x < an; x++) ops.push({ t: "del", ai: start + x });
        for (; y < bn; y++) ops.push({ t: "ins", bi: start + y });
      }
    }
    for (i = end - 1; i >= 0; i--) ops.push({ t: "eq", ai: n - 1 - i, bi: m - 1 - i });
    return ops;
  }

  /** Keys of the significant tokens, plus a map back to the token index. */
  function significant(parsed) {
    var keys = [];
    var at = [];
    parsed.tokens.forEach(function (tok, i) {
      var key = tokenKey(tok.word);
      if (key) {
        keys.push(key);
        at.push(i);
      }
    });
    return { keys: keys, at: at };
  }

  function wordDiff(truth, pred) {
    var t = tokenize(truth);
    var p = tokenize(pred);
    var ts = significant(t);
    var ps = significant(p);
    var ops = diffKeys(ts.keys, ps.keys).map(function (op) {
      return { t: op.t, ai: op.ai === undefined ? undefined : ts.at[op.ai], bi: op.bi === undefined ? undefined : ps.at[op.bi] };
    });
    var res = { t: t, p: p, ops: ops, eq: 0, del: 0, ins: 0, tMarks: [], pMarks: [] };
    ops.forEach(function (op) {
      if (op.t === "eq") res.eq++;
      else if (op.t === "del") {
        res.del++;
        res.tMarks[op.ai] = true;
      } else {
        res.ins++;
        res.pMarks[op.bi] = true;
      }
    });
    return res;
  }

  function renderToken(token, tag) {
    return esc(token.pre) + (tag ? "<" + tag + ">" + esc(token.word) + "</" + tag + ">" : esc(token.word));
  }

  function renderSide(parsed, marks, tag) {
    var out = "";
    for (var i = 0; i < parsed.tokens.length; i++) out += renderToken(parsed.tokens[i], marks[i] ? tag : null);
    return out + esc(parsed.tail);
  }

  function renderUnified(d) {
    var out = "";
    var nextPred = 0; // prediction tokens are emitted in order, syntax-only ones included
    function flushTo(bi) {
      for (; nextPred <= bi; nextPred++) out += renderToken(d.p.tokens[nextPred], d.pMarks[nextPred] ? "ins" : null);
    }
    d.ops.forEach(function (op) {
      if (op.t === "del") out += renderToken(d.t.tokens[op.ai], "del");
      else flushTo(op.bi);
    });
    flushTo(d.p.tokens.length - 1);
    return out + esc(d.p.tail);
  }

  /* ------------------------------------------- rule checker (mirror of bench.rs) */
  // A line-for-line port of `normalize`, `markdown_to_text` and `score_rules` in
  // crates/puffinparse-core, so every assertion can be shown passing or failing. The recorded
  // `rules_passed` from the Rust scorer stays authoritative; the viewer says when they differ.

  /** `INLINE_TAGS` in types.rs: removing these must not split a word (`9<sup>th</sup>`). */
  var INLINE_TAGS = [
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "del", "dfn", "em", "font", "i", "ins", "kbd", "mark",
    "q", "s", "samp", "small", "span", "strike", "strong", "sub", "sup", "time", "tt", "u", "var",
  ];

  function stripHtmlTags(s) {
    var out = "";
    for (var i = 0; i < s.length; i++) {
      var c = s[i];
      if (c === "<" && /[A-Za-z\/]/.test(s[i + 1] || "")) {
        var close = s.indexOf(">", i + 1);
        if (close !== -1) {
          var name = (/^\/*([A-Za-z0-9]*)/.exec(s.slice(i + 1, close)) || ["", ""])[1].toLowerCase();
          if (INLINE_TAGS.indexOf(name) === -1) out += " ";
          i = close;
          continue;
        }
        // Unterminated tag: bench.rs has consumed the rest, keeping only the `<`.
        out += "<";
        break;
      }
      out += c;
    }
    return out;
  }

  /** `strip_single_emphasis` in types.rs: `*x*` / `_x_` delimiters go, ` * ` and `a_b` stay. */
  function stripSingleEmphasis(s) {
    var chars = Array.from(s);
    function open(c) {
      return c === undefined || /\s/.test(c) || (/[!-\/:-@\[-`{-~]/.test(c) && c !== "*" && c !== "_");
    }
    var out = "";
    for (var i = 0; i < chars.length; i++) {
      var c = chars[i];
      if ((c === "*" || c === "_") && open(chars[i - 1]) !== open(chars[i + 1])) continue;
      out += c;
    }
    return out;
  }

  /** `collapse_dot_leaders` in bench.rs: 4+ dots (single spaces allowed between) become a space. */
  function collapseDotLeaders(s) {
    return s.replace(/\.(?: ?\.){3,}/g, " ");
  }

  function markdownToText(md) {
    var lines = stripHtmlTags(md).split("\n");
    var out = [];
    lines.forEach(function (line) {
      if (line.charAt(line.length - 1) === "\r") line = line.slice(0, -1);
      var trimmed = line.replace(/\s+$/, "").replace(/^\s+/, "");
      if (trimmed.charAt(0) === "|" && /^[|\-: ]*$/.test(trimmed)) return;
      var s = trimmed.replace(/^#+/, "").replace(/^\s+/, "");
      if (trimmed.charAt(0) === "#" && !s) return;
      var prefixes = ["- ", "* ", "+ ", "> "];
      for (var i = 0; i < prefixes.length; i++) {
        if (s.indexOf(prefixes[i]) === 0) {
          s = s.slice(2);
          break;
        }
      }
      if (s.charAt(0) === "|") {
        s = s
          .replace(/^\|+|\|+$/g, "")
          .split("|")
          .map(function (c) {
            return c.trim();
          })
          .join(" ");
      }
      s = stripSingleEmphasis(s.split("**").join("").split("__").join("").split("`").join(""));
      out.push(s.trim());
    });
    return out.join("\n").replace(/\s+$/, "");
  }

  // Scorer v3 (SCORER_VERSION = 3 in bench.rs) adds single-emphasis stripping, word-preserving
  // inline tags, dot-leader collapsing and header-row matching for table_cell rules.
  // Scorer v2: HTML entities decoded after markdown stripping,
  // spaces touching punctuation dropped for rule matching, markdown *and* HTML tables, fuzzy
  // bag_of_sentences (0.8 per sentence, default threshold 0.8) and olmOCR `max_diffs`.
  var BAG_SENTENCE_MIN_SIMILARITY = 0.8;
  var BAG_DEFAULT_THRESHOLD = 0.8;
  var MAX_SPAN = 64;

  var NAMED_ENTITIES = {
    amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " ", ndash: "–", mdash: "—",
    lsquo: "‘", rsquo: "’", ldquo: "“", rdquo: "”", hellip: "…", deg: "°",
    plusmn: "±", times: "×", copy: "©", reg: "®", euro: "€", pound: "£",
    cent: "¢", sect: "§", para: "¶", middot: "·", bull: "•",
  };

  /** `tables::decode_entities`. */
  function decodeEntities(s) {
    if (s.indexOf("&") === -1) return s;
    var out = "";
    var rest = s;
    var i;
    while ((i = rest.indexOf("&")) !== -1) {
      out += rest.slice(0, i);
      var tail = rest.slice(i);
      var semi = tail.slice(0, 12).indexOf(";");
      var ch = null;
      if (semi !== -1) {
        var name = tail.slice(1, semi);
        if (name.charAt(0) === "#") {
          var num = name.slice(1);
          var code = null;
          if (/^[xX]/.test(num)) {
            if (/^\+?[0-9a-fA-F]+$/.test(num.slice(1))) code = parseInt(num.slice(1).replace("+", ""), 16);
          } else if (/^\+?[0-9]+$/.test(num)) {
            code = parseInt(num.replace("+", ""), 10);
          }
          if (code !== null && code <= 0x10ffff && !(code >= 0xd800 && code <= 0xdfff)) ch = String.fromCodePoint(code);
        } else if (Object.prototype.hasOwnProperty.call(NAMED_ENTITIES, name)) {
          ch = NAMED_ENTITIES[name];
        }
      }
      if (ch !== null) {
        out += ch;
        rest = tail.slice(semi + 1);
      } else {
        out += "&";
        rest = tail.slice(1);
      }
    }
    return out + rest;
  }

  function normalizeText(s, opts) {
    s = s.normalize ? s.normalize("NFKC") : s;
    if (opts.strip_markdown) s = decodeEntities(markdownToText(s));
    s = s.replace(/[‘’]/g, "'").replace(/[“”]/g, '"').replace(/[–—]/g, "-");
    s = collapseDotLeaders(s);
    if (opts.case_insensitive) s = s.toLowerCase();
    var out = "";
    var lastSpace = true;
    for (var i = 0; i < s.length; i++) {
      var c = s[i];
      if (/\s/.test(c)) {
        if (!lastSpace) {
          out += " ";
          lastSpace = true;
        }
      } else if (opts.strip_punctuation && /[!-\/:-@\[-`{-~]/.test(c)) {
        continue;
      } else {
        out += c;
        lastSpace = false;
      }
    }
    return out.trim();
  }

  /** `is_punct` in bench.rs. */
  var PUNCT_RE = /[!-\/:-@\[-`{-~¡§«¶·»¿‐-‧‰-⁞、-〃〈-】〔-〟।॥]/;

  /** `squeeze_punct_spaces`: drop every space that touches punctuation. */
  function squeezePunctSpaces(s) {
    var chars = Array.from(s);
    var out = "";
    for (var i = 0; i < chars.length; i++) {
      if (chars[i] === " ") {
        var prev = i > 0 ? chars[i - 1] : "";
        var next = i + 1 < chars.length ? chars[i + 1] : "";
        if ((prev && PUNCT_RE.test(prev)) || (next && PUNCT_RE.test(next))) continue;
      }
      out += chars[i];
    }
    return out;
  }

  function ruleNormalize(s, opts) {
    return squeezePunctSpaces(normalizeText(s, opts));
  }

  function splitRow(line) {
    var cells = [""];
    var escaped = false;
    var body = line.trim().replace(/^\|+/, "");
    for (var i = 0; i < body.length; i++) {
      var c = body[i];
      if (escaped) {
        if (c !== "|") cells[cells.length - 1] += "\\";
        cells[cells.length - 1] += c;
        escaped = false;
      } else if (c === "\\") escaped = true;
      else if (c === "|") cells.push("");
      else cells[cells.length - 1] += c;
    }
    if (escaped) cells[cells.length - 1] += "\\";
    if (cells.length > 1 && !cells[cells.length - 1].trim()) cells.pop();
    return cells.map(function (c) {
      return c.trim();
    });
  }

  /** `tables::markdown_tables`: raw cells, separator rows dropped. */
  function markdownTables(md) {
    var tables = [];
    var current = [];
    md.split("\n").forEach(function (line) {
      if (line.charAt(line.length - 1) === "\r") line = line.slice(0, -1);
      var t = line.trim();
      if (t.charAt(0) !== "|") {
        if (current.length) tables.push(current);
        current = [];
        return;
      }
      if (/^[|\-: ]*$/.test(t)) return;
      current.push(splitRow(t));
    });
    if (current.length) tables.push(current);
    return tables;
  }

  function asciiLower(s) {
    return s.replace(/[A-Z]+/g, function (m) {
      return m.toLowerCase();
    });
  }

  function findTableOpen(lower, from) {
    var at = from;
    for (;;) {
      var i = lower.indexOf("<table", at);
      if (i === -1) return -1;
      var next = lower.charAt(i + 6);
      if (next === "" || /[\t\n\f\r >\/]/.test(next)) return i;
      at = i + 6;
    }
  }

  function spanAttr(attrs, key) {
    var lower = asciiLower(attrs);
    var from = 0;
    var i;
    while ((i = lower.indexOf(key, from)) !== -1) {
      from = i + key.length;
      var beforeOk = i === 0 || !/[A-Za-z0-9]/.test(lower.charAt(i - 1));
      var rest = lower.slice(i + key.length).replace(/^\s+/, "");
      if (!beforeOk || rest.charAt(0) !== "=") continue;
      rest = rest.slice(1).replace(/^\s+/, "").replace(/^["']+/, "");
      var digits = /^[0-9]*/.exec(rest)[0];
      var n = digits ? parseInt(digits, 10) : NaN;
      return n >= 1 && n <= MAX_SPAN ? n : 1;
    }
    return 1;
  }

  function collapseWs(s) {
    return s.split(/\s+/).filter(Boolean).join(" ");
  }

  /** `tables::expand`: repeat spanning cells into every slot, pad, drop all-empty rows. */
  function expandGrid(rows) {
    var grid = [];
    rows.forEach(function (row, r) {
      while (grid.length <= r) grid.push([]);
      var c = 0;
      row.forEach(function (cell) {
        while (grid[r][c] !== undefined && grid[r][c] !== null) c++;
        var text = collapseWs(cell.text);
        for (var dr = 0; dr < cell.rowspan; dr++) {
          var rr = r + dr;
          while (grid.length <= rr) grid.push([]);
          for (var dc = 0; dc < cell.colspan; dc++) {
            var cc = c + dc;
            while (grid[rr].length <= cc) grid[rr].push(null);
            grid[rr][cc] = text;
          }
        }
        c += cell.colspan;
      });
    });
    var width = grid.reduce(function (w, row) {
      return Math.max(w, row.length);
    }, 0);
    return grid
      .map(function (row) {
        var out = [];
        for (var i = 0; i < width; i++) out.push(row[i] == null ? "" : row[i]);
        return out;
      })
      .filter(function (row) {
        return row.some(function (c) {
          return c !== "";
        });
      });
  }

  /** `tables::parse_html_table`: {grid, consumed}. */
  function parseHtmlTable(html) {
    var rows = [];
    var row = null;
    var cell = null;
    var depth = 0;
    var started = false;
    var i = 0;
    var textStart = 0;
    function closeCell() {
      if (cell) {
        (row = row || []).push(cell);
        cell = null;
      }
    }
    function closeRow() {
      closeCell();
      if (row && row.length) rows.push(row);
      row = null;
    }
    function addText(t) {
      if (cell) cell.text += t;
    }
    while (i < html.length) {
      if (html.charAt(i) !== "<") {
        i++;
        continue;
      }
      if (html.slice(i, i + 4) === "<!--") {
        addText(decodeEntities(html.slice(textStart, i)));
        var endc = html.indexOf("-->", i + 4);
        i = endc === -1 ? html.length : endc + 3;
        textStart = i;
        continue;
      }
      var rel = html.indexOf(">", i);
      if (rel === -1) break;
      var tagEnd = rel + 1;
      var inner = html.slice(i + 1, tagEnd - 1);
      var closing = inner.charAt(0) === "/";
      var rest = closing ? inner.slice(1) : inner;
      var nameLen = /^[A-Za-z0-9]*/.exec(rest)[0].length;
      var name = asciiLower(rest.slice(0, nameLen));
      var attrs = rest.slice(nameLen);
      if (!name || !/[A-Za-z]/.test(rest.charAt(0))) {
        i++;
        continue;
      }
      addText(decodeEntities(html.slice(textStart, i)));
      i = tagEnd;
      textStart = i;
      if (!closing && name === "table") {
        if (started) {
          depth++;
          addText(" ");
        } else started = true;
      } else if (closing && name === "table") {
        if (depth > 0) {
          depth--;
          addText(" ");
        } else {
          closeRow();
          return { grid: expandGrid(rows), consumed: tagEnd };
        }
      } else if (depth > 0) {
        addText(" ");
      } else if (name === "tr") {
        closeRow();
      } else if (!closing && (name === "td" || name === "th")) {
        closeCell();
        cell = { text: "", colspan: spanAttr(attrs, "colspan"), rowspan: spanAttr(attrs, "rowspan") };
      } else if (closing && (name === "td" || name === "th")) {
        closeCell();
      } else if (name === "br" || name === "p" || name === "li" || name === "div") {
        addText(" ");
      }
    }
    addText(decodeEntities(html.slice(Math.min(textStart, html.length))));
    closeRow();
    return { grid: expandGrid(rows), consumed: html.length };
  }

  /** `tables::extract_tables`: markdown and HTML tables in document order, raw cells. */
  function extractTables(doc) {
    var lower = asciiLower(doc);
    var out = [];
    var pos = 0;
    var start;
    while ((start = findTableOpen(lower, pos)) !== -1) {
      out = out.concat(markdownTables(doc.slice(pos, start)));
      var parsed = parseHtmlTable(doc.slice(start));
      if (parsed.grid.length) out.push(parsed.grid);
      pos = start + Math.max(parsed.consumed, 1);
    }
    return out.concat(markdownTables(doc.slice(Math.min(pos, doc.length))));
  }

  function parseTables(md, opts) {
    return extractTables(md).map(function (grid) {
      var rows = grid.map(function (row) {
        return row.map(function (c) {
          return ruleNormalize(c, opts);
        });
      });
      return { header: rows[0], rows: rows.slice(1) };
    });
  }

  function asNumber(s) {
    var cleaned = s.replace(/[,$%  ]/g, "");
    if (!cleaned) return null;
    var m = /^\((.*)\)$/.exec(cleaned);
    if (m) cleaned = "-" + m[1];
    if (!/^[+-]?(\d+\.?\d*|\.\d+)(e[+-]?\d+)?$/i.test(cleaned)) return null;
    var n = parseFloat(cleaned);
    return isFinite(n) ? n : null;
  }

  function valuesEqual(a, b) {
    if (a === b) return true;
    var x = asNumber(a);
    var y = asNumber(b);
    return x !== null && y !== null && Math.abs(x - y) <= 1e-6 * Math.max(Math.abs(x), Math.abs(y), 1);
  }

  function levenshtein(a, b) {
    if (!a.length) return b.length;
    if (!b.length) return a.length;
    var prev = [];
    var cur = [];
    for (var j = 0; j <= b.length; j++) prev.push(j);
    for (var i = 0; i < a.length; i++) {
      cur = [i + 1];
      for (j = 0; j < b.length; j++) cur.push(Math.min(prev[j + 1] + 1, cur[j] + 1, prev[j] + (a[i] === b[j] ? 0 : 1)));
      prev = cur;
    }
    return prev[b.length];
  }

  /** `approx_contains`: some substring of `hay` within `max` edits of `needle` (char arrays). */
  function approxContains(needle, hay, max) {
    var m = needle.length;
    if (m <= max) return true;
    var col = [];
    for (var i = 0; i <= m; i++) col.push(i);
    for (var j = 0; j < hay.length; j++) {
      var h = hay[j];
      var diag = 0;
      for (i = 1; i <= m; i++) {
        var up = col[i];
        var v = needle[i - 1] === h ? diag : 1 + Math.min(diag, up, col[i - 1]);
        diag = up;
        col[i] = v;
      }
      if (col[m] <= max) return true;
    }
    return false;
  }

  /** `approx_match_ends`: exclusive end positions of matches within `max` edits. */
  function approxMatchEnds(needle, hay, max) {
    var m = needle.length;
    var ends = [];
    var col = [];
    for (var i = 0; i <= m; i++) col.push(i);
    if (col[m] <= max) ends.push(0);
    for (var j = 0; j < hay.length; j++) {
      var h = hay[j];
      var diag = 0;
      for (i = 1; i <= m; i++) {
        var up = col[i];
        var v = needle[i - 1] === h ? diag : 1 + Math.min(diag, up, col[i - 1]);
        diag = up;
        col[i] = v;
      }
      if (col[m] <= max) ends.push(j + 1);
    }
    return ends;
  }

  /** `approx_match_start_range`: [earliest start, latest start] or null. */
  function approxMatchStartRange(needle, hay, max) {
    var ends = approxMatchEnds(Array.from(needle).reverse(), hay.slice().reverse(), max);
    if (!ends.length) return null;
    var n = hay.length;
    return [n - Math.max.apply(null, ends), n - Math.min.apply(null, ends)];
  }

  function containsWithin(hay, needle, k) {
    if (!k) return hay.indexOf(needle) !== -1;
    return approxContains(Array.from(needle), Array.from(hay), k);
  }

  function valuesClose(a, b, k) {
    return valuesEqual(a, b) || (k > 0 && levenshtein(Array.from(a), Array.from(b)) <= k);
  }

  function snippet(s) {
    var chars = Array.from(s);
    return chars.length <= 60 ? JSON.stringify(s) : JSON.stringify(chars.slice(0, 60).join("")) + "…";
  }

  function RuleChecker(prediction, opts) {
    var self = this;
    var texts = {};
    var chars = {};
    var tables = {};
    function optsFor(cs) {
      return { case_insensitive: !cs, strip_markdown: opts.strip_markdown, strip_punctuation: opts.strip_punctuation };
    }
    self.text = function (cs) {
      var k = cs ? 1 : 0;
      if (texts[k] === undefined) texts[k] = ruleNormalize(prediction, optsFor(cs));
      return texts[k];
    };
    self.chars = function (cs) {
      var k = cs ? 1 : 0;
      if (chars[k] === undefined) chars[k] = Array.from(self.text(cs));
      return chars[k];
    };
    self.tables = function (cs) {
      var k = cs ? 1 : 0;
      if (tables[k] === undefined) tables[k] = parseTables(prediction, optsFor(cs));
      return tables[k];
    };
    self.needle = function (s, cs) {
      return ruleNormalize(String(s), optsFor(cs));
    };
  }

  function sentencePresent(ck, sentence, cs) {
    if (!sentence) return false;
    if (ck.text(cs).indexOf(sentence) !== -1) return true;
    var needle = Array.from(sentence);
    var max = Math.floor((1 - BAG_SENTENCE_MIN_SIMILARITY) * needle.length);
    return max > 0 && approxContains(needle, ck.chars(cs), max);
  }

  /** {ok, detail, found?: bool[]} for one rule. */
  function checkRule(ck, rule) {
    var cs = !!rule.case_sensitive;
    var k = isNum(rule.max_diffs) ? rule.max_diffs : 0;
    var type = rule.type;
    if (type === "present" || type === "absent") {
      if (rule.text == null) return { ok: false, detail: "missing `text`" };
      var needle = ck.needle(rule.text, cs);
      if (!needle) return { ok: false, detail: "`text` " + snippet(rule.text) + " is empty after normalisation" };
      var found = k === 0 ? ck.text(cs).indexOf(needle) !== -1 : approxContains(Array.from(needle), ck.chars(cs), k);
      if (type === "present" && !found) return { ok: false, detail: "not found: " + snippet(needle) };
      if (type === "absent" && found) return { ok: false, detail: "present but must be absent: " + snippet(needle) };
      return { ok: true };
    }
    if (type === "order") {
      if (rule.before == null) return { ok: false, detail: "missing `before`" };
      if (rule.after == null) return { ok: false, detail: "missing `after`" };
      var before = ck.needle(rule.before, cs);
      var after = ck.needle(rule.after, cs);
      if (!before || !after) return { ok: false, detail: "`before` or `after` is empty after normalisation" };
      if (k > 0) {
        var bRange = approxMatchStartRange(before, ck.chars(cs), k);
        if (!bRange) return { ok: false, detail: "`before` not found within " + k + " edits: " + snippet(before) };
        var aRange = approxMatchStartRange(after, ck.chars(cs), k);
        if (!aRange) return { ok: false, detail: "`after` not found within " + k + " edits: " + snippet(after) };
        if (bRange[0] < aRange[1]) return { ok: true };
        return { ok: false, detail: snippet(after) + " occurs only before " + snippet(before) };
      }
      var hay = ck.text(cs);
      var b = hay.indexOf(before);
      if (b === -1) return { ok: false, detail: "`before` not found: " + snippet(before) };
      if (hay.slice(b + before.length).indexOf(after) !== -1) return { ok: true };
      if (hay.indexOf(after) !== -1) return { ok: false, detail: snippet(after) + " occurs only before " + snippet(before) };
      return { ok: false, detail: "`after` not found: " + snippet(after) };
    }
    if (type === "bag_of_sentences") {
      var sentences = rule.sentences || [];
      if (!sentences.length) return { ok: false, detail: "missing `sentences`" };
      var threshold = isNum(rule.threshold) ? rule.threshold : BAG_DEFAULT_THRESHOLD;
      var hits = 0;
      var marks = sentences.map(function (s) {
        var hit = sentencePresent(ck, ck.needle(s, cs), cs);
        if (hit) hits++;
        return hit;
      });
      var fraction = hits / sentences.length;
      if (fraction + 1e-9 >= threshold) return { ok: true, found: marks };
      return {
        ok: false,
        found: marks,
        detail: hits + "/" + sentences.length + " sentences present (" + fraction.toFixed(3) + " < threshold " + threshold.toFixed(3) + ")",
      };
    }
    if (type === "table_cell") {
      if (!rule.cell) return { ok: false, detail: "missing `cell`" };
      return checkTableCell(ck, rule.cell, cs, k);
    }
    return { ok: false, detail: "unknown rule type " + JSON.stringify(type) };
  }

  function checkTableCell(ck, cell, cs, k) {
    var value = ck.needle(cell.value == null ? "" : cell.value, cs);
    if (!value) return { ok: false, detail: "`cell.value` is empty after normalisation" };
    var rowH = cell.row_header != null ? ck.needle(cell.row_header, cs) : null;
    if (rowH === "") return { ok: false, detail: "`cell.row_header` is empty after normalisation" };
    var colH = cell.col_header != null ? ck.needle(cell.col_header, cs) : null;
    if (colH === "") return { ok: false, detail: "`cell.col_header` is empty after normalisation" };
    var tables = ck.tables(cs);
    if (!tables.length) return { ok: false, detail: "prediction has no table (markdown or HTML)" };
    var sawRow = false;
    var sawCol = false;
    var seen = [];
    for (var t = 0; t < tables.length; t++) {
      var table = tables[t];
      var col = null;
      if (colH !== null) {
        col = -1;
        for (var h = 0; h < table.header.length; h++) {
          if (containsWithin(table.header[h], colH, k)) {
            col = h;
            break;
          }
        }
        if (col === -1) continue;
        sawCol = true;
      }
      var allRows = [table.header].concat(table.rows);
      for (var r = 0; r < allRows.length; r++) {
        var row = allRows[r];
        if (rowH !== null && !row.some(function (c) { return containsWithin(c, rowH, k); })) continue;
        sawRow = true;
        if (col !== null) {
          if (col >= row.length) continue;
          if (valuesClose(row[col], value, k)) return { ok: true };
          seen.push(row[col]);
        } else if (row.some(function (c) { return valuesClose(c, value, k); }) || containsWithin(row.join(" "), value, k)) {
          return { ok: true };
        }
      }
    }
    if (colH !== null && !sawCol) return { ok: false, detail: "no column matching " + snippet(colH) };
    if (!sawRow) return { ok: false, detail: rowH !== null ? "no row matching " + snippet(rowH) : "prediction has no table rows" };
    if (seen.length) return { ok: false, detail: "cell is " + snippet(seen[0]) + " , expected " + snippet(value) };
    return { ok: false, detail: "no cell in the matched row holds " + snippet(value) };
  }

  function scoreRules(prediction, rules, opts) {
    var ck = new RuleChecker(prediction, opts);
    var passed = 0;
    var results = rules.map(function (rule) {
      var r = checkRule(ck, rule);
      if (r.ok) passed++;
      return r;
    });
    return { passed: passed, total: rules.length, results: results };
  }

  /* ------------------------------------------------------- page viewer */

  var pdfLib = null;

  function loadPdfJs() {
    if (pdfLib) return pdfLib;
    pdfLib = new Promise(function (resolve, reject) {
      var script = document.createElement("script");
      if (PDFJS_LOCAL) {
        // Bundled with the CLI and served from this origin: no CDN, no network.
        script.src = url(PDFJS_LOCAL + "pdf.min.js");
      } else {
        script.src = PDFJS.lib;
        script.integrity = PDFJS.libSri;
        script.crossOrigin = "anonymous";
        script.referrerPolicy = "no-referrer";
      }
      script.onload = function () {
        resolve(window.pdfjsLib);
      };
      script.onerror = function () {
        reject(new Error(PDFJS_LOCAL ? "pdf.js could not be loaded" : "pdf.js could not be loaded from cdnjs"));
      };
      document.head.appendChild(script);
    }).then(function (lib) {
      if (!lib) throw new Error("pdf.js did not initialise");
      // Fetch the worker with the same integrity check, then run it from a blob URL.
      var worker = PDFJS_LOCAL
        ? fetch(url(PDFJS_LOCAL + "pdf.worker.min.js"))
        : fetch(PDFJS.worker, { integrity: PDFJS.workerSri, mode: "cors", credentials: "omit" });
      return worker
        .then(function (resp) {
          if (!resp.ok) throw new Error("pdf.js worker HTTP " + resp.status);
          return resp.blob();
        })
        .then(function (blob) {
          lib.GlobalWorkerOptions.workerSrc = URL.createObjectURL(blob);
          return lib;
        });
    });
    pdfLib.catch(function () {
      pdfLib = null;
    });
    return pdfLib;
  }

  function groupOf(type) {
    for (var i = 0; i < BLOCK_GROUPS.length; i++) if (BLOCK_GROUPS[i].types.indexOf(type) !== -1) return BLOCK_GROUPS[i];
    return BLOCK_GROUPS[BLOCK_GROUPS.length - 1];
  }

  /** Blocks with a usable bbox from a unified ParseResponse, grouped by 1-based page. */
  function blocksByPage(response) {
    var pages = {};
    var count = 0;
    function take(block, fallbackPage) {
      var b = block && block.bbox;
      if (!b || ![b.x0, b.y0, b.x1, b.y1].every(isNum)) return;
      var page = block.page_number || fallbackPage || 1;
      (pages[page] = pages[page] || []).push(block);
      count++;
    }
    ((response && response.pages) || []).forEach(function (page) {
      (page.blocks || []).forEach(function (block) {
        take(block, page.page_number);
      });
    });
    ((response && response.blocks) || []).forEach(function (block) {
      take(block, 1);
    });
    return { pages: pages, count: count };
  }

  function placeholder(title, body) {
    return '<div class="placeholder"><strong>' + esc(title) + "</strong>" + (body ? "<span>" + body + "</span>" : "") + "</div>";
  }

  /**
   * The page panel. Shows the input page (an image, or a page image the build rendered from the
   * PDF; pdf.js only for a PDF page the build did not render) and draws a model's layout boxes
   * over it. Boxes are drawn only once a page image is on screen, never over a placeholder.
   * Built once per document and kept while the model or tab changes.
   */
  function SourceViewer(key, opts) {
    var self = this;
    self.key = key;
    self.opts = opts;
    self.page = 1;
    self.pages = Math.max(1, opts.pages || 1, (opts.previews || []).length);
    self.overlay = null;
    self.overlayOn = false;
    self.ready = false;
    self.token = 0;
    self.el = document.createElement("div");
    self.el.className = "sv";
    self.el.innerHTML =
      '<div class="sv-bar">' +
      '<div class="pager" hidden><button type="button" class="iconbtn" data-page="-1" aria-label="Previous page ([)">‹</button>' +
      '<span class="pager-label"></span>' +
      '<button type="button" class="iconbtn" data-page="1" aria-label="Next page (])">›</button></div>' +
      '<span class="spacer"></span>' +
      '<div class="zoom" role="group" aria-label="Zoom"><button type="button" class="iconbtn" data-zoom="-1" aria-label="Zoom out">−</button>' +
      '<button type="button" class="zoom-label" data-zoom="0" title="Fit to width">100%</button>' +
      '<button type="button" class="iconbtn" data-zoom="1" aria-label="Zoom in">+</button></div>' +
      '<button type="button" class="toggle ov-toggle" aria-pressed="false" hidden title="Layout boxes from the unified response (o; hold to peek)">Layout boxes</button>' +
      (opts.restricted ? "" : '<a class="sv-open" href="' + esc(opts.inputUrl) + '" rel="noopener" target="_blank">Open original</a>') +
      "</div>" +
      '<div class="sv-page"><div class="sv-media"></div>' +
      '<svg class="ov" viewBox="0 0 1000 1000" preserveAspectRatio="none" aria-hidden="true" style="display:none"></svg></div>' +
      '<div class="sv-legend" hidden></div>';
    self.media = self.el.querySelector(".sv-media");
    self.svg = self.el.querySelector("svg.ov");
    self.legend = self.el.querySelector(".sv-legend");
    self.toggle = self.el.querySelector(".ov-toggle");
    self.pager = self.el.querySelector(".pager");
    self.pager.querySelectorAll("button").forEach(function (b) {
      b.addEventListener("click", function () {
        self.onPage(self.page + Number(b.getAttribute("data-page")));
      });
    });
    self.toggle.addEventListener("click", function () {
      self.onOverlay(!self.overlayOn);
    });
    self.zoom = 1;
    self.pageEl = self.el.querySelector(".sv-page");
    self.el.querySelectorAll("[data-zoom]").forEach(function (b) {
      b.addEventListener("click", function () {
        var step = Number(b.getAttribute("data-zoom"));
        self.setZoom(step === 0 ? 1 : self.zoom + step * 0.25);
      });
    });
    // A click on a box selects its block (the inspector shows it in the Blocks tab).
    self.svg.addEventListener("click", function (e) {
      var r = e.target.closest && e.target.closest("rect[data-i]");
      if (r && self.onBoxClick) self.onBoxClick(self.page, Number(r.getAttribute("data-i")));
    });
    self.linked = false;
    self.peek = false;
    self.active = null;
    // Hooks the inspector replaces so page / overlay changes land in the URL.
    self.onPage = function (n) {
      self.showPage(n);
    };
    self.onOverlay = function (on) {
      self.setOverlayOn(on);
    };
    self.load();
  }

  /** The image for the current page, or null when only pdf.js could show it. */
  SourceViewer.prototype.imageFor = function (page) {
    var o = this.opts;
    if (!o.isPdf) return page === 1 ? o.imageUrl : null;
    return (o.previews || [])[page - 1] || null;
  };

  SourceViewer.prototype.load = function () {
    var self = this;
    var token = ++self.token;
    self.ready = false;
    self.drawOverlay();
    self.updatePager();
    if (self.opts.restricted) {
      self.media.innerHTML = placeholder("Page not redistributed", "This source's licence is research-only. Fetch it locally to view the page.");
      return;
    }
    var src = self.imageFor(self.page);
    if (src) {
      var img = new Image();
      img.alt = "Page " + self.page + " of " + self.opts.title;
      img.decoding = "async";
      img.onload = function () {
        if (token !== self.token) return;
        self.media.innerHTML = "";
        self.media.appendChild(img);
        self.ready = true;
        self.drawOverlay();
      };
      img.onerror = function () {
        if (token !== self.token) return;
        self.media.innerHTML = placeholder("The page could not be displayed", '<a href="' + esc(self.opts.inputUrl) + '">Open the file</a>');
      };
      if (!self.media.firstChild) self.media.innerHTML = placeholder("Loading page…");
      img.src = src;
      return;
    }
    if (!self.opts.isPdf) {
      self.media.innerHTML = placeholder("No inline preview", '<a href="' + esc(self.opts.inputUrl) + '">Open the file</a>');
      return;
    }
    self.media.innerHTML = placeholder("Rendering page " + self.page + "…");
    self.renderPdf(token);
  };

  /** pdf.js fallback for a PDF page the build did not render. */
  SourceViewer.prototype.renderPdf = function (token) {
    var self = this;
    var docPromise = self.pdfDoc;
    if (!docPromise) {
      docPromise = self.pdfDoc = loadPdfJs().then(function (lib) {
        return lib.getDocument({ url: self.opts.inputUrl, isEvalSupported: false }).promise;
      });
      docPromise.catch(function () {
        self.pdfDoc = null;
      });
    }
    docPromise
      .then(function (pdf) {
        if (token !== self.token) return null;
        if (pdf.numPages !== self.pages) {
          self.pages = pdf.numPages;
          self.updatePager();
        }
        return pdf.getPage(Math.max(1, Math.min(self.page, pdf.numPages))).then(function (page) {
          if (token !== self.token) return null;
          var base = page.getViewport({ scale: 1 });
          var cssWidth = Math.max(320, Math.min(self.pageEl.clientWidth || self.el.clientWidth || 640, 2400));
          var ratio = Math.min(window.devicePixelRatio || 1, 2);
          var viewport = page.getViewport({ scale: (cssWidth / base.width) * ratio });
          var canvas = document.createElement("canvas");
          canvas.width = Math.floor(viewport.width);
          canvas.height = Math.floor(viewport.height);
          canvas.setAttribute("role", "img");
          canvas.setAttribute("aria-label", "Page " + self.page + " of " + self.opts.title);
          return page.render({ canvasContext: canvas.getContext("2d"), viewport: viewport }).promise.then(function () {
            if (token !== self.token) return;
            self.media.innerHTML = "";
            self.media.appendChild(canvas);
            self.canvasShown = true;
            self.ready = true;
            self.drawOverlay();
          });
        });
      })
      .catch(function () {
        if (token !== self.token) return;
        self.media.innerHTML = placeholder("This page cannot be shown here", '<a href="' + esc(self.opts.inputUrl) + '">Download the PDF</a>');
      });
  };

  SourceViewer.prototype.rerender = function () {
    if (this.canvasShown && !this.imageFor(this.page)) this.load();
  };

  SourceViewer.prototype.updatePager = function () {
    var multi = this.pages > 1 && !this.opts.restricted;
    this.pager.hidden = !multi;
    this.pager.querySelector(".pager-label").textContent = this.page + " / " + this.pages;
    this.pager.querySelector('[data-page="-1"]').disabled = this.page <= 1;
    this.pager.querySelector('[data-page="1"]').disabled = this.page >= this.pages;
  };

  SourceViewer.prototype.showPage = function (n) {
    n = Math.max(1, Math.min(n || 1, this.pages));
    if (n === this.page) return;
    this.page = n;
    this.load();
  };

  SourceViewer.prototype.setOverlay = function (overlay, message) {
    this.overlay = overlay && overlay.count ? overlay : null;
    this.toggle.hidden = !this.overlay || this.opts.restricted;
    this.toggle.title = (message || "Layout boxes") + " (o)";
    this.drawOverlay();
  };

  SourceViewer.prototype.setOverlayOn = function (on) {
    this.overlayOn = !!on;
    this.drawOverlay();
  };

  /** Boxes shown while a key is held, without changing the toggle. */
  SourceViewer.prototype.setPeek = function (on) {
    this.peek = !!on;
    this.drawOverlay();
  };

  SourceViewer.prototype.setZoom = function (z) {
    this.zoom = Math.max(0.5, Math.min(3, Math.round(z * 4) / 4));
    this.pageEl.style.width = this.zoom === 1 ? "" : this.zoom * 100 + "%";
    this.el.querySelector(".zoom-label").textContent = Math.round(this.zoom * 100) + "%";
    if (this.canvasShown && !this.imageFor(this.page)) this.load();
  };

  /** Emphasise one block's box (the others stay faint) and scroll it into view. */
  SourceViewer.prototype.setActive = function (page, i) {
    this.active = { page: page, i: i };
    if (page !== this.page) {
      this.showPage(page);
      return;
    }
    this.drawOverlay();
    var rect = this.svg.querySelector('rect[data-i="' + i + '"]');
    if (rect && rect.scrollIntoView) rect.scrollIntoView({ block: "nearest", inline: "nearest", behavior: reducedMotion() ? "auto" : "smooth" });
  };

  SourceViewer.prototype.drawOverlay = function () {
    var available = !!this.overlay;
    var active = available && (this.overlayOn || this.linked || this.peek) && this.ready;
    var current = this.active && this.active.page === this.page ? this.active.i : -1;
    this.toggle.setAttribute("aria-pressed", String(available && this.overlayOn));
    var blocks = active ? this.overlay.pages[this.page] || [] : [];
    var used = {};
    this.svg.innerHTML = blocks
      .map(function (block, i) {
        var b = block.bbox;
        var g = groupOf(block.type);
        used[g.key] = g;
        var x = Math.min(b.x0, b.x1) * 1000;
        var y = Math.min(b.y0, b.y1) * 1000;
        var w = Math.abs(b.x1 - b.x0) * 1000;
        var h = Math.abs(b.y1 - b.y0) * 1000;
        var text = String(block.text || block.content || "").replace(/\s+/g, " ").slice(0, 160);
        return (
          '<rect class="bt-' + g.key + (i === current ? " on" : "") + '" data-i="' + i + '" x="' + x.toFixed(1) + '" y="' + y.toFixed(1) + '" width="' + w.toFixed(1) +
          '" height="' + h.toFixed(1) + '" vector-effect="non-scaling-stroke"><title>' +
          esc("#" + (i + 1) + " " + (block.type || "?") + (isNum(block.confidence) ? " (" + block.confidence.toFixed(2) + ")" : "") + " — " + text) +
          "</title></rect>"
        );
      })
      .join("");
    this.svg.style.display = active ? "" : "none";
    this.svg.classList.toggle("linked", this.linked || current !== -1);
    var keys = BLOCK_GROUPS.filter(function (g) {
      return used[g.key];
    });
    this.legend.hidden = !active;
    this.legend.innerHTML = active
      ? keys
          .map(function (g) {
            return '<span><span class="sw bt-' + g.key + '"></span>' + esc(g.label) + "</span>";
          })
          .join("") + "<span>" + plural(blocks.length, "box", "boxes") + " on this page</span>"
      : "";
  };

  /* ---------------------------------------------------------- inspector */

  function metricsList(rec) {
    var m = (rec && rec.metrics) || {};
    var items = [];
    function add(label, value, title) {
      if (value !== null && value !== undefined && value !== "—") items.push([label, value, title]);
    }
    if (isNum(m.rules_total)) add("Checks passed", m.rules_passed + " / " + m.rules_total);
    if (isNum(m.char_similarity) && !isNum(m.rule_pass_rate)) add("Char similarity", fixed(m.char_similarity, 4));
    if (isNum(m.table_score)) add("Table", fixed(m.table_score, 4), "Similarity restricted to table rows");
    if (isNum(m.teds_grid)) add("TEDS", fixed(m.teds_grid, 4), "Tree-edit-distance similarity on the row/cell grid");
    if (!isNum(m.rule_pass_rate)) {
      if (isNum(m.cer)) add("CER", fixed(m.cer, 4), "Character error rate, lower is better");
      if (isNum(m.wer)) add("WER", fixed(m.wer, 4), "Word error rate, lower is better");
      if (isNum(m.word_f1)) add("Word F1", fixed(m.word_f1, 4));
    }
    if (isNum(m.order_score)) add("Order", fixed(m.order_score, isNum(m.rule_pass_rate) ? 3 : 4), isNum(m.rule_pass_rate) ? "Pass rate of the order checks" : "Reading-order agreement of shared lines");
    if (isNum(m.pred_chars) && (m.pred_chars || m.truth_chars)) add("Characters", m.pred_chars + " / " + m.truth_chars, "Output / truth characters after normalisation");
    if (rec) {
      add("Latency", msFmt(rec.latency_ms), "Client-measured, caches disabled");
      if (isNum(rec.cost_usd)) add("Cost", "$" + rec.cost_usd.toFixed(4), "List price for this document");
      if (isNum(rec.attempts) && rec.attempts > 1) add("Attempts", String(rec.attempts));
    }
    return (
      '<dl class="metrics">' +
      items
        .map(function (it) {
          return "<div" + (it[2] ? ' title="' + esc(it[2]) + '"' : "") + "><dt>" + esc(it[0]) + "</dt><dd>" + esc(it[1]) + "</dd></div>";
        })
        .join("") +
      "</dl>" +
      (rec ? '<p class="caption">The score is the ' + esc(scoreBasis(rec)) + ", times 100.</p>" : "")
    );
  }

  function q(text) {
    return "“" + esc(text) + "”";
  }

  function clip(text, n) {
    var chars = Array.from(String(text == null ? "" : text));
    return chars.length <= n ? chars.join("") : chars.slice(0, n).join("") + "…";
  }

  var RULE_GROUPS = [
    { type: "present", label: "Text present" },
    { type: "absent", label: "Text absent" },
    { type: "order", label: "Reading order" },
    { type: "table_cell", label: "Table cells" },
    { type: "bag_of_sentences", label: "Sentences" },
  ];

  function ruleGroupOf(type) {
    for (var i = 0; i < RULE_GROUPS.length; i++) if (RULE_GROUPS[i].type === type) return i;
    return RULE_GROUPS.length;
  }

  /** A check as one plain sentence (docs/DESIGN.md, check row). */
  function ruleSentence(rule) {
    var fuzzy = isNum(rule.max_diffs) && rule.max_diffs > 0 ? ' <span class="muted">(±' + rule.max_diffs + ")</span>" : "";
    if (rule.type === "present") return "Should contain " + q(rule.text) + fuzzy;
    if (rule.type === "absent") return "Should not contain " + q(rule.text) + fuzzy;
    if (rule.type === "order") return q(clip(rule.before, 80)) + " should come before " + q(clip(rule.after, 80)) + fuzzy;
    if (rule.type === "table_cell") {
      var c = rule.cell || {};
      var where = [];
      if (c.row_header != null) where.push("in row " + q(c.row_header));
      if (c.col_header != null) where.push("under header " + q(c.col_header));
      return "Table cell " + q(c.value) + (where.length ? " " + where.join(" ") : "") + fuzzy;
    }
    if (rule.type === "bag_of_sentences") {
      var t = isNum(rule.threshold) ? rule.threshold : BAG_DEFAULT_THRESHOLD;
      return "≥" + fixed(t * 100, 0) + "% of " + plural((rule.sentences || []).length, "sentence") + " present";
    }
    return "<code>" + esc(JSON.stringify(rule)) + "</code>";
  }

  /** The failure reason, only when it says more than the sentence already does. */
  function ruleReason(rule, r) {
    if (r.ok || !r.detail) return "";
    var d = r.detail;
    if (/^not found: /.test(d) && rule.type === "present") return "";
    if (/^present but must be absent/.test(d)) return "";
    if (/^`before` not found/.test(d)) return "The first text is missing";
    if (/^`after` not found/.test(d)) return "The second text is missing";
    if (/ occurs only before /.test(d)) return "Both are present, in the wrong order";
    if (/^prediction has no table/.test(d)) return "The output has no table";
    var cell = /^cell is (".*") , expected/.exec(d);
    if (cell) return "The cell reads " + esc(cell[1].replace(/^"|"$/g, "“").replace(/"$/, "”"));
    if (/^no column matching/.test(d)) return "No column with that header";
    if (/^no row matching/.test(d)) return "No row with that header";
    return esc(d.replace(/`/g, ""));
  }

  function bagDetail(rule, r) {
    var sentences = rule.sentences || [];
    var found = (r && r.found) || [];
    var hits = found.filter(Boolean).length;
    var order = sentences.map(function (s, i) {
      return i;
    });
    // Missing sentences first: they are what explains a failure.
    order.sort(function (a, b) {
      return (found[a] ? 1 : 0) - (found[b] ? 1 : 0) || a - b;
    });
    var shown = order.slice(0, BAG_SENTENCE_CAP);
    var rest = sentences.length - shown.length;
    return (
      "<details><summary>" + hits + " of " + sentences.length + " found</summary><ul class=\"bag\">" +
      shown
        .map(function (i) {
          return '<li class="' + (found[i] ? "ok" : "no") + '"><span class="mk">' + (found[i] ? "✓" : "✗") + "</span>" + esc(sentences[i]) + "</li>";
        })
        .join("") +
      (rest > 0 ? "<li>… " + rest + " more</li>" : "") +
      "</ul></details>"
    );
  }

  function checkRow(rule, r) {
    var reason = rule.type === "bag_of_sentences" ? "" : ruleReason(rule, r);
    var full = rule.type === "order" ? rule.before + " → " + rule.after : rule.text || (rule.cell && rule.cell.value) || "";
    return (
      '<li class="check ' + (r.ok ? "pass" : "fail") + '"' + (full && String(full).length > 120 ? ' title="' + esc(full) + '"' : "") + ">" +
      '<span class="ck" role="img" aria-label="' + (r.ok ? "pass" : "fail") + '">' + (r.ok ? "✓" : "✗") + "</span>" +
      '<span class="ct">' + ruleSentence(rule) + "</span>" +
      (reason ? '<span class="why">' + reason + "</span>" : "") +
      (rule.type === "bag_of_sentences" ? bagDetail(rule, r) : "") +
      "</li>"
    );
  }

  function rulesChecklist(rules, scored, rec, filter, limit) {
    if (!rules) return '<p class="empty">The checks for this document could not be loaded.</p>';
    if (!scored) return '<p class="empty">No saved output to run the checks against.</p>';
    var m = (rec && rec.metrics) || {};
    var matches = m.rules_passed === scored.passed && m.rules_total === scored.total;
    var failing = scored.total - scored.passed;
    var items = [];
    var groupTotals = {};
    rules.forEach(function (rule, i) {
      var r = scored.results[i];
      var g = ruleGroupOf(rule.type);
      var t = (groupTotals[g] = groupTotals[g] || { p: 0, n: 0 });
      t.n++;
      if (r.ok) t.p++;
      if (filter === "fail" && r.ok) return;
      if (filter === "pass" && !r.ok) return;
      items.push({ rule: rule, r: r, i: i, g: g });
    });
    var grouped = Object.keys(groupTotals).length > 1;
    if (grouped)
      items.sort(function (a, b) {
        return a.g - b.g || a.i - b.i;
      });
    var shown = items.slice(0, limit);
    var html = "";
    var current = -1;
    shown.forEach(function (it) {
      if (grouped && it.g !== current) {
        if (current !== -1) html += "</ul></div>";
        current = it.g;
        var t = groupTotals[it.g];
        html +=
          '<div class="check-group"><h3>' + esc(it.g < RULE_GROUPS.length ? RULE_GROUPS[it.g].label : "Other") +
          ' <span class="gc">· ' + t.p + " of " + t.n + " pass</span></h3><ul class=\"checks\">";
      } else if (!grouped && current === -1) {
        current = 0;
        html += '<div class="check-group"><ul class="checks">';
      }
      html += checkRow(it.rule, it.r);
    });
    if (current !== -1) html += "</ul></div>";
    return (
      '<div class="tool-row"><div class="seg" role="group" aria-label="Show">' +
      '<button type="button" data-rf="fail" aria-pressed="' + (filter === "fail") + '">Failing ' + failing + "</button>" +
      '<button type="button" data-rf="pass" aria-pressed="' + (filter === "pass") + '">Passing ' + scored.passed + "</button>" +
      '<button type="button" data-rf="all" aria-pressed="' + (filter === "all") + '">All ' + scored.total + "</button></div>" +
      (m.rules_total != null
        ? matches
          ? '<span class="recheck ok" id="recheck">Re-checked in your browser: matches the recorded score</span>'
          : '<span class="recheck warn" id="recheck">Re-checked in your browser: ' + scored.passed + " / " + scored.total + ", recorded " + esc(m.rules_passed) + " / " + esc(m.rules_total) + " (the recorded score is authoritative)</span>"
        : "") +
      "</div>" +
      (items.length
        ? html + (items.length > limit ? '<button type="button" class="btn more-rules">Show ' + Math.min(RULE_PAGE, items.length - limit) + " more of " + (items.length - limit) + "</button>" : "")
        : '<p class="empty">No ' + (filter === "fail" ? "failing" : filter === "pass" ? "passing" : "") + " checks.</p>")
    );
  }

  function modelSwitcher(models, model, byDoc, hrefFor, chipText) {
    chipText =
      chipText ||
      function (m, r) {
        return r && r.error ? "failed" : fixed(docScore(r), 1);
      };
    var chips = models
      .map(function (m) {
        var r = byDoc[m.model];
        return (
          '<a class="chip' + (m === model ? " on" : "") + '" href="' + esc(hrefFor(m)) + '"' + (m === model ? ' aria-current="true"' : "") +
          ' title="' + esc(m.model + (r && r.error ? " — failed: " + r.error : "")) + '">' +
          "<code>" + esc(m.model) + '</code><span class="chip-s">' + esc(chipText(m, r)) + "</span></a>"
        );
      })
      .join("");
    var options = models
      .map(function (m) {
        var r = byDoc[m.model];
        return (
          '<option value="' + esc(hrefFor(m)) + '"' + (m === model ? " selected" : "") + ">" +
          esc(m.model + " — " + chipText(m, r)) + "</option>"
        );
      })
      .join("");
    return (
      '<div class="models"><nav class="chips" aria-label="Models (m / M to cycle)">' + chips + "</nav>" +
      '<label class="model-select"><span class="sr-only">Model</span><select id="model-select">' + options + "</select></label></div>"
    );
  }

  function viewInspector(runInfo, slug, docId, params) {
    setNav("documents", runInfo.run_id);
    return Promise.all([getRun(runInfo.run_id), getManifest(runInfo.dataset.name)]).then(function (loaded) {
      var run = loaded[0];
      var manifest = loaded[1];
      var datasetName = runInfo.dataset.name;
      var entry = null;
      (manifest.documents || []).forEach(function (doc) {
        if (doc.id === docId) entry = doc;
      });
      if (!entry) throw new Error("Document " + docId + " is not in dataset " + datasetName);

      var models = byScore(run.models);
      var model = null;
      models.forEach(function (candidate) {
        if (slugOf(candidate.model) === slug || candidate.model === slug) model = candidate;
      });
      var canonical = function (m, values) {
        var next = {
          tab: params.get("tab"),
          diff: params.get("diff"),
          ov: params.get("ov"),
          page: params.get("page"),
          src: params.get("src"),
          cat: params.get("cat"),
          q: params.get("q"),
          rf: params.get("rf"),
          fmt: params.get("fmt"),
          ref: params.get("ref"),
        };
        Object.keys(values || {}).forEach(function (k) {
          next[k] = values[k];
        });
        return docHref(run.run_id, m.model, docId, next);
      };
      if (!model) {
        go(canonical(models[0]), true);
        return null;
      }

      var byDoc = recordsByDoc(run)[docId] || {};
      var rec = byDoc[model.model];
      var rulesDoc = isRulesDoc(entry);
      var inventory = (runInfo.outputs || {})[slugOf(model.model)] || { json: [], missing: [] };
      var list = docList(run, manifest, params);
      var pos = -1;
      list.forEach(function (d, i) {
        if (d.id === docId) pos = i;
      });
      var filters = { src: params.get("src"), cat: params.get("cat"), q: params.get("q") };
      // Sources whose licence forbids redistribution (tag `fetch-required`, e.g. OmniDocBench) ship
      // scores only: no input, truth or model output is served, so none of them is requested.
      var restricted = (entry.tags || []).indexOf("fetch-required") !== -1;
      var review = isReview(run);
      var reviewDoc = review ? run.reviewByDoc[docId] || {} : null;
      var hasTruth = !!entry.truth;
      // Blocks: the unified response's blocks, each linked to its box on the page (both ways).
      var blocksTab = !restricted && inventory.json.indexOf(docId) !== -1 ? [["blocks", "Blocks"]] : [];
      var tabs = restricted
        ? []
        : review
          ? blocksTab.concat([["output", "Output"], ["diff", "Diff"], ["compare", "Compare models"]], hasTruth ? [["truth", "Truth"]] : [])
          : rulesDoc
          ? [["rules", "Checks"], ["output", "Output"]].concat(blocksTab, [["compare", "Compare models"]])
          : [["diff", "Diff"], ["output", "Output"]].concat(blocksTab, [["truth", "Truth"], ["compare", "Compare models"]]);
      var tab = params.get("tab");
      if (!tabs.some(function (t) { return t[0] === tab; })) tab = tabs.length ? tabs[0][0] : null;
      var diffMode = params.get("diff") === "unified" ? "unified" : "split";
      var ovOn = params.get("ov") === "1";
      var page = parseInt(params.get("page") || "1", 10) || 1;
      var ruleFilter = params.get("rf") || "fail";
      var rawOutput = params.get("fmt") === "md";

      var datasetBase = url("data/datasets/" + datasetName + "/");
      var truthUrl = entry.truth && !restricted ? joinPath(datasetBase, entry.truth) : null;
      var rulesUrl = entry.rules ? joinPath(datasetBase, entry.rules) : null;
      var inputUrl = joinPath(datasetBase, entry.file);
      var previews = (entry.previews || (entry.preview ? [entry.preview] : [])).map(function (p) {
        return joinPath(datasetBase, p);
      });
      var isPdf = /\.pdf$/i.test(entry.file || "");
      var fetchCommand = "python -m benchmark.adapters " + (docId.split("/")[0] || datasetName);
      function predUrl(m) {
        return url("data/outputs/" + enc(run.run_id) + "/" + enc(slugOf(m.model)) + "/" + encPath(docId) + ".md");
      }
      function hasPred(m) {
        var inv = (runInfo.outputs || {})[slugOf(m.model)];
        return !inv || inv.missing.indexOf(docId) === -1;
      }
      function loadPred(m) {
        if (restricted) return Promise.resolve(null);
        return hasPred(m) ? getText(predUrl(m)).catch(function () { return null; }) : Promise.resolve(null);
      }
      var jsonUrl = url("data/outputs/" + enc(run.run_id) + "/" + enc(slugOf(model.model)) + "/" + encPath(docId) + ".json");
      var hasJson = !restricted && inventory.json.indexOf(docId) !== -1;

      // Review runs without a transcript diff against another model: the one named by `ref`, else
      // the model that agrees most with the others on this document.
      var refModel = null;
      var refChoices = [];
      if (review && !hasTruth) {
        refChoices = models.filter(function (m) {
          return m !== model && hasPred(m) && !(byDoc[m.model] || {}).error;
        });
        refChoices.forEach(function (m) {
          if (slugOf(m.model) === params.get("ref") || m.model === params.get("ref")) refModel = m;
        });
        if (!refModel) {
          var agree = reviewDoc.agreement || {};
          refModel = refChoices.slice().sort(function (a, b) {
            return (agree[b.model] || 0) - (agree[a.model] || 0);
          })[0] || null;
        }
      }

      var needTruth = (review ? hasTruth : !rulesDoc) && (tab === "diff" || tab === "truth" || tab === "compare");
      return Promise.all([
        truthUrl && needTruth ? getText(truthUrl).catch(function () { return null; }) : Promise.resolve(null),
        tab === "compare" || !tab ? Promise.resolve(null) : loadPred(model),
        rulesUrl && !restricted ? getRules(rulesUrl) : Promise.resolve(null),
        hasJson ? getJSON(jsonUrl).catch(function () { return null; }) : Promise.resolve(null),
        tab === "compare" ? Promise.all(models.map(loadPred)) : Promise.resolve(null),
        tab === "diff" && refModel ? loadPred(refModel) : Promise.resolve(null),
      ]).then(function (got) {
        var truth = got[0];
        var pred = got[1];
        var ruleList = got[2];
        var unified = got[3];
        var allPreds = got[4];
        var refPred = got[5];
        var baseLabel = "Truth";
        if (review && !hasTruth) {
          truth = refPred;
          baseLabel = refModel ? refModel.model : "Reference";
        }
        if (tab === "compare" && allPreds) pred = allPreds[models.indexOf(model)];
        var opts = run.normalize || {};
        var title = docTitle(entry);
        var srcKey = sourceOf(docId, datasetName);
        var srcName = entry.source_label || srcLabel(srcKey);
        document.title = title + (review ? "" : " · " + srcName) + " · " + model.model + " · " + APP_NAME;

        /* ---- the result body of the current tab */
        var body = "";
        if (tab === "diff") {
          if (pred === null) {
            body = '<p class="empty">No saved output for <code>' + esc(model.model) + "</code> on this document" +
              (rec && rec.error ? ": the call failed." : ".") + "</p>";
          } else if (truth === null) {
            body =
              review && !hasTruth
                ? '<p class="empty">No other model has an output for this document to compare with.</p>'
                : '<p class="empty">The ground truth could not be loaded.</p>';
          } else {
            var d = wordDiff(stripHtmlTags(truth), stripHtmlTags(pred));
            body =
              (review && !hasTruth
                ? '<div class="tool-row"><label class="ref-pick">Compared with <select id="ref-select">' +
                  refChoices
                    .map(function (m) {
                      return '<option value="' + esc(slugOf(m.model)) + '"' + (m === refModel ? " selected" : "") + ">" + esc(m.model) + "</option>";
                    })
                    .join("") +
                  "</select></label></div>"
                : "") +
              '<div class="tool-row"><div class="seg" role="group" aria-label="Diff layout">' +
              '<button type="button" data-diff="split" aria-pressed="' + (diffMode === "split") + '">Side by side</button>' +
              '<button type="button" data-diff="unified" aria-pressed="' + (diffMode === "unified") + '">Unified</button></div>' +
              '<span class="stats">' + d.eq + " words match · <del>" + d.del + " missing</del> · <ins>" + d.ins + " extra</ins></span></div>" +
              (rec && rec.table_only
                ? '<p class="note">Table-only document: the score counts table rows only; the diff shows the whole text.</p>'
                : "") +
              (diffMode === "unified"
                ? '<div class="pane" tabindex="0" role="region" aria-label="Unified word diff">' + renderUnified(d) + "</div>"
                : '<div class="split"><div><div class="pane-head">' + esc(baseLabel) + "</div>" +
                  '<div class="pane" tabindex="0" role="region" aria-label="' + esc(baseLabel) + ', missing words struck through">' + renderSide(d.t, d.tMarks, "del") + "</div></div>" +
                  '<div><div class="pane-head">Output</div>' +
                  '<div class="pane" tabindex="0" role="region" aria-label="Output, extra words underlined">' + renderSide(d.p, d.pMarks, "ins") + "</div></div></div>") +
              (review && !hasTruth
                ? '<p class="caption">Struck through: words ' + esc(baseLabel) + " has that this output lacks. Underlined: words only this output has. " +
                  "A difference shows where to look on the page; it does not say which model is right.</p>"
                : '<p class="caption">Struck through: in the truth but missing from the output. Underlined: extra in the output. Words are compared after the scorer\'s normalisation.</p>');
          }
        } else if (tab === "rules") {
          var scored = pred !== null && ruleList ? scoreRules(pred, ruleList, opts) : null;
          body = rulesChecklist(ruleList, scored, rec, ruleFilter, RULE_PAGE);
        } else if (tab === "compare") {
          body =
            '<div class="compare">' +
            models
              .map(function (m, i) {
                var p = allPreds ? allPreds[i] : null;
                var r = byDoc[m.model];
                var inner;
                var sub = "";
                if (p === null) inner = '<p class="empty">' + (r && r.error ? "The call failed." : "No saved output.") + "</p>";
                else if (rulesDoc) {
                  if (ruleList) {
                    var sc = scoreRules(p, ruleList, opts);
                    sub = sc.passed + " of " + plural(sc.total, "check") + " pass";
                  }
                  inner = esc(p);
                } else if (truth !== null) {
                  var dd = wordDiff(truth, p);
                  sub = dd.del + " missing · " + dd.ins + " extra";
                  inner = renderSide(dd.p, dd.pMarks, "ins");
                } else inner = rawOutput ? esc(p) : renderMarkdown(p);
                if (review && p !== null && isNum((reviewDoc.agreement || {})[m.model])) {
                  sub = pct(reviewDoc.agreement[m.model]) + "% agreement" + ((reviewDoc.outliers || []).indexOf(m.model) !== -1 ? " · outlier" : "");
                }
                return (
                  '<div class="col' + (m === model ? " on" : "") + '"><div class="col-head"><a href="' + esc(canonical(m, { tab: "compare" })) + '" title="' + esc(m.model) + '">' +
                  esc(m.model) + "</a>" + (review ? verdictChip(reviewDoc, m.model) : scoreHtml(docScore(r), 1)) + "</div>" +
                  (sub ? '<div class="col-sub">' + esc(sub) + "</div>" : "") +
                  (p === null
                    ? inner
                    : '<div class="pane' + (truth === null && !rulesDoc && !rawOutput ? " md" : "") + '" tabindex="0" role="region" aria-label="' + esc(m.model) + ' output">' + inner + "</div>") +
                  "</div>"
                );
              })
              .join("") +
            "</div>" +
            (truth === null && !rulesDoc
              ? '<div class="tool-row">' + outputFormatSwitch(rawOutput) + "</div>"
              : '<p class="caption">' + (rulesDoc ? "Pass counts are re-checked in your browser." : "Underlined words are not in the truth.") + "</p>");
        } else if (tab === "truth") {
          body =
            truth === null
              ? '<p class="empty">The ground truth could not be loaded.</p>'
              : '<div><div class="pane-head"><span>Markdown source the metrics compare against</span><a href="' + esc(truthUrl) + '" rel="noopener">raw</a></div>' +
                '<div class="pane" tabindex="0" role="region" aria-label="Ground truth markdown">' + esc(truth) + "</div></div>";
        } else if (tab === "blocks") {
          body = blocksBody(unified ? blocksByPage(unified) : null, model);
        } else if (tab === "output") {
          body =
            pred === null
              ? '<p class="empty">No saved output' + (rec && rec.error ? ": the call failed." : ".") + "</p>"
              : '<div class="tool-row">' + outputFormatSwitch(rawOutput) + "</div>" +
                '<div><div class="pane-head"><span class="model-id">' + esc(model.model) + '</span><a href="' + esc(predUrl(model)) + '" rel="noopener">raw</a></div>' +
                (rawOutput
                  ? '<div class="pane" tabindex="0" role="region" aria-label="Model output">' + esc(pred) + "</div></div>"
                  : '<div class="pane md" tabindex="0" role="region" aria-label="Model output, rendered">' + renderMarkdown(pred) + "</div></div>");
        } else {
          body =
            '<p class="note">This source\'s licence is research-only, so PuffinParse publishes the recorded scores but not the page, ' +
            "its ground truth or the model outputs. Fetch the data at the pinned revision to inspect it locally:</p>" + codeBlock(fetchCommand);
        }

        /* ---- reproduce */
        var repoBase = "benchmark/datasets/" + datasetName + "/";
        var predFile = docId.replace(/\//g, "_") + ".pred.md";
        var parseCommand = "puffinparse parse " + joinPath(repoBase, entry.file) + " -m " + model.model + " > " + predFile;
        var scoreCommand = rulesDoc
          ? "puffinparse bench run --dataset benchmark/datasets/" + datasetName + " \\\n    --models " + model.model + " --filter " + docId
          : "puffinparse bench score " + predFile + " " + joinPath(repoBase, entry.truth);
        var served = [];
        if (!restricted) {
          if (rulesDoc && rulesUrl) served.push('<a href="' + esc(rulesUrl) + '" rel="noopener">checks</a>');
          if (!rulesDoc && truthUrl) served.push('<a href="' + esc(truthUrl) + '" rel="noopener">truth</a>');
          if (hasPred(model)) served.push('<a href="' + esc(predUrl(model)) + '" rel="noopener">output</a>');
          if (hasJson) served.push('<a href="' + esc(jsonUrl) + '" rel="noopener">unified JSON</a>');
          served.push('<a href="' + esc(inputUrl) + '" rel="noopener">input</a>');
        }
        served.push('<a href="' + esc(url("data/runs/" + run.run_id + ".json")) + '" rel="noopener">run JSON</a>');
        if (review) {
          var localFile = joinPath(String(run.dataset.root || ".").replace(/\/?$/, "/"), docId);
          served = [];
          if (hasPred(model)) served.push('<a href="' + esc(predUrl(model)) + '" rel="noopener">output</a>');
          if (hasJson) served.push('<a href="' + esc(jsonUrl) + '" rel="noopener">unified JSON</a>');
          served.push('<a href="' + esc(inputUrl) + '" rel="noopener">input</a>');
        }
        var reproduce = review
          ? disclosure(
              "Parse it again",
              '<p class="note">The same call from the command line:</p>' +
                codeBlock("puffinparse parse " + JSON.stringify(localFile) + " -m " + model.model) +
                '<p class="links">Files behind this page: ' + served.join(" · ") + "</p>",
              { id: "doc-repro", cls: "reproduce" }
            )
          : disclosure(
          "Reproduce this score",
          (restricted ? '<p class="note">After fetching the data:</p>' : "") +
            '<p class="note">' + (rulesDoc ? "Parse the page with the same model, then re-run its checks:" : "Parse the page with the same model and score it locally:") + "</p>" +
            codeBlock(parseCommand) + codeBlock(scoreCommand) +
            '<p class="links">Files behind this page: ' + served.join(" · ") + "</p>",
          { id: "doc-repro", cls: "reproduce" }
        );

        /* ---- header */
        var prev = pos > 0 ? list[pos - 1] : null;
        var next = pos >= 0 && pos < list.length - 1 ? list[pos + 1] : null;
        function docLink(d) {
          return docHref(run.run_id, model.model, d.id, { tab: params.get("tab"), diff: params.get("diff"), ov: params.get("ov"), rf: params.get("rf"), src: filters.src, cat: filters.cat, q: filters.q });
        }
        var rulesTotal = ruleList ? ruleList.length : (rec && rec.metrics && rec.metrics.rules_total);
        var metaBits = [plural(entry.pages || 1, "page")];
        if (review) metaBits.push(hasTruth ? "has a reference transcript" : "no ground truth: compare models and check the page");
        else if (rulesDoc) {
          if (isNum(rulesTotal)) metaBits.push(plural(rulesTotal, "check"));
        } else metaBits.push(rec && rec.table_only ? "scored on its table" : "scored against a transcript");
        if (entry.license) metaBits.push(entry.license);
        else if (manifest.license && !/mixed/i.test(manifest.license)) metaBits.push(manifest.license);

        var ordinal = entry.ordinal;
        var h1 = entry.title && ordinal
          ? esc(entry.category_label || catLabel(entry.category)) + ' <span class="ord">' + esc(ordinal) + "</span>"
          : esc(title);
        var detailRows = review
          ? '<dt>Document</dt><dd><span class="idline"><code>' + esc(docId) + '</code><button type="button" class="copy-btn inline" data-copy="' + esc(docId) + '">Copy</button></span></dd>' +
            '<dt>Folder</dt><dd class="mono">' + esc(run.dataset.root || "") + "</dd>" +
            "<dt>Run</dt><dd class=\"mono\">" + esc(run.run_id) + "</dd>"
          : '<dt>Document id</dt><dd><span class="idline"><code>' + esc(docId) + '</code><button type="button" class="copy-btn inline" data-copy="' + esc(docId) + '">Copy</button></span></dd>' +
          "<dt>Source</dt><dd>" + esc(srcName) + " · " + esc(entry.category_label || catLabel(entry.category)) + " · dataset " + esc(datasetName) + " v" + esc(manifest.version) + "</dd>" +
          "<dt>Scored by</dt><dd>" + (rulesDoc ? "checks (machine-checkable assertions)" : rec && rec.table_only ? "table score (table rows only)" : "character similarity to the transcript") + "</dd>" +
          "<dt>Input</dt><dd class=\"mono\">" + esc(pathLabel(entry.file)) + "</dd>" +
          (entry.source_url ? '<dt>Original</dt><dd><a href="' + esc(entry.source_url) + '" rel="noopener noreferrer">' + esc(entry.source_url) + "</a></dd>" : "") +
          ((entry.tags || []).length
            ? '<dt>Tags</dt><dd><span class="tag-list">' + entry.tags.map(function (t) { return "<span>" + esc(t) + "</span>"; }).join("") + "</span></dd>"
            : "") +
          (entry.license ? "<dt>Licence</dt><dd>" + esc(entry.license) + "</dd>" : "") +
          (entry.attribution ? "<dt>Attribution</dt><dd>" + esc(entry.attribution) + "</dd>" : "");

        /* ---- summary: the one number */
        var score = docScore(rec);
        var aside = [];
        if (rec && isNum(rec.latency_ms)) aside.push('<span title="Client-measured latency, caches disabled">' + esc(msFmt(rec.latency_ms)) + "</span>");
        if (rec && isNum(rec.cost_usd)) aside.push('<span title="List price for this document">$' + esc(rec.cost_usd.toFixed(4)) + "</span>");

        var html =
          '<nav class="crumbs" aria-label="Breadcrumb"><a href="' + esc(listHref("documents", run.run_id, filters)) + '">Documents</a>' +
          (review ? "" : '<span aria-hidden="true">›</span><a href="' + esc(listHref("documents", run.run_id, { src: srcKey })) + '">' + esc(srcName) + "</a>") + "</nav>" +
          '<div class="doc-head"><h1>' + h1 + "</h1>" +
          '<div class="doc-nav">' +
          (prev ? '<a class="iconbtn" href="' + esc(docLink(prev)) + '" aria-label="Previous document: ' + esc(docTitle(prev)) + ' (k)" title="' + esc(docTitle(prev)) + ' (k)">‹</a>' : '<span class="iconbtn" aria-disabled="true">‹</span>') +
          '<span class="pos">' + (pos + 1) + "/" + list.length + "</span>" +
          (next ? '<a class="iconbtn" href="' + esc(docLink(next)) + '" aria-label="Next document: ' + esc(docTitle(next)) + ' (j)" title="' + esc(docTitle(next)) + ' (j)">›</a>' : '<span class="iconbtn" aria-disabled="true">›</span>') +
          '<button type="button" class="iconbtn" id="copy-link" aria-label="Copy a link to this view" title="Copy a link to this view">' +
          '<svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round">' +
          '<path d="M6.5 9.5l3-3M7 4.5l1.2-1.2a2.6 2.6 0 0 1 3.7 3.7L10.7 8.2M9 11.5l-1.2 1.2a2.6 2.6 0 0 1-3.7-3.7L5.3 7.8"/></svg></button>' +
          "</div></div>" +
          '<p class="doc-meta">' + esc(metaBits.join(" · ")) + "</p>" +
          (review ? reviewProgress(run, manifest, params) : "") +
          disclosure("Details", '<dl class="kv">' + detailRows + "</dl>", { id: "doc-details", cls: "doc-details" }) +
          modelSwitcher(
            models,
            model,
            byDoc,
            function (m) {
              return canonical(m);
            },
            review
              ? function (m, r) {
                  if (r && r.missing) return "not run";
                  if (r && r.error) return "failed";
                  var v = ((reviewDoc.reviews || {})[m.model] || {}).verdict;
                  if (v) return VERDICT_LABELS[v] || v;
                  var a = (reviewDoc.agreement || {})[m.model];
                  return isNum(a) ? pct(a) + "%" : "—";
                }
              : null
          ) +
          '<div class="insp rv" id="rv">' +
          '<section class="pagecol" aria-label="Page"><div id="sv-slot"></div></section>' +
          '<div class="rv-split" role="separator" aria-orientation="vertical" aria-label="Resize the page and the output" tabindex="0" title="Drag to resize (← →)"></div>' +
          '<section class="work" aria-label="Evidence"><div class="rv-scroll" id="rv-scroll">' +
          (review
            ? '<section class="summary" aria-label="This output">' + reviewSummary(run, model, rec, reviewDoc) + "</section>"
            : '<section class="summary" aria-label="Score">' +
          '<div class="hero bbox"><div class="hero-n' + (verdictOf(score) ? " v-" + verdictOf(score) : "") + '">' + vdot(score) +
          '<span class="n">' + esc(fixed(score, 1)) + '</span><span class="of">/ 100</span></div></div>' +
          '<p class="verdict">' + esc(verdictLine(rec, rulesTotal)) + "</p>" +
          (rec && rec.error ? '<p class="failnote">' + esc(rec.error) + "</p>" : "") +
          (aside.length ? '<p class="aside">' + aside.join(" · ") + "</p>" : "") +
          disclosure("All metrics", metricsList(rec), { id: "doc-metrics" }) +
          "</section>") +
          (tabs.length
            ? '<div class="tabs" role="tablist">' +
              tabs
                .map(function (t, i) {
                  return '<a role="tab" class="tab" href="' + esc(canonical(model, { tab: t[0] })) + '" aria-selected="' + (t[0] === tab) + '" title="' + t[1] + " (" + (i + 1) + ')">' + esc(t[1]) + "</a>";
                })
                .join("") +
              "</div>"
            : "") +
          '<div class="tab-body" role="' + (tabs.length ? "tabpanel" : "region") + '">' + body + "</div>" +
          reproduce +
          "</div>" +
          (review ? verdictBar(run, model, rec, reviewDoc) : "") +
          "</section></div>";

        view.innerHTML = html;
        var chipRow = view.querySelector(".chips");
        var onChip = chipRow.querySelector(".chip.on");
        if (onChip && onChip.offsetLeft + onChip.offsetWidth > chipRow.clientWidth) {
          chipRow.scrollLeft = onChip.offsetLeft - chipRow.offsetLeft - 16;
        }

        /* ---- page viewer: reuse across model / tab switches */
        var svKey = run.run_id + "|" + docId;
        if (!SV || SV.key !== svKey) {
          SV = new SourceViewer(svKey, {
            docId: docId,
            title: title,
            inputUrl: inputUrl,
            previews: isPdf ? previews : [],
            imageUrl: isPdf ? null : previews[0] || inputUrl,
            isPdf: isPdf,
            pages: entry.pages || 1,
            restricted: restricted,
          });
        }
        document.getElementById("sv-slot").appendChild(SV.el);
        SV.onPage = function (n) {
          go(canonical(model, { page: n > 1 ? String(n) : null }), true);
        };
        SV.onOverlay = function (on) {
          go(canonical(model, { ov: on ? "1" : null }), true);
        };
        SV.overlayOn = ovOn;
        SV.linked = tab === "blocks";
        SV.active = null;
        if (page !== SV.page) SV.showPage(page);
        var overlay = unified ? blocksByPage(unified) : null;
        SV.setOverlay(overlay, overlay && overlay.count ? plural(overlay.count, "layout box", "layout boxes") + " from the unified response of " + model.model : "");
        SV.onBoxClick = function (pg, i) {
          if (tab !== "blocks") {
            go(canonical(model, { tab: "blocks", b: pg + "." + i }), true);
            return;
          }
          activateBlock(pg, i, false);
        };
        function activateBlock(pg, i, fromCard) {
          view.querySelectorAll(".blk.on").forEach(function (c) {
            c.classList.remove("on");
          });
          var card = document.getElementById("blk-" + pg + "-" + i);
          if (card) {
            card.classList.add("on");
            if (!fromCard) card.scrollIntoView({ block: "nearest", behavior: reducedMotion() ? "auto" : "smooth" });
          }
          SV.setActive(pg, i);
        }
        if (tab === "blocks") {
          view.querySelectorAll(".blk").forEach(function (card) {
            var pg = Number(card.getAttribute("data-p"));
            var i = Number(card.getAttribute("data-i"));
            var on = function () {
              activateBlock(pg, i, true);
            };
            card.addEventListener("mouseenter", on);
            card.addEventListener("focus", on);
          });
          var wanted = /^(\d+)\.(\d+)$/.exec(params.get("b") || "");
          if (wanted) activateBlock(Number(wanted[1]), Number(wanted[2]), false);
        }
        wireSplit(view.querySelector("#rv"));
        fitWorkspace();

        /* ---- wiring */
        wireCopyButtons(view);
        wireDisclosures(view);
        view.querySelector("#copy-link").addEventListener("click", function (e) {
          var button = e.currentTarget;
          var done = function () {
            button.setAttribute("title", "Copied");
            button.style.color = "var(--good)";
            setTimeout(function () {
              button.setAttribute("title", "Copy a link to this view");
              button.style.color = "";
            }, 1500);
          };
          if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(location.href).then(done, function () {});
        });
        var modelSelect = view.querySelector("#model-select");
        modelSelect.addEventListener("change", function () {
          go(modelSelect.value, true);
        });
        view.querySelectorAll("button[data-diff]").forEach(function (b) {
          b.addEventListener("click", function () {
            go(canonical(model, { diff: b.getAttribute("data-diff") === "split" ? null : "unified" }), true);
          });
        });
        if (tab === "rules") wireRuleBody();
        view.querySelectorAll("button[data-fmt]").forEach(function (b) {
          b.addEventListener("click", function () {
            go(canonical(model, { fmt: b.getAttribute("data-fmt") === "md" ? "md" : null }), true);
          });
        });
        var refSelect = view.querySelector("#ref-select");
        if (refSelect)
          refSelect.addEventListener("change", function () {
            go(canonical(model, { ref: refSelect.value }), true);
          });
        if (review) wireVerdictBar(run, manifest, model, docId, rec);
        function wireRuleBody() {
          view.querySelectorAll(".tab-body button[data-rf]").forEach(function (b) {
            b.addEventListener("click", function () {
              var target = b.getAttribute("data-rf");
              go(canonical(model, { rf: target === "fail" ? null : target }), true);
            });
          });
          var again = view.querySelector(".more-rules");
          if (again)
            again.addEventListener("click", function () {
              var n = view.querySelectorAll(".check").length + RULE_PAGE;
              view.querySelector(".tab-body").innerHTML = rulesChecklist(ruleList, scoreRules(pred, ruleList, opts), rec, ruleFilter, n);
              wireRuleBody();
            });
        }

        var idx = models.indexOf(model);
        KEYS = {
          j: function () {
            if (next) go(docLink(next));
          },
          k: function () {
            if (prev) go(docLink(prev));
          },
          m: function () {
            go(canonical(models[(idx + 1) % models.length]), true);
          },
          M: function () {
            go(canonical(models[(idx - 1 + models.length) % models.length]), true);
          },
          d: function () {
            if (tab === "diff") go(canonical(model, { diff: diffMode === "split" ? "unified" : null }), true);
          },
          o: function () {
            if (SV.overlay) SV.onOverlay(!SV.overlayOn);
          },
          n: function () {
            if (review) goNextUnreviewed(run, manifest, docId, model.model, true);
          },
          e: function () {
            var notes = document.getElementById("review-notes");
            if (notes) notes.focus();
          },
          "[": function () {
            SV.onPage(SV.page - 1);
          },
          "]": function () {
            SV.onPage(SV.page + 1);
          },
        };
        tabs.forEach(function (t, i) {
          KEYS[String(i + 1)] = function () {
            go(canonical(model, { tab: t[0] }), true);
          };
        });
        return "doc|" + run.run_id + "|" + docId;
      });
    });
  }

  /* ------------------------------------------------------- safe Markdown rendering
   * Model output is untrusted text. Everything is escaped; Markdown structure is rebuilt from
   * scratch, and the only HTML kept is a short list of tags providers really emit (emphasis,
   * sub/superscript, line breaks, tables) with every attribute dropped except a numeric
   * colspan/rowspan. Links keep only http(s) targets; images are never loaded. The same renderer
   * as website/assets/playground.js: keep the two in sync. */

  var MD_INLINE_TAGS = { b: 1, strong: 1, i: 1, em: 1, u: 1, s: 1, del: 1, sup: 1, sub: 1, code: 1, br: 1, mark: 1, small: 1 };
  var MD_TABLE_TAGS = { table: 1, thead: 1, tbody: 1, tfoot: 1, tr: 1, td: 1, th: 1, caption: 1 };
  var MD_TAG_RE = /<\/?[a-zA-Z][a-zA-Z0-9]*(?:\s[^<>]*)?\/?>/g;
  var MD_FENCE = /^\s{0,3}(`{3,}|~{3,})/;
  var MD_HEADING = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
  var MD_RULE = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;
  var MD_LIST = /^\s{0,3}([-*+]|\d{1,9}[.)])\s+(.*)$/;
  var MD_QUOTE = /^\s{0,3}>\s?(.*)$/;
  var MD_DELIM = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

  function mdCleanTag(raw, allowTable) {
    var m = /^<(\/?)([a-zA-Z][a-zA-Z0-9]*)([^>]*)>$/.exec(raw);
    if (!m) return "";
    var name = m[2].toLowerCase();
    if (!MD_INLINE_TAGS[name] && !(allowTable && MD_TABLE_TAGS[name])) return ""; // dropped, text kept
    if (name === "br") return m[1] ? "" : "<br>";
    if (m[1]) return "</" + name + ">";
    var attrs = "";
    if (name === "td" || name === "th") {
      var span = /\b(colspan|rowspan)\s*=\s*["']?(\d{1,3})\b/gi;
      var a;
      while ((a = span.exec(m[3]))) attrs += " " + a[1].toLowerCase() + '="' + a[2] + '"';
    }
    return "<" + name + attrs + ">";
  }

  /** Inline Markdown on one escaped text run: code spans, links, images, bold, italic. */
  function mdInlineText(text) {
    var codes = [];
    var s = esc(text).replace(/`([^`]+)`/g, function (_, c) {
      codes.push("<code>" + c + "</code>");
      return "\u0000" + (codes.length - 1) + "\u0000";
    });
    s = s.replace(/!\[([^\]]*)\]\(([^)\s]*)[^)]*\)/g, function (_, alt) {
      return '<span class="md-img">[image' + (alt ? ": " + alt : "") + "]</span>";
    });
    s = s.replace(/\[([^\]]+)\]\(([^)\s]+)[^)]*\)/g, function (_, label, href) {
      return /^https?:\/\//i.test(href.replace(/&amp;/g, "&"))
        ? '<a href="' + href + '" rel="nofollow noopener noreferrer" target="_blank">' + label + "</a>"
        : label;
    });
    s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>").replace(/__([^_]+)__/g, "<strong>$1</strong>");
    s = s.replace(/(^|[^*\w])\*([^*\s][^*]*?)\*(?!\w)/g, "$1<em>$2</em>");
    s = s.replace(/(^|[^_\w])_([^_\s][^_]*?)_(?!\w)/g, "$1<em>$2</em>");
    return s.replace(/\u0000(\d+)\u0000/g, function (_, i) {
      return codes[+i];
    });
  }

  /** Text with inline HTML: whitelisted tags are rebuilt, everything else is escaped text. */
  function mdInline(text, allowTable) {
    var out = "";
    var last = 0;
    var s = String(text);
    MD_TAG_RE.lastIndex = 0;
    var m;
    while ((m = MD_TAG_RE.exec(s))) {
      out += mdInlineText(s.slice(last, m.index)) + mdCleanTag(m[0], allowTable);
      last = m.index + m[0].length;
    }
    return out + mdInlineText(s.slice(last));
  }

  function mdSplitRow(line) {
    var s = line.trim().replace(/\\\|/g, "\u0001").replace(/^\|/, "").replace(/\|$/, "");
    return s.split("|").map(function (c) {
      return c.trim().replace(/\u0001/g, "|");
    });
  }

  function mdStartsBlock(line, next) {
    return (
      MD_FENCE.test(line) || MD_HEADING.test(line) || MD_RULE.test(line) || MD_LIST.test(line) || MD_QUOTE.test(line) ||
      /^\s*<table\b/i.test(line) || (line.indexOf("|") !== -1 && MD_DELIM.test(next || ""))
    );
  }

  function renderMarkdown(md, depth) {
    depth = depth || 0;
    var lines = String(md == null ? "" : md).replace(/\r\n?/g, "\n").replace(/<!--[\s\S]*?-->/g, "").split("\n");
    var out = [];
    var i = 0;
    while (i < lines.length) {
      var line = lines[i];
      if (!line.trim()) {
        i++;
        continue;
      }
      var m;
      if ((m = MD_FENCE.exec(line))) {
        var fence = m[1];
        var body = [];
        i++;
        while (i < lines.length && lines[i].trim().indexOf(fence) !== 0) body.push(lines[i++]);
        i++;
        out.push("<pre><code>" + esc(body.join("\n")) + "</code></pre>");
        continue;
      }
      if (/^\s*<table\b/i.test(line)) {
        var block = [];
        while (i < lines.length) {
          block.push(lines[i]);
          if (/<\/table>/i.test(lines[i++])) break;
        }
        out.push('<div class="md-tablewrap">' + mdInline(block.join("\n"), true) + "</div>");
        continue;
      }
      if ((m = MD_HEADING.exec(line))) {
        var level = Math.min(m[1].length + 2, 6); // the page's own headings stop at h2
        out.push("<h" + level + ">" + mdInline(m[2]) + "</h" + level + ">");
        i++;
        continue;
      }
      if (MD_RULE.test(line)) {
        out.push("<hr>");
        i++;
        continue;
      }
      if (line.indexOf("|") !== -1 && MD_DELIM.test(lines[i + 1] || "")) {
        var head = mdSplitRow(line);
        var rows = [];
        i += 2;
        while (i < lines.length && lines[i].indexOf("|") !== -1 && lines[i].trim()) rows.push(mdSplitRow(lines[i++]));
        out.push(
          '<div class="md-tablewrap"><table><thead><tr>' +
            head.map(function (c) { return "<th>" + mdInline(c) + "</th>"; }).join("") +
            "</tr></thead><tbody>" +
            rows
              .map(function (r) {
                return "<tr>" + r.map(function (c) { return "<td>" + mdInline(c) + "</td>"; }).join("") + "</tr>";
              })
              .join("") +
            "</tbody></table></div>"
        );
        continue;
      }
      if (MD_QUOTE.test(line)) {
        var quoted = [];
        while (i < lines.length && (m = MD_QUOTE.exec(lines[i]))) {
          quoted.push(m[1]);
          i++;
        }
        out.push("<blockquote>" + (depth < 3 ? renderMarkdown(quoted.join("\n"), depth + 1) : esc(quoted.join(" "))) + "</blockquote>");
        continue;
      }
      if ((m = MD_LIST.exec(line))) {
        var ordered = /\d/.test(m[1]);
        var items = [];
        while (i < lines.length) {
          var lm = MD_LIST.exec(lines[i]);
          if (lm) {
            items.push(lm[2]);
            i++;
            continue;
          }
          if (lines[i].trim() && /^\s{2,}/.test(lines[i]) && items.length) {
            items[items.length - 1] += " " + lines[i].trim();
            i++;
            continue;
          }
          break;
        }
        var tag = ordered ? "ol" : "ul";
        out.push("<" + tag + ">" + items.map(function (t) { return "<li>" + mdInline(t) + "</li>"; }).join("") + "</" + tag + ">");
        continue;
      }
      var para = [line];
      i++;
      while (i < lines.length && lines[i].trim() && !mdStartsBlock(lines[i], lines[i + 1])) para.push(lines[i++]);
      out.push("<p>" + mdInline(para.join("\n")).replace(/\n/g, " ") + "</p>");
    }
    return out.join("\n") || '<p class="empty">Empty output.</p>';
  }

  function outputFormatSwitch(raw) {
    return (
      '<div class="seg" role="group" aria-label="Output format">' +
      '<button type="button" data-fmt="rendered" aria-pressed="' + !raw + '">Rendered</button>' +
      '<button type="button" data-fmt="md" aria-pressed="' + raw + '">Markdown</button></div>'
    );
  }

  /* ------------------------------------------------------------- review mode */

  var VERDICT_LABELS = { correct: "Correct", minor_errors: "Minor errors", major_errors: "Major errors", unusable: "Unusable" };
  var VERDICT_TONE = { correct: "good", minor_errors: "fair", major_errors: "poor", unusable: "poor" };

  function verdictChip(reviewDoc, model) {
    var r = ((reviewDoc && reviewDoc.reviews) || {})[model];
    if (!r) return '<span class="vchip none">unchecked</span>';
    return (
      '<span class="vchip v-' + esc(VERDICT_TONE[r.verdict] || "fair") + '" title="' + esc("Checked by " + r.by + (r.notes ? ": " + r.notes : "")) + '">' +
      esc(VERDICT_LABELS[r.verdict] || r.verdict) + "</span>"
    );
  }

  function pctOf(x) {
    return isNum(x) ? pct(x) + "%" : "—";
  }

  function statusLabel(status) {
    return String(status || "").replace(/_/g, " ");
  }

  /** The top of the output column for a review run: agreement and what the call returned. */
  function reviewSummary(run, model, rec, reviewDoc) {
    var agree = (reviewDoc.agreement || {})[model.model];
    var outlier = (reviewDoc.outliers || []).indexOf(model.model) !== -1;
    var line;
    if (rec && rec.missing) line = "This call was not made: resume the run to finish it.";
    else if (rec && rec.error) line = "The call failed.";
    else if (rec && rec.empty) line = "The model returned no text.";
    else if (isNum(agree)) line = "Shares " + pct(agree) + "% of its words with the other models" + (outlier ? ", the least on this document." : ".");
    else line = "Only one model read this document, so there is nothing to compare it with.";
    var aside = [];
    if (rec && isNum(rec.latency_ms) && !rec.missing) aside.push("<span>" + esc(msFmt(rec.latency_ms)) + "</span>");
    if (rec && isNum(rec.cost_usd)) aside.push('<span title="What this call cost">$' + esc(rec.cost_usd.toFixed(4)) + "</span>");
    if (rec && isNum(rec.pages) && rec.pages) aside.push("<span>" + esc(plural(rec.pages, "page")) + " billed</span>");
    var measured = function (x) {
      return isNum(x) ? pct(x) + "%" : "not measured";
    };
    return (
      '<div class="rv-sum"><div class="hero-n' + (isNum(agree) ? " v-" + (agree >= 0.9 ? "good" : agree >= 0.7 ? "fair" : "poor") : "") + '">' +
      '<span class="n">' + esc(isNum(agree) ? pct(agree) : "—") + '</span><span class="of">% agreement</span></div>' +
      '<div class="rv-sum-text"><p class="verdict">' + esc(line) + "</p>" +
      (aside.length ? '<p class="aside">' + aside.join(" · ") + "</p>" : "") +
      "</div></div>" +
      (rec && rec.error ? '<p class="failnote">' + esc(rec.error) + "</p>" : "") +
      disclosure(
        "This document",
        '<dl class="metrics">' +
          [
            ["Disagreement", measured(reviewDoc.disagreement), "1 − mean word agreement between the models that read it"],
            ["On numbers", measured(reviewDoc.numbers_disagreement), "The same, counting only tokens with digits: amounts, dates, ids"],
            ["Characters", rec && isNum(rec.chars) ? String(rec.chars) : "not measured", "Length of this model's output"],
            ["Failed", String((reviewDoc.failed || []).length), (reviewDoc.failed || []).join(", ")],
          ]
            .map(function (it) {
              return '<div title="' + esc(it[2]) + '"><dt>' + esc(it[0]) + "</dt><dd>" + esc(it[1]) + "</dd></div>";
            })
            .join("") +
          "</dl>" +
          '<p class="caption">Agreement is not accuracy: models can agree on the same mistake. It shows where to look first.</p>',
        { id: "doc-review-numbers" }
      )
    );
  }

  function autoNext() {
    try {
      return localStorage.getItem("puffinparse-auto-next") !== "0";
    } catch (e) {
      return true;
    }
  }

  /** The verdict bar under the output: always in reach, one key per verdict, saved at once. */
  function verdictBar(run, model, rec, reviewDoc) {
    if (!rec || rec.missing) {
      return '<div class="vbar"><p class="caption">This call was not made, so there is nothing to check. Resume the run to finish it.</p></div>';
    }
    var review = (reviewDoc.reviews || {})[model.model];
    var verdicts = run.review.verdicts || Object.keys(VERDICT_LABELS);
    return (
      '<form class="vbar" id="review-form" aria-label="Your check">' +
      '<div class="vbar-row"><span class="vbar-l">Your check</span><div class="verdicts" role="group" aria-label="Verdict">' +
      verdicts
        .map(function (v, i) {
          return (
            '<button type="button" class="vbtn v-' + esc(VERDICT_TONE[v] || "fair") + '" data-verdict="' + esc(v) + '" aria-pressed="' +
            (!!review && review.verdict === v) + '" title="v then ' + (i + 1) + '"><kbd>' + (i + 1) + "</kbd>" + esc(VERDICT_LABELS[v] || v) + "</button>"
          );
        })
        .join("") +
      "</div></div>" +
      '<div class="vbar-row"><label class="sr-only" for="review-notes">Note</label>' +
      '<input id="review-notes" type="text" maxlength="2000" autocomplete="off" placeholder="What is wrong, if anything (Enter saves)" value="' +
      esc(review ? review.notes || "" : "") + '">' +
      '<button type="button" class="btn sm" id="next-unreviewed" title="Next unchecked output (n)">Next unchecked</button></div>' +
      '<div class="vbar-row vbar-meta"><span class="review-status" id="review-status" role="status">' +
      (review
        ? esc("Checked by " + (review.by === "agent" ? "the agent" : "you") + " · " + dateFmt(review.at))
        : "Not checked yet · keys: v then 1–4") +
      '</span><label class="auto"><input type="checkbox" id="auto-next"' + (autoNext() ? " checked" : "") +
      "> Go to the next unchecked after saving</label></div></form>"
    );
  }

  function wireVerdictBar(run, manifest, model, docId) {
    var form = document.getElementById("review-form");
    if (!form) return;
    var notes = document.getElementById("review-notes");
    var status = document.getElementById("review-status");
    var auto = document.getElementById("auto-next");
    var buttons = Array.prototype.slice.call(form.querySelectorAll("button[data-verdict]"));
    var chosen = null;
    buttons.forEach(function (b) {
      if (b.getAttribute("aria-pressed") === "true") chosen = b.getAttribute("data-verdict");
    });
    function save(verdict, thenNext) {
      chosen = verdict;
      buttons.forEach(function (b) {
        b.setAttribute("aria-pressed", String(b.getAttribute("data-verdict") === verdict));
      });
      status.textContent = "Saving…";
      fetch(url("api/review"), {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ doc: docId, model: model.model, verdict: verdict, notes: notes.value }),
      })
        .then(function (resp) {
          return resp.json().then(function (body) {
            if (!resp.ok) throw new Error((body && body.error) || "HTTP " + resp.status);
            return body;
          });
        })
        .then(function () {
          delete runCache[run.run_id];
          delete jsonCache[url("data/runs/" + enc(run.run_id) + ".json")];
          return getRun(run.run_id);
        })
        .then(function (fresh) {
          if (thenNext && goNextUnreviewed(fresh, manifest, docId, model.model, false)) return;
          render();
        })
        .catch(function (err) {
          status.textContent = "Not saved: " + err.message;
        });
    }
    buttons.forEach(function (b) {
      b.addEventListener("click", function () {
        save(b.getAttribute("data-verdict"), auto.checked);
      });
    });
    notes.addEventListener("keydown", function (e) {
      if (e.key === "Escape") {
        notes.blur();
        return;
      }
      if (e.key !== "Enter" || e.metaKey || e.ctrlKey) return; // Ctrl/Cmd+Enter is the global save-and-next
      e.preventDefault();
      if (chosen) save(chosen, auto.checked);
      else status.textContent = "Pick a verdict first: v then 1–4.";
    });
    form.addEventListener("submit", function (e) {
      e.preventDefault();
    });
    auto.addEventListener("change", function () {
      try {
        localStorage.setItem("puffinparse-auto-next", auto.checked ? "1" : "0");
      } catch (e) {
        /* private mode */
      }
    });
    document.getElementById("next-unreviewed").addEventListener("click", function () {
      goNextUnreviewed(run, manifest, docId, model.model, true);
    });
    REVIEW_KEYS.verdict = function (i) {
      if (buttons[i]) save(buttons[i].getAttribute("data-verdict"), auto.checked);
    };
    REVIEW_KEYS.saveNext = function () {
      if (chosen) save(chosen, true);
      else goNextUnreviewed(run, manifest, docId, model.model, true);
    };
  }

  /** The leaderboard of a review run: one row per model, ranked by checks then agreement. */
  function reviewModels(runInfo, run, params) {
    var r = run.review;
    var ordered = byScore(run.models);
    var monthly = isNum(r.monthly_pages) ? r.monthly_pages : null;
    var cols = [
      { key: "rank", label: "#", cell: function (row, position) { return { html: String(position + 1), cls: "rank" }; } },
      {
        key: "model",
        label: "Model",
        asc: true,
        rowHeader: true,
        cls: "sticky-col",
        headCls: "sticky-col",
        value: function (row) { return row.model; },
        cell: function (row) {
          return { html: '<a class="model-id" href="' + esc(listHref("documents", run.run_id, { pm: "1" })) + '">' + esc(row.model) + "</a>" };
        },
      },
      {
        key: "checked",
        label: "Checked correct",
        num: true,
        best: "max",
        title: "Documents judged correct, of those checked (by you in this viewer or by the agent)",
        value: function (row) { var t = (row.review || {}).reviews || {}; return t.reviewed ? t.correct / t.reviewed : null; },
        cell: function (row) {
          var t = (row.review || {}).reviews || {};
          if (!t.reviewed) return { html: "—", cls: "muted", title: "Not checked yet" };
          return {
            html: esc(t.correct + " / " + t.reviewed),
            title: t.correct + " correct, " + t.minor_errors + " minor errors, " + t.major_errors + " major errors, " + t.unusable + " unusable",
          };
        },
      },
      {
        key: "agreement",
        label: "Agreement",
        num: true,
        best: "max",
        title: "Mean share of words this model has in common with the others (it is not accuracy)",
        value: function (row) { return (row.review || {}).agreement; },
        cell: function (row) { return { html: esc(pctOf((row.review || {}).agreement)) }; },
      },
      {
        key: "numbers",
        label: "On numbers",
        num: true,
        best: "max",
        cls: "hide-narrow",
        headCls: "hide-narrow",
        title: "Agreement counting only tokens with digits: amounts, dates, ids",
        value: function (row) { return (row.review || {}).numbers_agreement; },
        cell: function (row) { return { html: esc(pctOf((row.review || {}).numbers_agreement)) }; },
      },
      {
        key: "outliers",
        label: "Outlier on",
        num: true,
        asc: true,
        best: "min",
        cls: "hide-narrow",
        headCls: "hide-narrow",
        title: "Documents where this model disagreed with the others the most",
        value: function (row) { return (row.review || {}).outlier_docs; },
        cell: function (row) { return { html: esc(String((row.review || {}).outlier_docs || 0)) }; },
      },
      {
        key: "failed",
        label: "Failed",
        num: true,
        asc: true,
        value: function (row) { var m = row.review || {}; return (m.failed || 0) + (m.empty || 0) + (m.missing || 0); },
        cell: function (row) {
          var m = row.review || {};
          var n = (m.failed || 0) + (m.empty || 0) + (m.missing || 0);
          return { html: esc(String(n)), cls: n ? "v-poor" : "muted", title: m.failed + " failed, " + m.empty + " empty, " + m.missing + " not run" };
        },
      },
      {
        key: "p50",
        label: "p50",
        num: true,
        asc: true,
        best: "min",
        value: function (row) { return (row.review || {}).latency_p50_ms; },
        cell: function (row) { return { html: esc(msFmt((row.review || {}).latency_p50_ms)) }; },
      },
      {
        key: "cost",
        label: "$/1k pages",
        num: true,
        asc: true,
        best: "min",
        title: "What this run cost per 1,000 pages (list price when nothing was billed yet)",
        value: function (row) { var m = row.review || {}; return isNum(m.usd_per_1k_pages) ? m.usd_per_1k_pages : m.list_usd_per_1k_pages; },
        cell: function (row) {
          var m = row.review || {};
          return { html: esc(moneyFmt(isNum(m.usd_per_1k_pages) ? m.usd_per_1k_pages : m.list_usd_per_1k_pages)) };
        },
      },
    ];
    if (monthly) {
      cols.push({
        key: "monthly",
        label: "Monthly",
        num: true,
        asc: true,
        best: "min",
        title: "At " + monthly.toLocaleString("en-US") + " pages a month",
        value: function (row) { return (row.review || {}).monthly_usd; },
        cell: function (row) { return { html: esc(moneyFmt((row.review || {}).monthly_usd)) }; },
      });
    }
    cols.push({
      key: "evidence",
      label: "Evidence",
      cls: "hide-narrow muted",
      headCls: "hide-narrow",
      title: "What else is known about this model: the public benchmark, or a check against the live service",
      cell: function (row) { return { html: esc(statusLabel((row.review || {}).verification)) }; },
    });

    var sortKey = params.get("sort") || null;
    var sortDir = params.get("dir") || (sortKey && ["p50", "cost", "monthly", "failed", "outliers", "model"].indexOf(sortKey) !== -1 ? "asc" : "desc");
    var queue = (r.review_queue || []).slice(0, 6);
    var best = ordered[0];
    var lede =
      plural(run.dataset.documents, "document") + " · " + plural(run.models.length, "model") + " · " + esc(statusLabel(r.status)) +
      " · " + esc(dateFmt(run.created_at)) + (monthly ? " · monthly cost at " + esc(monthly.toLocaleString("en-US")) + " pages" : "");

    view.innerHTML =
      '<header class="page-head"><div><h1>Models</h1><p class="lede">' + lede + '</p><p class="caption mono">' + esc(run.dataset.root || "") + "</p></div></header>" +
      (queue.length
        ? '<div class="queue"><p><strong>Check these first.</strong> The documents with failures or where the models disagree most, not yet checked:</p><ul>' +
          queue
            .map(function (id) {
              return '<li><a href="' + esc(docHref(run.run_id, best.model, id, {})) + '">' + esc(id) + "</a></li>";
            })
            .join("") +
          "</ul></div>"
        : "") +
      '<div class="table-tools"><p class="caption">Ranked by the documents checked correct, then by agreement. Agreement shows where models read the same words; it is not accuracy.</p></div>' +
      sortableTable({ id: "leaderboard", cls: "lb-table", columns: cols, rows: ordered, sortKey: sortKey, sortDir: sortDir, caption: "Models in this run" }) +
      '<div class="bottom-disclosures">' +
      ((r.notes || []).length
        ? disclosure("Notes <span class=\"count\">" + r.notes.length + "</span>", '<ul class="prose">' + r.notes.map(function (n) { return "<li>" + esc(n) + "</li>"; }).join("") + "</ul>", { id: "rv-notes", open: true })
        : "") +
      disclosure(
        "Next steps",
        '<p class="note">Write the report next to the run, or finish calls that failed or were never made:</p>' +
          codeBlock("puffinparse eval analyze " + JSON.stringify(r.run_dir || "") + " --write-report") +
          codeBlock("puffinparse eval run --resume " + JSON.stringify(r.run_dir || "")),
        { id: "rv-next" }
      ) +
      disclosure(
        "Run details",
        '<dl class="kv"><dt>Run</dt><dd class="mono">' + esc(run.run_id) + "</dd>" +
          "<dt>Folder</dt><dd class=\"mono\">" + esc(run.dataset.root || "") + "</dd>" +
          "<dt>Started</dt><dd>" + esc(run.created_at) + "</dd>" +
          "<dt>Status</dt><dd>" + esc(statusLabel(r.status)) + "</dd>" +
          "<dt>Paid with</dt><dd>" + esc(r.via === "gateway" ? "a PuffinParse gateway key" : "your provider keys") + "</dd>" +
          (r.pages_selection ? "<dt>Pages</dt><dd>" + esc(r.pages_selection) + "</dd>" : "") +
          (isNum(r.max_cost_usd) ? "<dt>Spending cap</dt><dd>" + esc(moneyFmt(r.max_cost_usd)) + "</dd>" : "") +
          "<dt>Version</dt><dd>PuffinParse " + esc(run.puffinparse_version) + "</dd></dl>",
        { id: "rv-run" }
      ) +
      "</div>";
    wireSorting(view, function (key, dir) {
      return listHref("leaderboard", run.run_id, { sort: key, dir: dir });
    });
    wireCopyButtons(view);
    wireDisclosures(view);
    KEYS = {};
    return "leaderboard|" + run.run_id;
  }

  /** The documents of a review run, most disagreement first. */
  function reviewDocuments(runInfo, run, manifest, params) {
    var models = byScore(run.models);
    var byDoc = recordsByDoc(run);
    var perModel = params.get("pm") === "1";
    var filters = { q: params.get("q") };
    var rows = docList(run, manifest, params).map(function (doc) {
      var rd = run.reviewByDoc[doc.id] || {};
      var reviews = rd.reviews || {};
      var worst = null;
      var order = ["correct", "minor_errors", "major_errors", "unusable"];
      Object.keys(reviews).forEach(function (m) {
        if (worst === null || order.indexOf(reviews[m].verdict) > order.indexOf(worst)) worst = reviews[m].verdict;
      });
      return { doc: doc, rd: rd, checked: Object.keys(reviews).length, worst: worst };
    });
    function openModel(row) {
      var agree = row.rd.agreement || {};
      var outlier = (row.rd.outliers || [])[0];
      if (outlier) return outlier;
      var best = models.slice().sort(function (a, b) { return (agree[b.model] || 0) - (agree[a.model] || 0); })[0];
      return best ? best.model : models[0].model;
    }
    var columns = [
      {
        key: "id",
        label: "Document",
        asc: true,
        rowHeader: true,
        cls: "doc-cell" + (perModel ? " sticky-col" : ""),
        headCls: perModel ? "sticky-col" : "",
        value: function (row) { return row.doc.id; },
        cell: function (row) {
          return { html: '<a href="' + esc(docHref(run.run_id, openModel(row), row.doc.id, filters)) + '">' + esc(row.doc.id) + "</a>", title: row.doc.id };
        },
      },
      {
        key: "dis",
        label: "Disagreement",
        num: true,
        title: "1 − mean word agreement between the models that read the document; high means look here first",
        value: function (row) { return row.rd.disagreement; },
        cell: function (row) {
          var d = row.rd.disagreement;
          return { html: esc(pctOf(d)), cls: isNum(d) ? (d > 0.3 ? "v-poor" : d > 0.1 ? "v-fair" : "") : "muted" };
        },
      },
      {
        key: "num",
        label: "On numbers",
        num: true,
        cls: "hide-narrow",
        headCls: "hide-narrow",
        value: function (row) { return row.rd.numbers_disagreement; },
        cell: function (row) { return { html: esc(pctOf(row.rd.numbers_disagreement)) }; },
      },
      {
        key: "failed",
        label: "Failed",
        num: true,
        value: function (row) { return (row.rd.failed || []).length + (row.rd.empty || []).length; },
        cell: function (row) {
          var names = (row.rd.failed || []).concat(row.rd.empty || []);
          return { html: esc(String(names.length)), cls: names.length ? "v-poor" : "muted", title: names.join(", ") };
        },
      },
    ];
    if (!perModel) {
      columns.push(
        {
          key: "outliers",
          label: "Outlier",
          cls: "hide-narrow",
          headCls: "hide-narrow",
          cell: function (row) {
            var o = row.rd.outliers || [];
            return { html: o.length ? o.map(function (m) { return '<code class="model-id">' + esc(m) + "</code>"; }).join(" ") : '<span class="muted">—</span>' };
          },
        },
        {
          key: "checked",
          label: "Checked",
          num: true,
          asc: true,
          value: function (row) { return row.checked; },
          cell: function (row) {
            if (!row.checked) return { html: "—", cls: "muted" };
            return { html: esc(row.checked + " / " + models.length) + " " + (row.worst ? '<span class="vchip v-' + VERDICT_TONE[row.worst] + '">' + esc(VERDICT_LABELS[row.worst]) + "</span>" : "") };
          },
        }
      );
    } else {
      models.forEach(function (model) {
        var parts = modelParts(model.model);
        columns.push({
          key: "m:" + model.model,
          label: model.model,
          labelHtml: '<span class="mhead"><span class="p">' + esc(parts.p) + '</span><span class="v">' + esc(parts.v) + "</span></span>",
          num: true,
          title: "Agreement of " + model.model + " with the other models, or your verdict once checked",
          value: function (row) { var a = (row.rd.agreement || {})[model.model]; return isNum(a) ? a : null; },
          cell: function (row) {
            var rec = byDoc[row.doc.id] && byDoc[row.doc.id][model.model];
            var href = esc(docHref(run.run_id, model.model, row.doc.id, filters));
            if (rec && rec.missing) return { html: "not run", cls: "muted" };
            if (rec && rec.error) return { html: '<a href="' + href + '">failed</a>', cls: "low", title: rec.error };
            var v = ((row.rd.reviews || {})[model.model] || {}).verdict;
            if (v) return { html: '<a href="' + href + '" class="vchip v-' + VERDICT_TONE[v] + '">' + esc(VERDICT_LABELS[v]) + "</a>" };
            var a = (row.rd.agreement || {})[model.model];
            return { html: '<a href="' + href + '">' + esc(pctOf(a)) + "</a>", cls: "pm-cell" };
          },
        });
      });
    }
    var sortKey = params.get("sort") || "dis";
    var sortDir = params.get("dir") || (sortKey === "id" || sortKey === "checked" ? "asc" : "desc");
    var total = manifest.documents.length;
    view.innerHTML =
      '<header class="page-head"><div><h1>Documents</h1><p class="lede">' +
      (rows.length === total ? plural(total, "document") : rows.length + " of " + plural(total, "document")) +
      " · most disagreement first: open one to see the page next to each model's output and record what you find</p></div></header>" +
      '<div class="filters" role="search">' +
      '<label class="grow"><span class="sr-only">Filter</span><input id="doc-search" type="search" placeholder="Filter" value="' + esc(params.get("q") || "") + '" /></label>' +
      '<span class="spacer"></span>' +
      '<button type="button" class="toggle" id="per-model" aria-pressed="' + perModel + '">Per model</button>' +
      "</div>" +
      sortableTable({ cls: "docs-table", columns: columns, rows: rows, sortKey: sortKey, sortDir: sortDir, caption: "Documents with how much the models disagree" });
    function refilter(values) {
      var next = { q: params.get("q"), sort: params.get("sort"), dir: params.get("dir"), pm: params.get("pm") };
      Object.keys(values).forEach(function (k) {
        next[k] = values[k];
      });
      go(listHref("documents", run.run_id, next), true);
    }
    wireSorting(view, function (key, dir) {
      return listHref("documents", run.run_id, { q: params.get("q"), sort: key, dir: dir, pm: params.get("pm") });
    });
    view.querySelector("#per-model").addEventListener("click", function () {
      refilter({ pm: perModel ? null : "1" });
    });
    var search = view.querySelector("#doc-search");
    var timer = null;
    search.addEventListener("input", function () {
      clearTimeout(timer);
      timer = setTimeout(function () {
        refilter({ q: search.value });
        var again = document.getElementById("doc-search");
        if (again) {
          again.focus();
          again.setSelectionRange(again.value.length, again.value.length);
        }
      }, 220);
    });
    KEYS = {};
    return "documents|" + run.run_id;
  }

  /* ------------------------------------------------------- review workspace */

  function reducedMotion() {
    return !!(window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches);
  }

  /** On a wide screen the inspector fills the window below its header: page and output scroll inside. */
  function fitWorkspace() {
    var rv = document.getElementById("rv");
    if (!rv) return;
    if (window.innerWidth <= 960) {
      rv.style.removeProperty("--rv-h");
      return;
    }
    var top = rv.getBoundingClientRect().top + window.scrollY;
    rv.style.setProperty("--rv-h", Math.max(420, window.innerHeight - top - 16) + "px");
  }

  var SPLIT_KEY = "puffinparse-split";

  /** The draggable divider between the page and the output; its position is remembered. */
  function wireSplit(rv) {
    if (!rv) return;
    var saved = null;
    try {
      saved = Number(localStorage.getItem(SPLIT_KEY));
    } catch (e) {
      /* private mode */
    }
    if (saved >= 25 && saved <= 75) rv.style.setProperty("--split", saved + "%");
    var bar = rv.querySelector(".rv-split");
    function set(pct, done) {
      pct = Math.max(25, Math.min(75, pct));
      rv.style.setProperty("--split", pct.toFixed(1) + "%");
      if (!done) return;
      try {
        localStorage.setItem(SPLIT_KEY, String(Math.round(pct)));
      } catch (e) {
        /* private mode */
      }
      if (SV) SV.rerender();
    }
    bar.addEventListener("pointerdown", function (e) {
      e.preventDefault();
      bar.setPointerCapture(e.pointerId);
      var box = rv.getBoundingClientRect();
      var last = null;
      function move(ev) {
        last = ((ev.clientX - box.left) / box.width) * 100;
        set(last, false);
      }
      function up() {
        bar.removeEventListener("pointermove", move);
        bar.removeEventListener("pointerup", up);
        if (last !== null) set(last, true);
      }
      bar.addEventListener("pointermove", move);
      bar.addEventListener("pointerup", up);
    });
    bar.addEventListener("keydown", function (e) {
      var cur = parseFloat(rv.style.getPropertyValue("--split")) || 48;
      if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
        e.preventDefault();
        set(cur + (e.key === "ArrowLeft" ? -5 : 5), true);
      }
    });
    view.querySelectorAll(".doc-details").forEach(function (d) {
      d.addEventListener("toggle", fitWorkspace);
    });
  }

  /** The Blocks tab: one card per block with a box, in page order, linked to its box. */
  function blocksBody(overlay, model) {
    if (!overlay || !overlay.count) {
      return '<p class="empty">This model\'s response has no layout boxes for this document.</p>';
    }
    var pages = Object.keys(overlay.pages)
      .map(Number)
      .sort(function (a, b) {
        return a - b;
      });
    var multi = pages.length > 1;
    return (
      '<p class="caption">' + esc(plural(overlay.count, "block")) + " from " + esc(model.model) +
      ". Point at a block to see it on the page; click a box on the page to find its block.</p>" +
      '<div class="blocks">' +
      pages
        .map(function (pg) {
          return (
            (multi ? '<h3 class="blk-page">Page ' + pg + "</h3>" : "") +
            overlay.pages[pg]
              .map(function (b, i) {
                var g = groupOf(b.type);
                var conf = isNum(b.confidence) ? b.confidence : null;
                var text = String(b.text || b.content || "");
                return (
                  '<div class="blk" tabindex="0" id="blk-' + pg + "-" + i + '" data-p="' + pg + '" data-i="' + i + '">' +
                  '<div class="blk-h"><span class="sw bt-' + g.key + '" aria-hidden="true"></span><span class="blk-type">' +
                  esc(b.type || g.label) + "</span>" +
                  (conf !== null
                    ? '<span class="blk-c' + (conf < 0.8 ? " low" : "") + '" title="Confidence the provider reported for this block">' +
                      Math.round(conf * 100) + "%</span>"
                    : "") +
                  '</div><div class="md">' + (text.trim() ? renderMarkdown(text) : '<p class="empty">No text</p>') + "</div></div>"
                );
              })
              .join("")
          );
        })
        .join("") +
      "</div>"
    );
  }

  /** Handlers the verdict bar installs for the global keys (`v` then 1–4, Ctrl/Cmd+Enter). */
  var REVIEW_KEYS = { verdict: null, saveNext: null };

  /** Every output that can be checked (the call succeeded), in document then model order. */
  function reviewPairs(run, manifest) {
    var byDoc = recordsByDoc(run);
    var models = byScore(run.models);
    var pairs = [];
    docList(run, manifest, new URLSearchParams()).forEach(function (d) {
      models.forEach(function (m) {
        var r = (byDoc[d.id] || {})[m.model];
        if (!r || r.error || r.missing) return;
        var checked = ((run.reviewByDoc[d.id] || {}).reviews || {})[m.model];
        pairs.push({ doc: d.id, model: m.model, done: !!checked });
      });
    });
    return pairs;
  }

  function reviewProgress(run, manifest) {
    var pairs = reviewPairs(run, manifest);
    if (!pairs.length) return "";
    var done = pairs.filter(function (p) {
      return p.done;
    }).length;
    var width = Math.round((done / pairs.length) * 100);
    return (
      '<div class="rv-progress"><span class="rv-bar" aria-hidden="true"><i style="width:' + width + '%"></i></span><span>' +
      (done === pairs.length
        ? "All " + pairs.length + ' outputs checked · <a href="' + esc(listHref("leaderboard", run.run_id)) + '">see the models</a>'
        : esc(done + " of " + pairs.length + " outputs checked")) +
      "</span></div>"
    );
  }

  /** Open the next output nobody has checked yet, after the current one. False when there is none. */
  function goNextUnreviewed(run, manifest, docId, modelName, announce) {
    var pairs = reviewPairs(run, manifest);
    var at = -1;
    pairs.forEach(function (p, i) {
      if (p.doc === docId && p.model === modelName) at = i;
    });
    for (var k = 1; k <= pairs.length; k++) {
      var p = pairs[(at + k + pairs.length) % pairs.length];
      if (!p.done && !(p.doc === docId && p.model === modelName)) {
        go(docHref(run.run_id, p.model, p.doc, {}));
        return true;
      }
    }
    if (announce) {
      var status = document.getElementById("review-status");
      if (status) status.textContent = "Every output is checked.";
    }
    return false;
  }

  /* ------------------------------------------------------------------ boot */

  function render() {
    var route = parseHash();
    var parts = route.parts;
    var params = route.params;
    var name = parts[0] || "leaderboard";
    view.setAttribute("aria-busy", "true");
    var main = document.getElementById("main");
    if (main) main.classList.toggle("wide", !!findRun(parts[0]) && parts.length >= 3);
    REVIEW_KEYS.verdict = null;
    REVIEW_KEYS.saveNext = null;
    var task;
    try {
      if (name === "doc") {
        // Old `#/doc/<id>?run=&model=` links: rewrite to the canonical `#/<run>/<model>/<doc>`.
        var legacyRun = resolveRun(params);
        var legacyModel = params.get("model") || ((legacyRun.models || [])[0] || {}).model || "";
        go(docHref(legacyRun.run_id, legacyModel, parts.slice(1).join("/"), { diff: params.get("diff") }), true);
        return;
      }
      var direct = findRun(parts[0]);
      if (direct && parts.length >= 3) task = viewInspector(direct, parts[1], parts.slice(2).join("/"), params);
      else if (direct) task = viewLeaderboard(direct, params);
      else if (name === "documents") task = viewDocuments(resolveRun(params), params);
      else task = viewLeaderboard(resolveRun(params), params);
    } catch (err) {
      showError(err);
      return;
    }
    Promise.resolve(task).then(
      function (scrollKey) {
        if (scrollKey === null) return; // redirected
        view.removeAttribute("aria-busy");
        if (scrollKey !== lastScrollKey) {
          window.scrollTo(0, 0);
          lastScrollKey = scrollKey;
        }
      },
      function (err) {
        showError(err);
      }
    );
  }

  var pendingG = false;
  var pendingV = false;
  var oDownAt = 0;
  function onKey(e) {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && REVIEW_KEYS.saveNext) {
      e.preventDefault();
      REVIEW_KEYS.saveNext();
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    var t = e.target;
    var tag = t && t.tagName;
    if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA" || (t && t.isContentEditable)) return;
    var help = document.getElementById("help");
    if (e.key === "?") {
      if (help && help.showModal && !help.open) help.showModal();
      e.preventDefault();
      return;
    }
    if (help && help.open) return;
    if (pendingG) {
      pendingG = false;
      var run = parseHash().params.get("run");
      var direct = findRun(parseHash().parts[0]);
      var runId = direct ? direct.run_id : run;
      if (e.key === "l") go(listHref("leaderboard", runId));
      else if (e.key === "d") go(listHref("documents", runId));
      return;
    }
    if (pendingV) {
      pendingV = false;
      if (/^[1-4]$/.test(e.key) && REVIEW_KEYS.verdict) {
        e.preventDefault();
        REVIEW_KEYS.verdict(Number(e.key) - 1);
      }
      return;
    }
    if (e.key === "v" && REVIEW_KEYS.verdict) {
      pendingV = true;
      setTimeout(function () {
        pendingV = false;
      }, 1500);
      return;
    }
    // `o`: a tap toggles the boxes, holding it shows them until the key is released.
    if (e.key === "o" && SV && SV.overlay && document.body.contains(SV.el)) {
      e.preventDefault();
      if (!e.repeat) {
        oDownAt = Date.now();
        SV.setPeek(true);
      }
      return;
    }
    if (e.key === "g") {
      pendingG = true;
      setTimeout(function () {
        pendingG = false;
      }, 1200);
      return;
    }
    var fn = KEYS[e.key];
    if (fn) {
      e.preventDefault();
      fn();
    }
  }

  function wireChrome() {
    var toggle = document.getElementById("theme");
    if (toggle)
      toggle.addEventListener("click", function () {
        var root = document.documentElement;
        var dark = root.getAttribute("data-theme") === "dark" || (!root.getAttribute("data-theme") && window.matchMedia("(prefers-color-scheme: dark)").matches);
        var next = dark ? "light" : "dark";
        root.setAttribute("data-theme", next);
        try {
          localStorage.setItem("puffinparse-theme", next);
        } catch (e) {
          /* private mode */
        }
      });
    var helpBtn = document.getElementById("help-btn");
    var help = document.getElementById("help");
    if (helpBtn && help && help.showModal)
      helpBtn.addEventListener("click", function () {
        help.showModal();
      });
    else if (helpBtn) helpBtn.hidden = true;
    if (HOME) document.getElementById("brand").setAttribute("href", HOME);
    if (MODE === "review") {
      document.querySelectorAll(".review-only").forEach(function (el) {
        el.hidden = false;
      });
      var sub = document.querySelector(".brand-sub");
      if (sub) sub.textContent = "run viewer";
      var lb = document.getElementById("nav-leaderboard");
      if (lb) lb.textContent = "Models";
      var footer = document.querySelector(".site-footer .wrap");
      if (footer)
        footer.innerHTML =
          "Served on this computer by <code>puffinparse eval view</code>. Your documents and outputs never leave it.";
    }
    var docs = document.getElementById("nav-docs");
    if (DOCS && docs) {
      docs.setAttribute("href", DOCS);
      docs.hidden = false;
    }
    document.addEventListener("keydown", onKey);
    document.addEventListener("keyup", function (e) {
      if (e.key !== "o" || !oDownAt || !SV) return;
      var tapped = Date.now() - oDownAt < 300;
      oDownAt = 0;
      SV.setPeek(false);
      if (tapped && KEYS.o) KEYS.o();
    });
    var resizeTimer = null;
    window.addEventListener("resize", function () {
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(function () {
        if (SV && document.body.contains(SV.el)) SV.rerender();
        fitWorkspace();
      }, 250);
    });
  }

  function boot() {
    wireChrome();
    getJSON(url("data/index.json")).then(
      function (data) {
        INDEX = data;
        var generated = document.getElementById("footer-generated");
        if (generated && data.generated_at) generated.textContent = " · built " + dateFmt(data.generated_at);
        window.addEventListener("hashchange", render);
        render();
      },
      function (err) {
        showError(err);
      }
    );
  }

  boot();
})();
