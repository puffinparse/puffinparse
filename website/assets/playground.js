/* PuffinParse playground (/playground/). No dependencies of its own; supabase-js and Turnstile are
   loaded by the page only when the build configured them.

   The browser <-> gateway contract is docs/SERVER.md, "Playground API": GET /v1/playground/config,
   POST /v1/playground/runs (multipart), then GET /v1/jobs/{id} per model until it settles. Free-tier
   requests carry the Supabase access token as a bearer; bring-your-own-key requests carry one
   `x-provider-key-<provider>` header per provider and no Authorization header.

   The pure helpers at the top (file sniffing, the Markdown renderer, the API client, the poller)
   are exported for the offline tests in website/tests/playground.test.mjs. */
(function (root, factory) {
  "use strict";
  var core = factory();
  if (typeof module === "object" && module.exports) {
    module.exports = core;
  } else {
    root.PuffinPlayground = core;
    if (typeof document !== "undefined") {
      if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", core.boot);
      else core.boot();
    }
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  /* ================================================================== pure helpers */

  function esc(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  /** The document type from its first bytes, never from the name or the browser's guess. */
  function sniffType(bytes) {
    var b = bytes || [];
    if (b.length >= 5 && b[0] === 0x25 && b[1] === 0x50 && b[2] === 0x44 && b[3] === 0x46 && b[4] === 0x2d) {
      return "application/pdf"; // %PDF-
    }
    var png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    if (b.length >= 8 && png.every(function (v, i) { return b[i] === v; })) return "image/png";
    if (b.length >= 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff) return "image/jpeg";
    return "";
  }

  /** A rough PDF page count (page objects in the file), for an early warning only: the gateway
   *  counts pages itself and its answer is the one that matters. 0 when unknown. */
  function estimatePdfPages(text) {
    var m = String(text || "").match(/\/Type\s*\/Page(?![a-zA-Z])/g);
    return m ? m.length : 0;
  }

  /** Client-side checks before anything is uploaded. Returns an error sentence or "". */
  function validateFile(info, limits) {
    if (!info) return "Choose a file first.";
    if (!info.type) return "That file is not a PDF, PNG or JPEG (checked by its contents, not its name).";
    if ((limits.accepted_types || []).indexOf(info.type) === -1) return "That file type is not accepted here.";
    if (info.size > limits.max_file_bytes) {
      return "That file is " + mb(info.size) + "; the playground takes up to " + mb(limits.max_file_bytes) + ".";
    }
    if (info.pages > limits.pages_per_run) {
      return "That document has about " + info.pages + " pages; the playground takes up to " + limits.pages_per_run + ".";
    }
    return "";
  }

  function mb(bytes) {
    return (bytes / (1024 * 1024)).toFixed(bytes < 1024 * 1024 ? 2 : 1).replace(/\.0+$/, "") + " MB";
  }

  function money(usd) {
    if (typeof usd !== "number" || !isFinite(usd)) return "–";
    if (usd === 0) return "$0";
    return "$" + (usd < 0.1 ? usd.toFixed(4) : usd.toFixed(2));
  }

  function seconds(ms) {
    if (typeof ms !== "number" || !isFinite(ms)) return "–";
    return (ms / 1000).toFixed(ms < 10000 ? 1 : 0) + " s";
  }

  function per1k(price) {
    if (typeof price !== "number") return "price unknown";
    var v = price * 1000;
    return "$" + (v % 1 === 0 ? v.toFixed(0) : v.toFixed(2)) + " / 1k pages";
  }

  /* ------------------------------------------------------------ safe Markdown rendering
   * Model output is untrusted text. Everything is escaped; Markdown structure is rebuilt from
   * scratch, and the only HTML kept is a short list of tags providers really emit (emphasis,
   * sub/superscript, line breaks, tables) with every attribute dropped except a numeric
   * colspan/rowspan. Links keep only http(s) targets; images are never loaded. The benchmark
   * viewer (benchmark/site/src/app.js) has a copy with `md`-prefixed names: keep the two in sync. */

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

  /* ------------------------------------------------------------ API client */

  function ApiError(status, body, retryAfter) {
    var e = (body && body.error) || {};
    this.name = "ApiError";
    this.status = status;
    this.type = e.type || (status ? "http_" + status : "network_error");
    this.message = e.message || (status ? "The playground API answered " + status + "." : "Could not reach the playground API.");
    this.details = e.details || null;
    this.requestId = e.request_id || null;
    this.retryAfter = retryAfter || null;
  }
  ApiError.prototype = Object.create(Error.prototype);

  /** auth is {mode: "free", token} or {mode: "byok", keys: {provider: key}}. Never both. */
  function authHeaders(auth, providers) {
    var h = {};
    if (!auth) return h;
    if (auth.mode === "free") {
      if (auth.token) h.Authorization = "Bearer " + auth.token;
    } else if (auth.mode === "byok") {
      (providers || Object.keys(auth.keys || {})).forEach(function (p) {
        var key = (auth.keys || {})[p];
        if (key && /^[a-z0-9_]+$/.test(p)) h["x-provider-key-" + p] = key;
      });
    }
    return h;
  }

  function Api(base, fetchImpl) {
    this.base = String(base || "").replace(/\/+$/, "");
    this.fetch = fetchImpl || function (url, init) { return fetch(url, init); };
  }

  Api.prototype.request = function (method, path, headers, body) {
    var self_ = this;
    return Promise.resolve()
      .then(function () {
        return self_.fetch(self_.base + path, {
          method: method,
          headers: headers,
          body: body,
          mode: "cors",
          credentials: "omit",
          cache: "no-store",
          referrerPolicy: "no-referrer",
        });
      })
      .then(
        function (res) {
          return res.text().then(function (text) {
            var data = null;
            try { data = text ? JSON.parse(text) : null; } catch (e) { data = null; }
            if (!res.ok) {
              var retry = res.headers && res.headers.get ? Number(res.headers.get("retry-after")) || null : null;
              throw new ApiError(res.status, data, retry);
            }
            return data;
          });
        },
        function () { throw new ApiError(0, null); }
      );
  };

  Api.prototype.config = function (token) {
    return this.request("GET", "/v1/playground/config", token ? { Authorization: "Bearer " + token } : {});
  };

  /** One run: the document once, the models, and the free tier's Turnstile token. */
  Api.prototype.submit = function (opts) {
    var form = new FormData();
    form.append("file", opts.file, opts.filename || "document");
    opts.models.forEach(function (m) { form.append("models", m); });
    if (opts.auth && opts.auth.mode === "free" && opts.turnstile) form.append("turnstile_token", opts.turnstile);
    var providers = opts.models.map(providerOf).filter(function (p, i, a) { return a.indexOf(p) === i; });
    return this.request("POST", "/v1/playground/runs", authHeaders(opts.auth, providers), form);
  };

  Api.prototype.job = function (id, model, auth) {
    return this.request("GET", "/v1/jobs/" + encodeURIComponent(id), authHeaders(auth, [providerOf(model)]));
  };

  function providerOf(model) {
    var s = String(model);
    return s.indexOf("/") > 0 ? s.slice(0, s.indexOf("/")) : s;
  }

  function sleep(ms) {
    return new Promise(function (r) { setTimeout(r, ms); });
  }

  /** Poll every pending job until each is succeeded or failed, `stop()` returns true, or
   *  `maxMs` passes. `onUpdate(job)` fires on every change. Intervals back off from 1.5 s to 5 s.
   *  A poll that fails (network, 5xx) is retried; a 4xx ends that job with the error. */
  function pollJobs(api, jobs, auth, onUpdate, opts) {
    opts = opts || {};
    var wait = opts.sleep || sleep;
    var now = opts.now || Date.now;
    var maxMs = opts.maxMs || 8 * 60 * 1000;
    var start = now();
    var round = 0;
    function pending() {
      return jobs.filter(function (j) { return j.id && j.status === "pending"; });
    }
    function tick() {
      var open = pending();
      if (!open.length) return Promise.resolve("done");
      if (opts.stop && opts.stop()) return Promise.resolve("stopped");
      if (now() - start > maxMs) return Promise.resolve("timeout");
      var delay = round === 0 ? (opts.firstDelay != null ? opts.firstDelay : 1500) : Math.min(5000, 1500 + round * 500);
      round++;
      return wait(delay).then(function () {
        if (opts.stop && opts.stop()) return "stopped";
        return Promise.all(
          open.map(function (job) {
            return api.job(job.id, job.model, auth).then(
              function (data) {
                if (!data || data.status === "pending") return;
                job.status = data.status === "succeeded" ? "succeeded" : "failed";
                job.result = data.result || null;
                job.error = data.error || null;
                job.finishedAt = now();
                onUpdate(job);
              },
              function (err) {
                if (err.status >= 400 && err.status < 500 && err.status !== 429) {
                  job.status = "failed";
                  job.error = { type: err.type, message: err.message };
                  onUpdate(job);
                }
              }
            );
          })
        ).then(tick);
      });
    }
    return tick();
  }

  /** What failed and what to do, for the error types in docs/SERVER.md. */
  function errorSentence(err, ctx) {
    ctx = ctx || {};
    var d = err.details || {};
    var when = err.retryAfter ? " Try again in " + Math.ceil(err.retryAfter / 60) + " min." : "";
    switch (err.type) {
      case "network_error":
        return "Could not reach the playground API. Check your connection and try again.";
      case "unauthorized":
        return "Your sign-in has expired or is not valid. Sign in again.";
      case "turnstile_failed":
        return "The human check failed or expired. Complete it again, then run.";
      case "quota_exceeded":
        return "That run needs " + (d.requested || "more") + " model-pages and you have " +
          (d.remaining != null ? d.remaining : "fewer") + " left today. Pick fewer models or pages, or use your own keys.";
      case "free_tier_unavailable":
        return d.reason === "budget_exhausted"
          ? "Today's free budget is used up; it resets at 00:00 UTC. Samples and your own keys still work."
          : "The free tier is paused. Samples and your own keys still work.";
      case "ip_rate_limited":
        return "Too many requests from this network." + (when || " Wait a minute and try again.");
      case "too_many_pages":
        return "That document has " + (d.pages || "too many") + " pages; the playground takes up to " + (d.limit || ctx.pagesLimit || 10) + ".";
      case "too_many_models":
        return "Pick at most " + (d.limit || ctx.modelsLimit || 3) + " models.";
      case "payload_too_large":
        return "That file is larger than the playground accepts.";
      case "unsupported_media_type":
        return "Only PDF, PNG and JPEG files are accepted.";
      case "missing_provider_key":
        return "Paste a key for " + (d.provider || "every provider you picked") + ", or switch to the free tier.";
      case "model_not_allowed":
        return (err.message || "That model is not available here.") + " Pick another model.";
      case "authentication_error":
        return "The provider rejected the API key. Check it and try again.";
      default:
        return err.message || "Something went wrong.";
    }
  }

  /* ------------------------------------------------------------ remembered keys (opt-in) */

  var KEY_STORE = "puffinparse-playground-keys";

  function loadKeys(storage) {
    try {
      var raw = storage && storage.getItem(KEY_STORE);
      var data = raw ? JSON.parse(raw) : null;
      return data && typeof data === "object" ? data : null;
    } catch (e) {
      return null;
    }
  }

  function saveKeys(storage, keys) {
    try {
      var clean = {};
      Object.keys(keys).forEach(function (p) { if (keys[p]) clean[p] = keys[p]; });
      if (Object.keys(clean).length) storage.setItem(KEY_STORE, JSON.stringify(clean));
      else storage.removeItem(KEY_STORE);
    } catch (e) { /* storage blocked: keys stay in memory only */ }
  }

  function forgetKeys(storage) {
    try { storage.removeItem(KEY_STORE); } catch (e) { /* nothing stored */ }
  }

  /* ================================================================== the page */

  function boot() {
    var cfgEl = document.getElementById("pp-config");
    if (!cfgEl) return;
    var CFG = JSON.parse(cfgEl.textContent);
    var $ = function (id) { return document.getElementById(id); };
    var store = (function () { try { return window.localStorage; } catch (e) { return null; } })();

    var S = {
      server: null,
      liveError: "",
      samples: [],
      source: "sample",
      sample: null,
      upload: null, // {file, name, size, type, pages, error}
      selected: [],
      mode: "free",
      sb: null,
      session: null,
      keys: {},
      turnstile: { run: "", signin: "", runWidget: null, signinWidget: null },
      running: false,
      stopped: false,
      objectUrl: "",
    };
    var api = CFG.api ? new Api(CFG.api) : null;
    var models = CFG.models || [];
    var byId = {};
    models.forEach(function (m) { byId[m.id] = m; });

    function limits() {
      var l = {};
      Object.keys(CFG.limits).forEach(function (k) { l[k] = CFG.limits[k]; });
      if (S.server && S.server.limits) Object.keys(S.server.limits).forEach(function (k) { l[k] = S.server.limits[k]; });
      return l;
    }

    /* ---------- availability */

    function serverModel(id) {
      if (!S.server || !S.server.models) return null;
      for (var i = 0; i < S.server.models.length; i++) if (S.server.models[i].id === id) return S.server.models[i];
      return null;
    }
    function liveModes() {
      var srv = S.server;
      var free = !!(srv && srv.free_tier && srv.free_tier.available && CFG.supabase && window.supabase);
      var byok = !!(srv && srv.byok && srv.byok.available);
      return { free: free, byok: byok, any: free || byok };
    }
    function isLive() {
      return S.source === "upload" || $("pg-live-sample").checked;
    }
    function modelUsable(m) {
      if (!isLive()) {
        return !!(S.sample && S.sample.outputs.some(function (o) { return o.model === m.id; }));
      }
      var sm = serverModel(m.id);
      if (S.server && !sm) return false;
      if (S.mode === "free") return sm ? !!sm.free_tier : !!m.free_tier;
      return sm ? sm.byok !== false : true;
    }

    /* ---------- samples */

    function renderSamples() {
      var box = $("pg-samples");
      var html = '<legend class="pg-sr">Sample documents</legend>';
      S.samples.forEach(function (s, i) {
        var thumb = s.previews && s.previews[0]
          ? '<img src="' + esc(s.previews[0]) + '" alt="" loading="lazy" width="96" height="124">'
          : '<span class="pg-thumb-empty" aria-hidden="true">PDF</span>';
        html +=
          '<label class="pg-sample">' +
          '<input type="radio" name="sample" value="' + esc(s.id) + '"' + (i === 0 ? " checked" : "") + ">" +
          '<span class="pg-thumb">' + thumb + "</span>" +
          '<span class="pg-sample-text"><span class="pg-sample-title">' + esc(s.title) + "</span>" +
          '<span class="pg-hint">' + esc(s.source) + " · " + esc(s.pages) + (s.pages === 1 ? " page" : " pages") +
          " · " + esc(s.license) + "</span></span></label>";
      });
      box.innerHTML = html;
      S.sample = S.samples[0] || null;
      box.addEventListener("change", function (e) {
        if (e.target.name !== "sample") return;
        S.sample = S.samples.filter(function (s) { return s.id === e.target.value; })[0] || null;
        renderModels();
        refresh();
      });
    }

    function loadSamples() {
      return fetch(CFG.samples_url, { cache: "no-cache" })
        .then(function (r) { if (!r.ok) throw new Error(String(r.status)); return r.json(); })
        .then(function (index) {
          S.samples = index.samples || [];
          renderSamples();
        })
        .catch(function () {
          $("pg-samples").innerHTML = '<p class="pg-error">The sample documents could not be loaded. Reload the page to retry.</p>';
        });
    }

    /* ---------- upload */

    function readFile(file) {
      if (S.objectUrl) { URL.revokeObjectURL(S.objectUrl); S.objectUrl = ""; }
      var info = { file: file, name: file.name, size: file.size, type: "", pages: 0 };
      S.upload = info;
      $("pg-file-status").textContent = "Checking " + file.name + "…";
      var head = file.slice(0, 16).arrayBuffer();
      return head.then(function (buf) {
        info.type = sniffType(new Uint8Array(buf));
        if (info.type === "application/pdf" && file.size <= limits().max_file_bytes) {
          return file.arrayBuffer().then(function (all) {
            info.pages = estimatePdfPages(new TextDecoder("latin1").decode(new Uint8Array(all)));
          });
        }
        if (info.type) info.pages = 1;
      }).then(function () {
        info.error = validateFile(info, limits());
        var kind = { "application/pdf": "PDF", "image/png": "PNG", "image/jpeg": "JPEG" }[info.type] || "unknown type";
        var pages = info.type === "application/pdf" ? (info.pages ? ", about " + info.pages + (info.pages === 1 ? " page" : " pages") : "") : ", 1 page";
        $("pg-file-status").innerHTML = info.error
          ? '<span class="pg-error">' + esc(info.error) + "</span>"
          : "<strong>" + esc(info.name) + "</strong> · " + kind + ", " + mb(info.size) + pages;
        refresh();
      });
    }

    function wireUpload() {
      var input = $("pg-file");
      var drop = $("pg-drop");
      input.addEventListener("change", function () { if (input.files && input.files[0]) readFile(input.files[0]); });
      ["dragenter", "dragover"].forEach(function (t) {
        drop.addEventListener(t, function (e) { e.preventDefault(); drop.classList.add("over"); });
      });
      ["dragleave", "drop"].forEach(function (t) {
        drop.addEventListener(t, function (e) { e.preventDefault(); drop.classList.remove("over"); });
      });
      drop.addEventListener("drop", function (e) {
        var f = e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files[0];
        if (f) readFile(f);
      });
    }

    /* ---------- models */

    function defaultSelection() {
      var picked = [];
      var seen = {};
      models.forEach(function (m) {
        if (picked.length < limits().models_per_run && m.default && !seen[m.provider] && modelUsable(m)) {
          picked.push(m.id);
          seen[m.provider] = true;
        }
      });
      return picked;
    }

    function renderModels() {
      S.selected = S.selected.filter(function (id) { return byId[id] && modelUsable(byId[id]); });
      if (!S.selected.length) S.selected = defaultSelection();
      var groups = [];
      models.forEach(function (m) {
        if (!groups.length || groups[groups.length - 1].provider !== m.provider) groups.push({ provider: m.provider, name: m.provider_name, items: [] });
        groups[groups.length - 1].items.push(m);
      });
      var max = limits().models_per_run;
      var html = "";
      groups.forEach(function (g) {
        html += '<fieldset class="pg-provider"><legend>' + esc(g.name) + "</legend>";
        g.items.forEach(function (m) {
          var usable = modelUsable(m);
          var on = S.selected.indexOf(m.id) !== -1;
          var full = !on && S.selected.length >= max;
          var why = "";
          if (!usable) {
            why = !isLive() ? "no saved output" : S.mode === "free" ? "own key only" : "unavailable";
          }
          var out = !isLive() && S.sample ? S.sample.outputs.filter(function (o) { return o.model === m.id; })[0] : null;
          html +=
            '<label class="pg-model' + (usable ? "" : " off") + '">' +
            '<input type="checkbox" value="' + esc(m.id) + '"' + (on ? " checked" : "") + (!usable || full ? " disabled" : "") + ">" +
            '<span class="pg-model-id">' + esc(m.model) + "</span>" +
            '<span class="pg-hint">' + esc(per1k(m.price_per_page_usd)) +
            (out && typeof out.score === "number" ? " · score " + out.score.toFixed(1) : "") +
            (why ? " · " + esc(why) : "") + "</span></label>";
        });
        html += "</fieldset>";
      });
      $("pg-models").innerHTML = html || '<p class="pg-hint">No hosted models are configured.</p>';
      $("pg-model-count").textContent = S.selected.length + " of " + max + " selected";
    }

    function wireModels() {
      $("pg-models").addEventListener("change", function (e) {
        var id = e.target.value;
        if (e.target.checked) { if (S.selected.indexOf(id) === -1) S.selected.push(id); }
        else S.selected = S.selected.filter(function (x) { return x !== id; });
        renderModels();
        // keep focus on the same checkbox after the re-render
        var again = $("pg-models").querySelector('input[value="' + CSS.escape(id) + '"]');
        if (again) again.focus();
        refresh();
      });
    }

    /* ---------- keys (bring your own) */

    function selectedProviders() {
      var seen = {};
      return S.selected.map(function (id) { return byId[id]; }).filter(function (m) {
        if (!m || seen[m.provider]) return false;
        seen[m.provider] = true;
        return true;
      });
    }

    function renderKeys() {
      var box = $("pg-keys");
      var html = "";
      selectedProviders().forEach(function (m) {
        var id = "pg-key-" + m.provider;
        html += '<div class="pg-key"><label for="' + id + '">' + esc(m.provider_name) + " API key</label>" +
          '<input type="password" id="' + id + '" data-provider="' + esc(m.provider) + '" autocomplete="off" ' +
          'spellcheck="false" autocapitalize="off" value="' + esc(S.keys[m.provider] || "") + '"></div>';
      });
      box.innerHTML = html || '<p class="pg-hint">Pick models to see which keys are needed.</p>';
    }

    function wireKeys() {
      var saved = loadKeys(store);
      if (saved) {
        S.keys = saved;
        $("pg-remember").checked = true;
        $("pg-forget").hidden = false;
      }
      $("pg-keys").addEventListener("input", function (e) {
        var p = e.target.getAttribute("data-provider");
        if (!p) return;
        S.keys[p] = e.target.value.trim();
        if ($("pg-remember").checked) saveKeys(store, S.keys);
        refresh();
      });
      $("pg-remember").addEventListener("change", function () {
        if (this.checked) { saveKeys(store, S.keys); $("pg-forget").hidden = false; }
        else { forgetKeys(store); $("pg-forget").hidden = true; }
      });
      $("pg-forget").addEventListener("click", function () {
        forgetKeys(store);
        S.keys = {};
        $("pg-remember").checked = false;
        this.hidden = true;
        renderKeys();
        refresh();
      });
    }

    /* ---------- Turnstile (free tier only) */

    function whenTurnstile(cb, tries) {
      if (window.turnstile && window.turnstile.render) return cb(window.turnstile);
      if ((tries || 0) > 60) return;
      setTimeout(function () { whenTurnstile(cb, (tries || 0) + 1); }, 250);
    }

    function mountTurnstile(slot, which) {
      if (!CFG.turnstile_site_key) return;
      var key = which === "run" ? "runWidget" : "signinWidget";
      if (S.turnstile[key] !== null) return;
      S.turnstile[key] = "pending";
      whenTurnstile(function (ts) {
        S.turnstile[key] = ts.render($(slot), {
          sitekey: CFG.turnstile_site_key,
          action: which === "run" ? "playground-run" : "playground-signin",
          theme: "auto",
          size: "flexible",
          callback: function (token) { S.turnstile[which] = token; refresh(); },
          "expired-callback": function () { S.turnstile[which] = ""; refresh(); },
          "error-callback": function () { S.turnstile[which] = ""; refresh(); },
        });
      });
    }

    function resetTurnstile(which) {
      S.turnstile[which] = "";
      var w = S.turnstile[which === "run" ? "runWidget" : "signinWidget"];
      if (w && w !== "pending" && window.turnstile) window.turnstile.reset(w);
    }

    /* ---------- Supabase sign-in */

    function setSession(session) {
      S.session = session || null;
      var user = S.session && S.session.user;
      $("pg-signed-out").hidden = !!user;
      $("pg-signed-in").hidden = !user;
      if (user) {
        $("pg-user").textContent = user.email || (user.user_metadata && user.user_metadata.user_name) || "your account";
        mountTurnstile("pg-turnstile", "run");
        loadServerConfig();
      } else {
        mountTurnstile("pg-signin-turnstile", "signin");
      }
      refresh();
    }

    function initAuth() {
      if (!CFG.supabase || !window.supabase || !window.supabase.createClient) return;
      S.sb = window.supabase.createClient(CFG.supabase.url, CFG.supabase.anon_key, {
        auth: { flowType: "pkce", persistSession: true, autoRefreshToken: true, detectSessionInUrl: true },
      });
      S.sb.auth.onAuthStateChange(function (_event, session) {
        setSession(session);
        if (location.search.indexOf("code=") !== -1) history.replaceState(null, "", location.pathname + location.hash);
      });
      S.sb.auth.getSession().then(function (r) { setSession(r.data && r.data.session); });
      var back = location.origin + location.pathname;
      $("pg-github").addEventListener("click", function () {
        S.sb.auth.signInWithOAuth({ provider: "github", options: { redirectTo: back } });
      });
      $("pg-email-send").addEventListener("click", function () {
        var email = $("pg-email").value.trim();
        var status = $("pg-email-status");
        if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) { status.textContent = "Enter an email address."; return; }
        if (CFG.turnstile_site_key && !S.turnstile.signin) { status.textContent = "Complete the human check first."; return; }
        status.textContent = "Sending…";
        S.sb.auth
          .signInWithOtp({ email: email, options: { emailRedirectTo: back, captchaToken: S.turnstile.signin || undefined } })
          .then(function (r) {
            resetTurnstile("signin");
            status.textContent = r.error ? "Could not send the link: " + r.error.message : "Check your inbox for a sign-in link from Supabase.";
          });
      });
      $("pg-signout").addEventListener("click", function () { S.sb.auth.signOut(); });
    }

    /* ---------- server config */

    function loadServerConfig() {
      if (!api) return Promise.resolve();
      var token = S.session && S.session.access_token;
      return api.config(token).then(
        function (cfg) { S.server = cfg; S.liveError = ""; renderModels(); refresh(); },
        function (err) {
          S.server = null;
          S.liveError = err.type === "network_error"
            ? "The playground API is not answering right now. Samples still work."
            : errorSentence(err);
          refresh();
        }
      );
    }

    function quotaLine() {
      var ft = S.server && S.server.free_tier;
      var u = ft && ft.user;
      if (!u) return "";
      return u.model_pages_remaining + " of " + limits().model_pages_per_day + " free model-pages left today (resets 00:00 UTC).";
    }

    /* ---------- the run button and its estimate */

    function pagesNow() {
      if (S.source === "sample") return S.sample ? S.sample.pages : 0;
      return S.upload && !S.upload.error ? S.upload.pages || 1 : 0;
    }

    function blocker() {
      if (!S.selected.length) return "Pick at least one model.";
      if (S.source === "sample" && !S.sample) return "Pick a sample document.";
      if (S.source === "upload" && (!S.upload || S.upload.error)) return S.upload && S.upload.error ? S.upload.error : "Choose a file first.";
      if (!isLive()) return "";
      var lm = liveModes();
      if (!api) return "Live runs are not enabled on this site yet.";
      if (!lm.any) return S.liveError || "Live runs are unavailable right now.";
      if (S.mode === "free") {
        if (!lm.free) return "The free tier is unavailable right now. Use your own keys.";
        if (!S.session) return "Sign in to use the free tier.";
        if (CFG.turnstile_site_key && !S.turnstile.run) return "Complete the human check.";
        var u = S.server.free_tier.user;
        var need = pagesNow() * S.selected.length;
        if (u && need > u.model_pages_remaining) return "This run needs " + need + " model-pages; you have " + u.model_pages_remaining + " left today.";
      } else {
        var missing = selectedProviders().filter(function (m) { return !S.keys[m.provider]; });
        if (missing.length) return "Paste a key for " + missing.map(function (m) { return m.provider_name; }).join(", ") + ".";
      }
      return "";
    }

    function refresh() {
      var live = isLive();
      var lm = liveModes();
      $("pg-sample-pane").hidden = S.source !== "sample";
      $("pg-upload-pane").hidden = S.source !== "upload";
      $("pg-sample-run").hidden = S.source !== "sample";
      $("pg-live-sample-wrap").hidden = !(api && lm.any);
      $("pg-saved-note").hidden = live;
      $("pg-live").hidden = !live || !api || !lm.any;
      var off = live && (!api || !lm.any);
      $("pg-live-off").hidden = !off;
      if (off) {
        $("pg-live-off").textContent = !api
          ? "Live runs are not enabled on this site yet. Pick a sample document to compare saved outputs."
          : S.liveError || (S.server ? "Live runs are paused right now. Samples still work." : "Connecting to the playground API…");
      }
      var freeRadio = document.querySelector('input[name="mode"][value="free"]');
      var byokRadio = document.querySelector('input[name="mode"][value="byok"]');
      freeRadio.disabled = !lm.free;
      byokRadio.disabled = !lm.byok;
      if (S.mode === "free" && !lm.free && lm.byok) { S.mode = "byok"; byokRadio.checked = true; renderModels(); }
      $("pg-free").hidden = S.mode !== "free";
      $("pg-byok").hidden = S.mode !== "byok";
      var ft = S.server && S.server.free_tier;
      $("pg-free-off").hidden = !(ft && !ft.available);
      if (ft && !ft.available) {
        $("pg-free-off").textContent = errorSentence({ type: "free_tier_unavailable", details: { reason: ft.reason } });
      }
      $("pg-quota").textContent = quotaLine();
      if (live && S.mode === "byok") renderKeysIfChanged();
      $("pg-turnstile").hidden = !(live && S.mode === "free" && S.session);

      var why = blocker();
      var run = $("pg-run");
      run.textContent = S.running ? "Running…" : live ? "Run " + S.selected.length + (S.selected.length === 1 ? " model" : " models") : "Show outputs";
      run.setAttribute("aria-disabled", why || S.running ? "true" : "false");
      var est = "";
      if (!why && live) {
        var pages = pagesNow();
        var cost = 0;
        var known = true;
        S.selected.forEach(function (id) {
          var p = byId[id] && byId[id].price_per_page_usd;
          if (typeof p === "number") cost += p * pages; else known = false;
        });
        est = (pages ? pages + (pages === 1 ? " page × " : " pages × ") + S.selected.length + " = " + pages * S.selected.length + " model-pages" : "") +
          (known ? ", about " + money(cost) + " at list price" + (S.mode === "free" ? " (on us)" : " (on your account)") : "");
      }
      $("pg-estimate").textContent = why ? why : est;
    }

    var lastKeyProviders = "";
    function renderKeysIfChanged() {
      var sig = selectedProviders().map(function (m) { return m.provider; }).join(",");
      if (sig !== lastKeyProviders) { lastKeyProviders = sig; renderKeys(); }
    }

    /* ---------- results */

    function card(job) {
      var m = byId[job.model] || { model: job.model, provider_name: providerOf(job.model) };
      var id = "pg-out-" + job.model.replace(/[^a-z0-9]+/gi, "-");
      var head = '<div class="pg-card-head"><h3><span class="pg-hint">' + esc(m.provider_name) + "</span> " +
        '<span class="pg-model-id">' + esc(m.model || job.model) + "</span></h3>";
      if (job.status === "pending") {
        return head + '</div><p class="pg-pending"><span class="pg-spinner" aria-hidden="true"></span>Waiting for ' +
          esc(m.provider_name) + "…</p>";
      }
      if (job.status === "failed") {
        var e = job.error || {};
        return head + '</div><div class="pg-fail"><p><strong>Failed</strong>' + (e.type ? ' <code>' + esc(e.type) + "</code>" : "") + "</p>" +
          "<p>" + esc(e.type === "authentication_error" && S.mode === "byok" ? errorSentence(e) : e.message || "The provider reported a failure.") + "</p></div>";
      }
      var r = job.result || {};
      var md = typeof r.markdown === "string" ? r.markdown : "";
      var pages = r.usage && r.usage.pages != null ? r.usage.pages : job.pages;
      var latency = typeof r.latency_ms === "number" ? r.latency_ms : job.finishedAt && job.startedAt ? job.finishedAt - job.startedAt : null;
      var metrics =
        '<dl class="pg-metrics"><div><dt>Latency</dt><dd>' + esc(seconds(latency)) + "</dd></div>" +
        "<div><dt>Pages</dt><dd>" + esc(pages != null ? pages : "–") + "</dd></div>" +
        "<div><dt>Cost</dt><dd>" + esc(money(r.cost_usd)) + "</dd></div>" +
        (typeof job.score === "number" ? "<div><dt>Score</dt><dd>" + esc(job.score.toFixed(1)) + "</dd></div>" : "") +
        "</dl>";
      return head +
        '<div class="pg-view" role="group" aria-label="Output view">' +
        '<button type="button" data-view="rendered" aria-pressed="true">Rendered</button>' +
        '<button type="button" data-view="raw" aria-pressed="false">Markdown</button></div></div>' +
        metrics +
        '<div class="pg-out pg-md" id="' + id + '-r" tabindex="0" role="region" aria-label="' + esc(job.model) + ' output, rendered">' +
        (md ? renderMarkdown(md) : '<p class="pg-hint">Empty output.</p>') + "</div>" +
        '<pre class="pg-out pg-raw" id="' + id + '-m" tabindex="0" role="region" aria-label="' + esc(job.model) + ' output, Markdown" hidden>' + esc(md) + "</pre>" +
        '<div class="pg-card-foot"><button type="button" class="pg-link" data-copy-md>Copy Markdown</button>' +
        '<button type="button" class="pg-link" data-download-md>Download .md</button>' +
        (job.viewer ? '<a href="' + esc(job.viewer) + '">Inspect in the benchmark viewer</a>' : "") + "</div>";
    }

    function drawCards(jobs) {
      var grid = $("pg-grid");
      grid.style.setProperty("--cols", String(Math.max(1, jobs.length)));
      grid.innerHTML = jobs.map(function (j, i) {
        return '<article class="pg-card" data-i="' + i + '" aria-busy="' + (j.status === "pending") + '">' + card(j) + "</article>";
      }).join("");
      grid.querySelectorAll(".pg-card").forEach(function (el) { el._job = jobs[+el.getAttribute("data-i")]; });
    }

    function updateCard(job, jobs) {
      var i = jobs.indexOf(job);
      var el = $("pg-grid").querySelector('.pg-card[data-i="' + i + '"]');
      if (!el) return;
      el.innerHTML = card(job);
      el.setAttribute("aria-busy", "false");
      el._job = job;
    }

    function wireResults() {
      $("pg-grid").addEventListener("click", function (e) {
        var cardEl = e.target.closest(".pg-card");
        if (!cardEl) return;
        var job = cardEl._job;
        var view = e.target.getAttribute("data-view");
        if (view) {
          cardEl.querySelectorAll("[data-view]").forEach(function (b) { b.setAttribute("aria-pressed", String(b === e.target)); });
          cardEl.querySelector(".pg-md").hidden = view !== "rendered";
          cardEl.querySelector(".pg-raw").hidden = view !== "raw";
          return;
        }
        var md = (job && job.result && job.result.markdown) || "";
        if (e.target.hasAttribute("data-copy-md")) {
          var btn = e.target;
          var done = function () { btn.textContent = "Copied"; setTimeout(function () { btn.textContent = "Copy Markdown"; }, 1400); };
          if (navigator.clipboard) navigator.clipboard.writeText(md).then(done, function () {});
        }
        if (e.target.hasAttribute("data-download-md")) {
          var url = URL.createObjectURL(new Blob([md], { type: "text/markdown;charset=utf-8" }));
          var a = document.createElement("a");
          a.href = url;
          a.download = job.model.replace(/\//g, "_") + ".md";
          document.body.appendChild(a);
          a.click();
          a.remove();
          setTimeout(function () { URL.revokeObjectURL(url); }, 1000);
        }
      });
      $("pg-stop").addEventListener("click", function () { S.stopped = true; });
    }

    function docLine(name, pages, extra) {
      $("pg-docline").innerHTML = "<strong>" + esc(name) + "</strong>" +
        (pages ? " · " + pages + (pages === 1 ? " page" : " pages") : "") + (extra || "");
    }

    function showSamples() {
      var s = S.sample;
      var jobs = S.selected.map(function (id) {
        var o = s.outputs.filter(function (x) { return x.model === id; })[0];
        return { model: id, status: "pending", score: o && o.score, viewer: o && o.viewer, out: o };
      });
      $("pg-results").hidden = false;
      $("pg-stop").hidden = true;
      docLine(s.title, s.pages, ' · <a href="' + esc(s.file) + '">original file</a> · ' + esc(s.source) + ", " +
        (s.license_url ? '<a href="' + esc(s.license_url) + '" rel="noopener">' + esc(s.license) + "</a>" : esc(s.license)));
      $("pg-status").textContent = "Saved outputs from the benchmark run. No provider was called.";
      drawCards(jobs);
      return Promise.all(jobs.map(function (job) {
        if (!job.out) { job.status = "failed"; job.error = { message: "No saved output for this sample." }; updateCard(job, jobs); return null; }
        return fetch(job.out.markdown).then(function (r) { return r.ok ? r.text() : Promise.reject(r.status); }).then(
          function (md) {
            job.status = "succeeded";
            job.result = { markdown: md, latency_ms: job.out.latency_ms, cost_usd: job.out.cost_usd, usage: { pages: job.out.pages } };
            updateCard(job, jobs);
          },
          function () { job.status = "failed"; job.error = { message: "The saved output could not be loaded." }; updateCard(job, jobs); }
        );
      }));
    }

    function documentForRun() {
      if (S.source === "upload") return Promise.resolve({ blob: S.upload.file, name: S.upload.name, pages: S.upload.pages || 1 });
      var s = S.sample;
      return fetch(s.file).then(function (r) { if (!r.ok) throw new Error("sample"); return r.blob(); })
        .then(function (blob) { return { blob: blob, name: s.filename, pages: s.pages }; });
    }

    function runLive() {
      // Free tier: read the session's token on every request, since supabase-js refreshes it
      // during a long poll and the gateway ties the jobs to the user, not to one token.
      // Own-key runs need no sign-in, so there may be no session at all.
      var startToken = S.session ? S.session.access_token : "";
      var auth = S.mode === "free"
        ? { mode: "free", get token() { return (S.session && S.session.access_token) || startToken; } }
        : { mode: "byok", keys: S.keys };
      var chosen = S.selected.slice();
      S.running = true;
      S.stopped = false;
      refresh();
      $("pg-form-error").textContent = "";
      return documentForRun().then(function (doc) {
        $("pg-results").hidden = false;
        docLine(doc.name, doc.pages, "");
        $("pg-status").textContent = "Uploading…";
        var jobs = chosen.map(function (id) { return { model: id, status: "pending", startedAt: Date.now() }; });
        drawCards(jobs);
        return api.submit({ file: doc.blob, filename: doc.name, models: chosen, auth: auth, turnstile: S.turnstile.run })
          .then(function (run) {
            if (S.mode === "free") resetTurnstile("run");
            (run.jobs || []).forEach(function (rj) {
              var job = jobs.filter(function (j) { return j.model === rj.model; })[0];
              if (!job) return;
              job.id = rj.id;
              job.pages = run.pages;
              if (rj.status === "failed" || !rj.id) { job.status = "failed"; job.error = rj.error; updateCard(job, jobs); }
            });
            if (run.usage && S.server && S.server.free_tier && S.server.free_tier.user) {
              S.server.free_tier.user.model_pages_remaining = run.usage.model_pages_remaining;
            }
            if (run.pages) docLine(doc.name, run.pages, "");
            $("pg-status").textContent = "Waiting for the providers. Results appear as each one finishes.";
            $("pg-stop").hidden = false;
            return pollJobs(api, jobs, auth, function (job) { updateCard(job, jobs); }, { stop: function () { return S.stopped; } });
          })
          .then(function (outcome) {
            $("pg-stop").hidden = true;
            var cost = jobs.reduce(function (t, j) { return t + ((j.result && j.result.cost_usd) || 0); }, 0);
            $("pg-status").textContent = outcome === "stopped" ? "Stopped. Providers may still finish (and bill) the jobs."
              : outcome === "timeout" ? "Some providers have not finished after 8 minutes. Run again later to retry."
              : "Done. Total " + money(cost) + " at list price.";
          }, function (err) {
            if (S.mode === "free") resetTurnstile("run");
            $("pg-results").hidden = true;
            $("pg-form-error").textContent = errorSentence(err, { pagesLimit: limits().pages_per_run, modelsLimit: limits().models_per_run });
            if (err.type === "unauthorized" && S.sb) S.sb.auth.signOut();
          });
      }, function () {
        // Only the document step lands here: a sample that did not download, or an upload the
        // browser can no longer read (moved or deleted since it was chosen).
        $("pg-results").hidden = true;
        $("pg-form-error").textContent = S.source === "upload"
          ? "The document could not be read. Choose it again."
          : "The sample document could not be downloaded. Check your connection and try again.";
      }).catch(function (err) {
        if (window.console) console.error(err);
        $("pg-form-error").textContent = "Something went wrong on this page. Reload it and try again.";
      }).then(function () {
        S.running = false;
        if (S.mode === "free") loadServerConfig();
        refresh();
      });
    }

    /* ---------- wiring */

    document.querySelectorAll('input[name="source"]').forEach(function (r) {
      r.addEventListener("change", function () { S.source = this.value; renderModels(); refresh(); });
    });
    document.querySelectorAll('input[name="mode"]').forEach(function (r) {
      r.addEventListener("change", function () { S.mode = this.value; renderModels(); refresh(); });
    });
    $("pg-live-sample").addEventListener("change", function () { renderModels(); refresh(); });
    $("pg-form").addEventListener("submit", function (e) {
      e.preventDefault();
      if (S.running) return;
      var why = blocker();
      if (why) { $("pg-form-error").textContent = why; return; }
      $("pg-form-error").textContent = "";
      var go = isLive() ? runLive() : showSamples();
      go.then(function () {
        var h = $("pg-results-h");
        if (!$("pg-results").hidden && h) h.scrollIntoView({ block: "start", behavior: "smooth" });
      });
    });

    wireUpload();
    wireModels();
    wireKeys();
    wireResults();
    renderModels();
    refresh();
    loadSamples().then(function () { renderModels(); refresh(); });
    if (api) {
      initAuth();
      loadServerConfig();
    }
  }

  return {
    esc: esc,
    sniffType: sniffType,
    estimatePdfPages: estimatePdfPages,
    validateFile: validateFile,
    renderMarkdown: renderMarkdown,
    authHeaders: authHeaders,
    Api: Api,
    ApiError: ApiError,
    pollJobs: pollJobs,
    errorSentence: errorSentence,
    loadKeys: loadKeys,
    saveKeys: saveKeys,
    forgetKeys: forgetKeys,
    KEY_STORE: KEY_STORE,
    boot: boot,
  };
});
