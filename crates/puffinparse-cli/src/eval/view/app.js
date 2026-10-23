/* PuffinParse run viewer (served by `puffinparse eval view`). Vanilla JS, no build step (ADR-17).
 *
 * The Markdown renderer below is copied from website/assets/playground.js: it escapes everything
 * and rebuilds only whitelisted inline and table tags, so model output can never inject markup.
 * Keep the two in sync. */
(function () {
  "use strict";

  function esc(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  /* ------------------------------------------------------------ safe Markdown rendering
   * Model output is untrusted text. Everything is escaped; Markdown structure is rebuilt from
   * scratch, and the only HTML kept is a short list of tags providers really emit (emphasis,
   * sub/superscript, line breaks, tables) with every attribute dropped except a numeric
   * colspan/rowspan. Links keep only http(s) targets; images are never loaded. */

  var INLINE_TAGS = { b: 1, strong: 1, i: 1, em: 1, u: 1, s: 1, del: 1, sup: 1, sub: 1, code: 1, br: 1, mark: 1, small: 1 };
  var TABLE_TAGS = { table: 1, thead: 1, tbody: 1, tfoot: 1, tr: 1, td: 1, th: 1, caption: 1 };
  var TAG_RE = /<\/?[a-zA-Z][a-zA-Z0-9]*(?:\s[^<>]*)?\/?>/g;

  function cleanTag(raw, allowTable) {
    var m = /^<(\/?)([a-zA-Z][a-zA-Z0-9]*)([^>]*)>$/.exec(raw);
    if (!m) return "";
    var name = m[2].toLowerCase();
    if (!INLINE_TAGS[name] && !(allowTable && TABLE_TAGS[name])) return ""; // dropped, text kept
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
  function inlineText(text) {
    var codes = [];
    var s = esc(text).replace(/`([^`]+)`/g, function (_, c) {
      codes.push("<code>" + c + "</code>");
      return "\u0000" + (codes.length - 1) + "\u0000";
    });
    s = s.replace(/!\[([^\]]*)\]\(([^)\s]*)[^)]*\)/g, function (_, alt) {
      return '<span class="pg-img">[image' + (alt ? ": " + alt : "") + "]</span>";
    });
    s = s.replace(/\[([^\]]+)\]\(([^)\s]+)[^)]*\)/g, function (_, label, href) {
      return /^https?:\/\//i.test(href.replace(/&amp;/g, "&"))
        ? '<a href="' + href + '" rel="nofollow noopener noreferrer" target="_blank">' + label + "</a>"
        : label;
    });
    s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>").replace(/__([^_]+)__/g, "<strong>$1</strong>");
    s = s.replace(/(^|[^*\w])\*([^*\s][^*]*?)\*(?!\w)/g, "$1<em>$2</em>");
    s = s.replace(/(^|[^_\w])_([^_\s][^_]*?)_(?!\w)/g, "$1<em>$2</em>");
    return s.replace(/\u0000(\d+)\u0000/g, function (_, i) { return codes[+i]; });
  }

  /** Text with inline HTML: whitelisted tags are rebuilt, everything else is escaped text. */
  function inline(text, allowTable) {
    var out = "";
    var last = 0;
    var s = String(text);
    TAG_RE.lastIndex = 0;
    var m;
    while ((m = TAG_RE.exec(s))) {
      out += inlineText(s.slice(last, m.index)) + cleanTag(m[0], allowTable);
      last = m.index + m[0].length;
    }
    return out + inlineText(s.slice(last));
  }

  var FENCE = /^\s{0,3}(`{3,}|~{3,})/;
  var HEADING = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
  var RULE = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;
  var LIST = /^\s{0,3}([-*+]|\d{1,9}[.)])\s+(.*)$/;
  var QUOTE = /^\s{0,3}>\s?(.*)$/;
  var DELIM = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

  function splitRow(line) {
    var s = line.trim().replace(/\\\|/g, "\u0001").replace(/^\|/, "").replace(/\|$/, "");
    return s.split("|").map(function (c) { return c.trim().replace(/\u0001/g, "|"); });
  }

  function startsBlock(line, next) {
    return FENCE.test(line) || HEADING.test(line) || RULE.test(line) || LIST.test(line) || QUOTE.test(line) ||
      /^\s*<table\b/i.test(line) || (line.indexOf("|") !== -1 && DELIM.test(next || ""));
  }

  function renderMarkdown(md, depth) {
    depth = depth || 0;
    var lines = String(md == null ? "" : md).replace(/\r\n?/g, "\n").replace(/<!--[\s\S]*?-->/g, "").split("\n");
    var out = [];
    var i = 0;
    while (i < lines.length) {
      var line = lines[i];
      if (!line.trim()) { i++; continue; }
      var m;
      if ((m = FENCE.exec(line))) {
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
        out.push('<div class="pg-tablewrap">' + inline(block.join("\n"), true) + "</div>");
        continue;
      }
      if ((m = HEADING.exec(line))) {
        var level = Math.min(m[1].length + 2, 6); // a result card's title is the h3
        out.push("<h" + level + ">" + inline(m[2]) + "</h" + level + ">");
        i++;
        continue;
      }
      if (RULE.test(line)) { out.push("<hr>"); i++; continue; }
      if (line.indexOf("|") !== -1 && DELIM.test(lines[i + 1] || "")) {
        var head = splitRow(line);
        var rows = [];
        i += 2;
        while (i < lines.length && lines[i].indexOf("|") !== -1 && lines[i].trim()) rows.push(splitRow(lines[i++]));
        out.push(
          '<div class="pg-tablewrap"><table><thead><tr>' +
            head.map(function (c) { return "<th>" + inline(c) + "</th>"; }).join("") +
            "</tr></thead><tbody>" +
            rows.map(function (r) {
              return "<tr>" + r.map(function (c) { return "<td>" + inline(c) + "</td>"; }).join("") + "</tr>";
            }).join("") +
            "</tbody></table></div>"
        );
        continue;
      }
      if (QUOTE.test(line)) {
        var quoted = [];
        while (i < lines.length && (m = QUOTE.exec(lines[i]))) { quoted.push(m[1]); i++; }
        out.push("<blockquote>" + (depth < 3 ? renderMarkdown(quoted.join("\n"), depth + 1) : esc(quoted.join(" "))) + "</blockquote>");
        continue;
      }
      if ((m = LIST.exec(line))) {
        var ordered = /\d/.test(m[1]);
        var items = [];
        while (i < lines.length) {
          var lm = LIST.exec(lines[i]);
          if (lm) { items.push(lm[2]); i++; continue; }
          if (lines[i].trim() && /^\s{2,}/.test(lines[i]) && items.length) { items[items.length - 1] += " " + lines[i].trim(); i++; continue; }
          break;
        }
        var tag = ordered ? "ol" : "ul";
        out.push("<" + tag + ">" + items.map(function (t) { return "<li>" + inline(t) + "</li>"; }).join("") + "</" + tag + ">");
        continue;
      }
      var para = [line];
      i++;
      while (i < lines.length && lines[i].trim() && !startsBlock(lines[i], lines[i + 1])) para.push(lines[i++]);
      out.push("<p>" + inline(para.join("\n")).replace(/\n/g, " ") + "</p>");
    }
    return out.join("\n");
  }

  /* ------------------------------------------------------------ viewer */

  var state = { run: null, doc: null, model: null, mode: "rendered", against: null, cache: {} };
  var $ = function (id) { return document.getElementById(id); };

  function pct(x) { return x == null ? "–" : (x * 100).toFixed(1) + "%"; }
  function usd(x) { return x == null ? "–" : "$" + Number(x).toFixed(2); }
  function secs(ms) { return ms == null ? "–" : (ms / 1000).toFixed(1) + " s"; }
  function tone(dis) { return dis == null ? "" : dis > 0.3 ? "poor" : dis > 0.1 ? "fair" : "good"; }

  function get(path) {
    return fetch(path, { headers: { accept: "application/json" } }).then(function (r) {
      return r.json().then(function (body) {
        if (!r.ok) throw new Error(body && body.error ? body.error : "HTTP " + r.status);
        return body;
      });
    });
  }

  function load() {
    return get("api/run").then(function (run) {
      state.run = run;
      state.cache = {};
      renderSummary();
      renderDocs();
      if (!state.doc && run.per_doc.length) selectDoc(sorted()[0].id);
      else if (state.doc) selectDoc(state.doc);
    }).catch(function (e) {
      $("run-meta").textContent = "Could not load the run: " + e.message;
    });
  }

  function renderSummary() {
    var r = state.run;
    $("run-meta").textContent = r.run_id + " · " + r.documents + " documents · " + r.models.length + " models · " +
      String(r.status).replace("_", " ") + (r.monthly_pages ? " · monthly cost at " + r.monthly_pages + " pages" : "");
    $("models").tBodies[0].innerHTML = r.per_model.map(function (m) {
      var checked = m.reviews.reviewed ? m.reviews.correct + "/" + m.reviews.reviewed : "–";
      return "<tr><td><code>" + esc(m.model) + "</code></td><td class=n>" + m.ok + "</td><td class=n>" +
        (m.failed + m.missing) + "</td><td class=n>" + pct(m.agreement) + "</td><td class=n>" + pct(m.numbers_agreement) +
        "</td><td class=n>" + checked + "</td><td class=n>" + secs(m.latency_p50_ms) + "</td><td class=n>" +
        usd(m.usd_per_1k_pages != null ? m.usd_per_1k_pages : m.list_usd_per_1k_pages) + "</td><td class=n>" +
        usd(m.monthly_usd) + "</td><td>" + esc(m.verification) + "</td></tr>";
    }).join("");
  }

  function sorted() {
    var docs = state.run.per_doc.slice();
    var by = $("sort").value;
    if (by === "name") docs.sort(function (a, b) { return a.id < b.id ? -1 : 1; });
    else {
      docs.sort(function (a, b) {
        if (by === "unreviewed") {
          var ra = Object.keys(a.reviews).length ? 1 : 0, rb = Object.keys(b.reviews).length ? 1 : 0;
          if (ra !== rb) return ra - rb;
        }
        var fa = a.failed.length + a.empty.length ? 1 : 0, fb = b.failed.length + b.empty.length ? 1 : 0;
        if (fa !== fb) return fb - fa;
        return (b.disagreement || 0) - (a.disagreement || 0);
      });
    }
    return docs;
  }

  function renderDocs() {
    $("doc-list").innerHTML = sorted().map(function (d) {
      var badges = "";
      if (d.failed.length) badges += '<span class="badge poor" title="failed: ' + esc(d.failed.join(", ")) + '">✕ ' + d.failed.length + "</span>";
      if (d.empty.length) badges += '<span class="badge fair" title="empty output">∅ ' + d.empty.length + "</span>";
      if (Object.keys(d.reviews).length) badges += '<span class="badge good" title="checked">✓</span>';
      if (d.disagreement != null) badges += '<span class="badge ' + tone(d.disagreement) + '" title="disagreement">' + pct(d.disagreement) + "</span>";
      return '<li data-id="' + esc(d.id) + '" aria-selected="' + (d.id === state.doc) + '"><span class="id" title="' +
        esc(d.id) + '">' + esc(d.id) + '</span><span class="badges">' + badges + "</span></li>";
    }).join("");
  }

  function docInfo(id) {
    return state.run.docs.filter(function (d) { return d.id === id; })[0];
  }

  function selectDoc(id) {
    state.doc = id;
    renderDocs();
    var info = docInfo(id) || { kind: "" };
    var url = "doc?id=" + encodeURIComponent(id);
    $("doc-title").textContent = id;
    $("doc-open").href = url;
    var frame = $("doc-frame");
    if (info.kind === "pdf") frame.innerHTML = '<iframe title="Original document" src="' + url + '"></iframe>';
    else if (/^(png|jpe?g|gif|webp|bmp)$/.test(info.kind)) frame.innerHTML = '<div class="scroll"><img alt="Original document" src="' + url + '"></div>';
    else frame.innerHTML = '<p class="msg">Your browser cannot preview .' + esc(info.kind) + ' files here. Use “open” to download it.</p>';
    var d = currentDoc();
    if (!state.model || state.run.models.indexOf(state.model) < 0) state.model = state.run.models[0];
    $("tabs").innerHTML = state.run.models.map(function (m) {
      var bad = d && d.failed.indexOf(m) >= 0;
      var a = d && d.agreement[m] != null ? " " + Math.round(d.agreement[m] * 100) + "%" : "";
      return '<button type="button" role="tab" data-model="' + esc(m) + '" aria-selected="' + (m === state.model) + '"' +
        (bad ? ' class="bad"' : "") + ">" + esc(m) + a + "</button>";
    }).join("");
    $("against").innerHTML = state.run.models.map(function (m) { return '<option value="' + esc(m) + '">vs ' + esc(m) + "</option>"; }).join("");
    showOutput();
  }

  function currentDoc() {
    return state.run.per_doc.filter(function (d) { return d.id === state.doc; })[0];
  }

  function output(model, doc) {
    var key = model + "\u0000" + doc;
    if (!state.cache[key]) {
      state.cache[key] = get("api/output?model=" + encodeURIComponent(model) + "&doc=" + encodeURIComponent(doc));
    }
    return state.cache[key];
  }

  function words(s) { return String(s).split(/\s+/).filter(Boolean); }

  /** Word diff by longest common subsequence; long outputs are cut to keep it quick. */
  function diff(a, b) {
    var max = 1500;
    var x = words(a).slice(0, max), y = words(b).slice(0, max);
    var n = x.length, m = y.length, i, j;
    var dp = new Array((n + 1) * (m + 1));
    for (i = 0; i <= n; i++) dp[i * (m + 1) + m] = 0;
    for (j = 0; j <= m; j++) dp[n * (m + 1) + j] = 0;
    for (i = n - 1; i >= 0; i--) for (j = m - 1; j >= 0; j--) {
      dp[i * (m + 1) + j] = x[i] === y[j] ? dp[(i + 1) * (m + 1) + j + 1] + 1 : Math.max(dp[(i + 1) * (m + 1) + j], dp[i * (m + 1) + j + 1]);
    }
    var out = [];
    i = 0; j = 0;
    while (i < n && j < m) {
      if (x[i] === y[j]) { out.push(esc(x[i])); i++; j++; }
      else if (dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1]) out.push("<del>" + esc(x[i++]) + "</del>");
      else out.push("<ins>" + esc(y[j++]) + "</ins>");
    }
    while (i < n) out.push("<del>" + esc(x[i++]) + "</del>");
    while (j < m) out.push("<ins>" + esc(y[j++]) + "</ins>");
    var cut = words(a).length > max || words(b).length > max ? '<p class="muted">Showing the first ' + max + " words of each.</p>" : "";
    return cut + '<div class="diff">' + out.join(" ") + "</div>";
  }

  function showOutput() {
    var model = state.model, doc = state.doc;
    var mode = $("mode").value;
    $("against").hidden = mode !== "diff";
    if (mode === "diff" && $("against").value === model) {
      var other = state.run.models.filter(function (m) { return m !== model; })[0];
      if (other) $("against").value = other;
    }
    Array.prototype.forEach.call($("tabs").children, function (b) { b.setAttribute("aria-selected", String(b.dataset.model === model)); });
    $("output").innerHTML = '<p class="muted">Loading…</p>';
    var d = currentDoc();
    var review = d && d.reviews[model];
    renderReview(review);
    var main = output(model, doc);
    var second = mode === "diff" ? output($("against").value, doc) : Promise.resolve(null);
    Promise.all([main, second]).then(function (res) {
      if (state.model !== model || state.doc !== doc) return;
      var o = res[0], rec = o.record;
      $("output-meta").textContent = rec
        ? [rec.pages + " pages", secs(rec.latency_ms), rec.cost_usd != null ? usd(rec.cost_usd) : null, rec.chars + " characters"]
            .filter(Boolean).join(" · ")
        : "No result yet";
      if (!o.ok) {
        $("output").innerHTML = '<p class="error">' + esc(rec ? (rec.error_kind || "error") + ": " + (rec.error || "") : "Not run yet.") + "</p>";
        return;
      }
      if (mode === "raw") $("output").innerHTML = '<pre class="raw">' + esc(o.markdown) + "</pre>";
      else if (mode === "diff") {
        var b = res[1];
        $("output").innerHTML = b && b.ok
          ? '<p class="muted"><del>struck</del>: only in ' + esc(model) + " · <ins>underlined</ins>: only in " + esc($("against").value) + "</p>" + diff(o.markdown, b.markdown)
          : '<p class="error">The other model has no output for this document.</p>';
      } else $("output").innerHTML = renderMarkdown(o.markdown) || '<p class="muted">(empty output)</p>';
    }).catch(function (e) {
      $("output").innerHTML = '<p class="error">' + esc(e.message) + "</p>";
    });
  }

  function renderReview(review) {
    $("review").hidden = false;
    $("verdicts").innerHTML = state.run.verdicts.map(function (v) {
      return '<label><input type="radio" name="verdict" value="' + v + '"' + (review && review.verdict === v ? " checked" : "") +
        ">" + v.replace("_", " ") + "</label>";
    }).join("");
    $("notes").value = review ? review.notes || "" : "";
    $("review-state").textContent = review ? "checked by " + review.by : "";
  }

  $("doc-list").addEventListener("click", function (e) {
    var li = e.target.closest("li");
    if (li) selectDoc(li.dataset.id);
  });
  $("tabs").addEventListener("click", function (e) {
    var b = e.target.closest("button");
    if (b) { state.model = b.dataset.model; showOutput(); }
  });
  $("sort").addEventListener("change", renderDocs);
  $("mode").addEventListener("change", showOutput);
  $("against").addEventListener("change", showOutput);
  $("review").addEventListener("submit", function (e) {
    e.preventDefault();
    var picked = document.querySelector('input[name="verdict"]:checked');
    if (!picked) { $("review-state").textContent = "Pick a verdict first."; return; }
    fetch("api/review", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ doc: state.doc, model: state.model, verdict: picked.value, notes: $("notes").value })
    }).then(function (r) {
      if (!r.ok) return r.json().then(function (b) { throw new Error(b.error || "HTTP " + r.status); });
      $("review-state").textContent = "Saved.";
      return load();
    }).catch(function (err) { $("review-state").textContent = err.message; });
  });
  document.addEventListener("keydown", function (e) {
    if (e.target.tagName === "INPUT" || e.target.tagName === "SELECT") return;
    if (e.key !== "j" && e.key !== "k") return;
    var ids = sorted().map(function (d) { return d.id; });
    var i = ids.indexOf(state.doc) + (e.key === "j" ? 1 : -1);
    if (i >= 0 && i < ids.length) selectDoc(ids[i]);
  });

  load();
})();
