// Offline tests for website/assets/playground.js (no browser, no network):
//   node --test website/tests/
// The page's pure helpers are exported when the file is loaded as a CommonJS module.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const pg = require("../assets/playground.js");

const LIMITS = {
  max_file_bytes: 4 * 1024 * 1024,
  accepted_types: ["application/pdf", "image/png", "image/jpeg"],
  models_per_run: 3,
  pages_per_run: 10,
};

test("sniffType reads magic bytes, not names", () => {
  assert.equal(pg.sniffType([0x25, 0x50, 0x44, 0x46, 0x2d, 0x31]), "application/pdf");
  assert.equal(pg.sniffType([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), "image/png");
  assert.equal(pg.sniffType([0xff, 0xd8, 0xff, 0xe0]), "image/jpeg");
  assert.equal(pg.sniffType([0x3c, 0x68, 0x74, 0x6d, 0x6c]), ""); // <html
  assert.equal(pg.sniffType([0x25, 0x50]), "");
  assert.equal(pg.sniffType(null), "");
});

test("estimatePdfPages counts page objects but not the Pages tree", () => {
  assert.equal(pg.estimatePdfPages("<< /Type /Pages /Kids [] >> << /Type /Page >> << /Type/Page>>"), 2);
  assert.equal(pg.estimatePdfPages(""), 0);
});

test("validateFile enforces type, size and pages", () => {
  assert.equal(pg.validateFile(null, LIMITS), "Choose a file first.");
  assert.match(pg.validateFile({ type: "", size: 1, pages: 0 }, LIMITS), /not a PDF, PNG or JPEG/);
  assert.match(pg.validateFile({ type: "image/gif", size: 1, pages: 0 }, LIMITS), /not accepted/);
  assert.match(pg.validateFile({ type: "application/pdf", size: 5 * 1024 * 1024, pages: 1 }, LIMITS), /5 MB.*4 MB/);
  assert.match(pg.validateFile({ type: "application/pdf", size: 1000, pages: 11 }, LIMITS), /about 11 pages/);
  assert.equal(pg.validateFile({ type: "image/png", size: 1000, pages: 0 }, LIMITS), "");
});

test("renderMarkdown builds structure from escaped text", () => {
  const html = pg.renderMarkdown("# Title\n\nSome **bold** and *it* text.\n\n- a\n- b\n\n| x | y |\n|---|---|\n| 1 | 2 |");
  assert.match(html, /<h3>Title<\/h3>/);
  assert.match(html, /<strong>bold<\/strong>/);
  assert.match(html, /<em>it<\/em>/);
  assert.match(html, /<ul><li>a<\/li><li>b<\/li><\/ul>/);
  assert.match(html, /<th>x<\/th><th>y<\/th>/);
  assert.match(html, /<td>1<\/td><td>2<\/td>/);
});

test("renderMarkdown never emits active content", () => {
  const evil = [
    "<script>alert(1)</script>",
    '<img src=x onerror="alert(1)">',
    '<a href="javascript:alert(1)">x</a>',
    "[click](javascript:alert(1))",
    "[click](data:text/html,<script>alert(1)</script>)",
    '<b onclick="alert(1)">bold</b>',
    '<td style="x" onmouseover="alert(1)" colspan="2">c</td>',
    "<table><tr><td onclick=alert(1)>c</td></tr></table>",
    '<svg onload="alert(1)"></svg>',
    '<iframe src="https://evil.example"></iframe>',
    "![x](https://evil.example/pixel.png)",
    '```\n<script>alert(1)</script>\n```',
    "> <img src=x onerror=alert(1)>",
    '<<script>script>alert(1)<</script>/script>',
    '<b title="a>b" onmouseover=alert(1)>t</b>',
  ].join("\n\n");
  const html = pg.renderMarkdown(evil);
  assert.doesNotMatch(html, /<script/i);
  assert.doesNotMatch(html, /<img/i);
  assert.doesNotMatch(html, /<svg/i);
  assert.doesNotMatch(html, /<iframe/i);
  assert.doesNotMatch(html, /href="(?!https?:)/i);
  assert.doesNotMatch(html, /<[^>]*\son[a-z]+\s*=/i, "no event-handler attribute inside a tag");
  assert.doesNotMatch(html, /<[^>]*\sstyle\s*=/i);
  assert.match(html, /\[image: x\]/);
});

test("renderMarkdown keeps http(s) links with safe rel", () => {
  const html = pg.renderMarkdown("[docs](https://puffinparse.com/docs/)");
  assert.match(html, /<a href="https:\/\/puffinparse.com\/docs\/" rel="nofollow noopener noreferrer" target="_blank">docs<\/a>/);
});

test("renderMarkdown keeps provider HTML tables with numeric spans only", () => {
  const html = pg.renderMarkdown('<table><tr><th colspan="2" class="x">H</th></tr><tr><td rowspan=3>a</td></tr></table>');
  assert.match(html, /<th colspan="2">H<\/th>/);
  assert.match(html, /<td rowspan="3">a<\/td>/);
  assert.doesNotMatch(html, /class="x"/);
});

test("authHeaders: free tier sends a bearer, own keys send one header per chosen provider", () => {
  assert.deepEqual(pg.authHeaders({ mode: "free", token: "jwt" }, ["reducto"]), { Authorization: "Bearer jwt" });
  // The token is read on every call, so a session refreshed mid-run is picked up by the next poll.
  const session = { access_token: "jwt-1" };
  const live = { mode: "free", get token() { return session.access_token; } };
  assert.deepEqual(pg.authHeaders(live), { Authorization: "Bearer jwt-1" });
  session.access_token = "jwt-2";
  assert.deepEqual(pg.authHeaders(live), { Authorization: "Bearer jwt-2" });
  const byok = pg.authHeaders({ mode: "byok", keys: { reducto: "r", extend: "e", llamaparse: "" } }, ["reducto", "llamaparse"]);
  assert.deepEqual(byok, { "x-provider-key-reducto": "r" });
  assert.equal("Authorization" in byok, false);
  assert.deepEqual(pg.authHeaders({ mode: "byok", keys: { "Bad Name": "x" } }), {});
  assert.deepEqual(pg.authHeaders(null), {});
});

function fakeFetch(routes, calls) {
  return async (url, init) => {
    calls.push({ url, init });
    const handler = routes.shift();
    if (!handler) throw new Error("unexpected request " + url);
    const { status = 200, body = null, headers = {} } = typeof handler === "function" ? handler(url, init) : handler;
    if (status === 0) throw new TypeError("network");
    return {
      ok: status >= 200 && status < 300,
      status,
      headers: { get: (k) => headers[k.toLowerCase()] ?? null },
      text: async () => (body == null ? "" : JSON.stringify(body)),
    };
  };
}

test("Api.submit posts multipart to /v1/playground/runs with the right auth", async () => {
  const calls = [];
  const api = new pg.Api("https://api.example/", fakeFetch([{ status: 202, body: { id: "run1", jobs: [] } }], calls));
  const out = await api.submit({
    file: new Blob(["%PDF-1.7"], { type: "application/pdf" }),
    filename: "a.pdf",
    models: ["reducto/standard", "reducto/r-1", "extend/parse_light"],
    auth: { mode: "byok", keys: { reducto: "rk", extend: "ek", llamaparse: "lk" } },
    turnstile: "ignored-for-byok",
  });
  assert.equal(out.id, "run1");
  const { url, init } = calls[0];
  assert.equal(url, "https://api.example/v1/playground/runs");
  assert.equal(init.method, "POST");
  assert.equal(init.credentials, "omit");
  assert.equal(init.referrerPolicy, "no-referrer");
  assert.deepEqual(init.headers, { "x-provider-key-reducto": "rk", "x-provider-key-extend": "ek" });
  assert.deepEqual(init.body.getAll("models"), ["reducto/standard", "reducto/r-1", "extend/parse_light"]);
  assert.equal(init.body.get("turnstile_token"), null);
  assert.equal(init.body.get("file").name, "a.pdf");
});

test("Api.submit on the free tier carries the Turnstile token and the bearer", async () => {
  const calls = [];
  const api = new pg.Api("https://api.example", fakeFetch([{ status: 202, body: {} }], calls));
  await api.submit({ file: new Blob(["x"]), models: ["llamaparse/fast"], auth: { mode: "free", token: "jwt" }, turnstile: "ts" });
  assert.equal(calls[0].init.body.get("turnstile_token"), "ts");
  assert.deepEqual(calls[0].init.headers, { Authorization: "Bearer jwt" });
});

test("Api errors carry the gateway's type, details and Retry-After", async () => {
  const api = new pg.Api(
    "https://api.example",
    fakeFetch(
      [
        { status: 429, body: { error: { type: "quota_exceeded", message: "m", details: { requested: 30, remaining: 4 } } } },
        { status: 429, body: { error: { type: "ip_rate_limited", message: "m" } }, headers: { "retry-after": "120" } },
        { status: 0 },
        { status: 502, body: "not json" },
      ],
      []
    )
  );
  const e1 = await api.config("t").catch((e) => e);
  assert.equal(e1.type, "quota_exceeded");
  assert.match(pg.errorSentence(e1), /needs 30 model-pages and you have 4 left/);
  const e2 = await api.config().catch((e) => e);
  assert.equal(e2.retryAfter, 120);
  assert.match(pg.errorSentence(e2), /Try again in 2 min/);
  const e3 = await api.config().catch((e) => e);
  assert.equal(e3.type, "network_error");
  const e4 = await api.config().catch((e) => e);
  assert.equal(e4.type, "http_502");
});

test("errorSentence covers every documented error type", () => {
  const types = [
    "unauthorized", "turnstile_failed", "quota_exceeded", "free_tier_unavailable", "ip_rate_limited",
    "too_many_pages", "too_many_models", "payload_too_large", "unsupported_media_type", "missing_provider_key",
    "model_not_allowed", "authentication_error", "network_error",
  ];
  for (const type of types) {
    const s = pg.errorSentence({ type, details: {}, message: "" });
    assert.ok(s && s !== "Something went wrong.", type);
  }
  assert.match(pg.errorSentence({ type: "free_tier_unavailable", details: { reason: "budget_exhausted" } }), /00:00 UTC/);
});

test("pollJobs backs off, settles each job and retries transient failures", async () => {
  const states = {
    a: [{ status: 0 }, { body: { status: "pending" } }, { body: { status: "succeeded", result: { markdown: "# A" } } }],
    b: [{ body: { status: "failed", error: { type: "provider_error", message: "boom" } } }],
    c: [{ status: 404, body: { error: { type: "not_found", message: "gone" } } }],
  };
  const api = {
    job: async (id) => {
      const next = states[id].shift();
      if (next.status === 0) throw new pg.ApiError(0, null);
      if (next.status) throw new pg.ApiError(next.status, next.body);
      return next.body;
    },
  };
  const jobs = ["a", "b", "c"].map((id) => ({ id, model: "reducto/standard", status: "pending" }));
  jobs.push({ model: "extend/parse_light", status: "failed" }); // rejected at submit: never polled
  const delays = [];
  const updates = [];
  const outcome = await pg.pollJobs(api, jobs, null, (j) => updates.push(j.id), {
    sleep: async (ms) => { delays.push(ms); },
  });
  assert.equal(outcome, "done");
  assert.deepEqual(jobs.map((j) => j.status), ["succeeded", "failed", "failed", "failed"]);
  assert.equal(jobs[0].result.markdown, "# A");
  assert.equal(jobs[2].error.type, "not_found");
  assert.deepEqual(updates.sort(), ["a", "b", "c"]);
  assert.deepEqual(delays, [1500, 2000, 2500]);
});

test("pollJobs stops when asked and times out", async () => {
  const api = { job: async () => ({ status: "pending" }) };
  let stop = false;
  const jobs = [{ id: "x", model: "m/x", status: "pending" }];
  const stopped = await pg.pollJobs(api, jobs, null, () => {}, { sleep: async () => { stop = true; }, stop: () => stop });
  assert.equal(stopped, "stopped");
  let t = 0;
  const timedOut = await pg.pollJobs(api, jobs, null, () => {}, {
    sleep: async () => { t += 60_000; },
    now: () => t,
    maxMs: 120_000,
  });
  assert.equal(timedOut, "timeout");
});

test("remembered keys round-trip, drop blanks and survive blocked storage", () => {
  const mem = new Map();
  const storage = {
    getItem: (k) => (mem.has(k) ? mem.get(k) : null),
    setItem: (k, v) => mem.set(k, v),
    removeItem: (k) => mem.delete(k),
  };
  pg.saveKeys(storage, { reducto: "r", extend: "" });
  assert.deepEqual(pg.loadKeys(storage), { reducto: "r" });
  pg.saveKeys(storage, { reducto: "" });
  assert.equal(mem.has(pg.KEY_STORE), false);
  pg.saveKeys(storage, { reducto: "r" });
  pg.forgetKeys(storage);
  assert.equal(pg.loadKeys(storage), null);
  const blocked = { getItem() { throw new Error("blocked"); }, setItem() { throw new Error("blocked"); }, removeItem() { throw new Error("blocked"); } };
  assert.equal(pg.loadKeys(blocked), null);
  pg.saveKeys(blocked, { reducto: "r" });
  pg.forgetKeys(blocked);
  storage.setItem(pg.KEY_STORE, "not json");
  assert.equal(pg.loadKeys(storage), null);
});
