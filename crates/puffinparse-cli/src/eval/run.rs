//! Running chosen models over a user's dataset.
//!
//! A run lives in one folder, `<dataset>/.puffinparse/runs/<run_id>/` unless told otherwise:
//!
//! ```text
//! run.json         plan, estimate, status (rewritten at start and end)
//! results.jsonl    one line per finished call, appended as it happens (the last line per
//!                  model and document wins, so a resumed run simply appends)
//! outputs/<provider>_<model>/<doc id>.md   each model's markdown per document
//! reviews.json     verdicts recorded by the agent or in the viewer (see `analyze.rs`)
//! report.md        written by `eval analyze --write-report`
//! ```
//!
//! Two ways to pay, chosen per run:
//!
//! - **keys** (default): the core calls each provider directly with the key in this process's
//!   environment. Documents go only to the providers of the chosen models.
//! - **gateway**: every call goes to a PuffinParse gateway (`puffinparse serve`) at
//!   `PUFFINPARSE_GATEWAY_URL` with the key in `PUFFINPARSE_GATEWAY_KEY`; the gateway's provider
//!   keys pay and the key's budget caps the spend. A `402 budget_exceeded` stops the run.
//!
//! Cost control: the plan prices every call at list price before the first one; a run whose
//! estimate is above `max_cost_usd` does not start, and a running total above it stops new calls.

use super::dataset::{self, Dataset, DatasetDoc};
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use puffinparse_core::{self as core, DocumentRequest, Mode, OutputFormat};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const ENV_GATEWAY_URL: &str = "PUFFINPARSE_GATEWAY_URL";
pub const ENV_GATEWAY_KEY: &str = "PUFFINPARSE_GATEWAY_KEY";

/// Most models in one run.
pub const MAX_MODELS: usize = 10;

// ---- files ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Complete,
    Cancelled,
    /// Stopped because the spend reached `max_cost_usd` or the gateway key's budget.
    BudgetStopped,
    /// The process ended while the run was in progress; resume it to finish.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelEstimate {
    pub pages: u32,
    /// `None` when the model has no list price.
    pub usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Estimate {
    pub documents: usize,
    pub calls: usize,
    pub per_model: BTreeMap<String, ModelEstimate>,
    pub total_usd: f64,
    pub unpriced_models: Vec<String>,
    /// Documents whose page count is unknown (office files): estimated as one page each.
    pub unknown_page_documents: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub run_id: String,
    pub created_at: String,
    pub puffinparse_version: String,
    /// Canonical path of the dataset folder.
    pub dataset_root: String,
    pub models: Vec<String>,
    /// Page selection applied to every document (e.g. `"1-3"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<String>,
    /// `"keys"` or `"gateway"`.
    pub via: String,
    pub docs: Vec<DatasetDoc>,
    pub estimate: Estimate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// The user's requirements (monthly volume, tolerance, …) as given to `recommend_models`, so
    /// the analysis can project monthly cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<serde_json::Value>,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One finished call (a line of `results.jsonl`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CallRecord {
    pub model: String,
    pub doc: String,
    pub ok: bool,
    #[serde(default)]
    pub pages: u32,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub chars: usize,
    /// The call succeeded but returned no text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub empty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_job_id: Option<String>,
    #[serde(default)]
    pub started_at: String,
}

pub fn model_slug(model: &str) -> String {
    model.replace('/', "_")
}

/// Where a model's output for a document is saved.
pub fn output_path(run_dir: &Path, model: &str, doc: &str) -> PathBuf {
    run_dir.join("outputs").join(model_slug(model)).join(format!("{doc}.md"))
}

pub fn manifest_path(run_dir: &Path) -> PathBuf {
    run_dir.join("run.json")
}

pub fn results_path(run_dir: &Path) -> PathBuf {
    run_dir.join("results.jsonl")
}

pub fn load_manifest(run_dir: &Path) -> Result<RunManifest, String> {
    let p = manifest_path(run_dir);
    let raw = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    let mut m: RunManifest = serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", p.display()))?;
    // A dataset folder that was moved or copied (with its runs) to another place or machine: the
    // recorded root is gone, but the run still sits at `<dataset>/.puffinparse/runs/<id>`.
    if !Path::new(&m.dataset_root).is_dir() {
        if let Some(root) = moved_dataset_root(run_dir) {
            m.dataset_root = root.to_string_lossy().into_owned();
        }
    }
    Ok(m)
}

fn moved_dataset_root(run_dir: &Path) -> Option<PathBuf> {
    let dir = std::fs::canonicalize(run_dir).ok()?;
    let runs = dir.parent()?;
    let hidden = runs.parent()?;
    (runs.file_name()? == "runs" && hidden.file_name()? == ".puffinparse")
        .then(|| hidden.parent().map(Path::to_path_buf))?
}

pub fn write_manifest(run_dir: &Path, m: &RunManifest) -> Result<(), String> {
    let p = manifest_path(run_dir);
    let tmp = p.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(m).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("cannot write {}: {e}", p.display()))
}

/// Every record of a run, last one per (model, document) winning. A torn last line (the process
/// died mid-write) is skipped.
pub fn load_results(run_dir: &Path) -> BTreeMap<(String, String), CallRecord> {
    let mut out = BTreeMap::new();
    if let Ok(raw) = std::fs::read_to_string(results_path(run_dir)) {
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(r) = serde_json::from_str::<CallRecord>(line) {
                out.insert((r.model.clone(), r.doc.clone()), r);
            }
        }
    }
    out
}

/// Resolve a run reference: a run folder, a `run.json`, or a run id under a dataset's
/// `.puffinparse/runs/`.
pub fn resolve_run_dir(reference: &str, dataset: Option<&Path>) -> Result<PathBuf, String> {
    let p = PathBuf::from(reference);
    if p.is_dir() && manifest_path(&p).is_file() {
        return Ok(p);
    }
    if p.is_file() && p.file_name().is_some_and(|n| n == "run.json") {
        return Ok(p.parent().map(Path::to_path_buf).unwrap_or_default());
    }
    if let Some(ds) = dataset {
        let candidate = runs_dir(ds).join(reference);
        if manifest_path(&candidate).is_file() {
            return Ok(candidate);
        }
    }
    Err(format!("no run found at '{reference}' (pass the run folder that holds run.json)"))
}

pub fn runs_dir(dataset_root: &Path) -> PathBuf {
    dataset_root.join(".puffinparse").join("runs")
}

// ---- planning ------------------------------------------------------------------------------------

/// How the calls are paid for.
#[derive(Clone)]
pub enum Via {
    Keys,
    Gateway { url: String, key: String },
}

impl std::fmt::Debug for Via {
    // Never print the gateway key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Via::Keys => write!(f, "Keys"),
            Via::Gateway { url, .. } => write!(f, "Gateway {{ url: {url:?}, key: [redacted] }}"),
        }
    }
}

impl Via {
    pub fn name(&self) -> &'static str {
        match self {
            Via::Keys => "keys",
            Via::Gateway { .. } => "gateway",
        }
    }

    /// The gateway from `PUFFINPARSE_GATEWAY_URL` / `PUFFINPARSE_GATEWAY_KEY`.
    pub fn gateway_from_env() -> Result<Self, String> {
        let url = std::env::var(ENV_GATEWAY_URL).ok().filter(|v| !v.trim().is_empty());
        let key = std::env::var(ENV_GATEWAY_KEY).ok().filter(|v| !v.trim().is_empty());
        match (url, key) {
            (Some(url), Some(key)) => {
                let url = url.trim().trim_end_matches('/').to_string();
                if !(url.starts_with("https://")
                    || url.starts_with("http://127.0.0.1")
                    || url.starts_with("http://localhost"))
                {
                    return Err(format!("{ENV_GATEWAY_URL} must be an https URL (or http on localhost)"));
                }
                Ok(Via::Gateway { url, key: key.trim().to_string() })
            }
            _ => Err(format!(
                "paying through a gateway needs {ENV_GATEWAY_URL} and {ENV_GATEWAY_KEY} in the environment the \
                 server starts from. PuffinParse-hosted credits are not available yet; use your own provider keys \
                 (the default), or a key from a PuffinParse gateway someone runs for you."
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub dataset: PathBuf,
    pub models: Vec<String>,
    pub pages: Option<String>,
    /// Only the first N documents (in id order).
    pub limit: Option<usize>,
    /// Only documents whose id contains this.
    pub filter: Option<String>,
    pub max_cost_usd: Option<f64>,
    /// Parent folder for the run folder (default `<dataset>/.puffinparse/runs`).
    pub out_dir: Option<PathBuf>,
    pub requirements: Option<serde_json::Value>,
}

/// Validate models (parse mode), select documents and price the run. No network.
pub fn plan(spec: &RunSpec, via: &Via) -> Result<(Dataset, RunManifest), String> {
    let mut ds = dataset::scan(&spec.dataset)?;
    if let Some(f) = &spec.filter {
        ds.docs.retain(|d| d.id.contains(f.as_str()));
    }
    if let Some(n) = spec.limit {
        ds.docs.truncate(n);
    }
    if ds.docs.is_empty() {
        return Err("no documents selected".into());
    }
    let mut models: Vec<String> = Vec::new();
    for m in &spec.models {
        let q = core::ModelRef::parse_for(m, Mode::Parse).map_err(|e| e.to_string())?.qualified();
        if !models.contains(&q) {
            models.push(q);
        }
    }
    if models.is_empty() {
        return Err("name at least one model".into());
    }
    if models.len() > MAX_MODELS {
        return Err(format!("a run takes at most {MAX_MODELS} models ({} given)", models.len()));
    }
    let selection = match &spec.pages {
        Some(p) => Some(core::util::parse_page_ranges(p).map_err(|e| e.to_string())?),
        None => None,
    };
    let estimate = estimate(&ds.docs, &models, selection.as_deref());
    let now = chrono::Utc::now();
    let manifest = RunManifest {
        run_id: format!("run-{}", now.format("%Y%m%dT%H%M%SZ")),
        created_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        puffinparse_version: core::VERSION.to_string(),
        dataset_root: ds.root.to_string_lossy().into_owned(),
        models,
        pages: spec.pages.clone(),
        via: via.name().to_string(),
        docs: ds.docs.clone(),
        estimate,
        max_cost_usd: spec.max_cost_usd,
        requirements: spec.requirements.clone(),
        status: RunStatus::Running,
        finished_at: None,
        note: None,
    };
    Ok((ds, manifest))
}

pub fn estimate(docs: &[DatasetDoc], models: &[String], selection: Option<&[(u32, Option<u32>)]>) -> Estimate {
    let pages: u32 = docs.iter().map(|d| d.billed_pages(selection)).sum();
    let mut per_model = BTreeMap::new();
    let mut unpriced = Vec::new();
    let mut total = 0.0;
    for m in models {
        let usd = core::pricing::estimate_cost(m, Mode::Parse, pages);
        match usd {
            Some(u) => total += u,
            None => unpriced.push(m.clone()),
        }
        per_model.insert(m.clone(), ModelEstimate { pages, usd: usd.map(|u| (u * 10_000.0).round() / 10_000.0) });
    }
    Estimate {
        documents: docs.len(),
        calls: docs.len() * models.len(),
        per_model,
        total_usd: (total * 10_000.0).round() / 10_000.0,
        unpriced_models: unpriced,
        unknown_page_documents: docs.iter().filter(|d| d.pages.is_none()).count(),
    }
}

/// A prepared run: its folder exists, `run.json` is written and the calls still to make are known.
pub struct Prepared {
    pub dir: PathBuf,
    pub manifest: RunManifest,
    pub dataset_root: PathBuf,
    /// (model, doc id) pairs already done successfully.
    pub done: HashSet<(String, String)>,
    /// Spend already recorded (resumed runs).
    pub spent: f64,
}

impl Prepared {
    pub fn remaining_calls(&self) -> usize {
        self.manifest.docs.len() * self.manifest.models.len() - self.done.len()
    }
}

/// Create the run folder for a fresh plan. Refuses to start a priced run above `max_cost_usd`.
pub fn prepare_new(spec: &RunSpec, via: &Via, mut manifest: RunManifest) -> Result<Prepared, String> {
    check_budget(&manifest.estimate, spec.max_cost_usd)?;
    let parent = spec.out_dir.clone().unwrap_or_else(|| runs_dir(Path::new(&manifest.dataset_root)));
    let mut dir = parent.join(&manifest.run_id);
    let mut n = 2;
    while dir.exists() {
        dir = parent.join(format!("{}-{n}", manifest.run_id));
        n += 1;
    }
    manifest.run_id = dir.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(manifest.run_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    manifest.via = via.name().to_string();
    write_manifest(&dir, &manifest)?;
    Ok(Prepared {
        dataset_root: PathBuf::from(&manifest.dataset_root),
        dir,
        manifest,
        done: HashSet::new(),
        spent: 0.0,
    })
}

/// Reopen an existing run to finish its missing or failed calls.
pub fn prepare_resume(dir: &Path, via: &Via, max_cost_usd: Option<f64>) -> Result<Prepared, String> {
    let mut manifest = load_manifest(dir)?;
    let results = load_results(dir);
    let done: HashSet<(String, String)> =
        results.values().filter(|r| r.ok).map(|r| (r.model.clone(), r.doc.clone())).collect();
    let spent: f64 = results.values().filter_map(|r| r.cost_usd).sum();
    if max_cost_usd.is_some() {
        manifest.max_cost_usd = max_cost_usd;
    }
    if let Some(max) = manifest.max_cost_usd {
        if spent >= max {
            return Err(format!(
                "this run has already spent ${spent:.2} of its ${max:.2} limit; raise max_cost_usd to continue"
            ));
        }
    }
    manifest.status = RunStatus::Running;
    manifest.finished_at = None;
    manifest.note = None;
    manifest.via = via.name().to_string();
    write_manifest(dir, &manifest)?;
    Ok(Prepared { dataset_root: PathBuf::from(&manifest.dataset_root), dir: dir.to_path_buf(), manifest, done, spent })
}

pub fn check_budget(est: &Estimate, max_cost_usd: Option<f64>) -> Result<(), String> {
    let priced = est.total_usd > 0.0;
    match max_cost_usd {
        Some(max) if est.total_usd > max => Err(format!(
            "estimated cost ${:.2} at list price is above max_cost_usd ${max:.2}; raise it after the user agrees, or run \
             fewer documents, pages or models",
            est.total_usd
        )),
        None if priced => Err(format!(
            "this run is estimated at ${:.2} at list price; pass max_cost_usd (at least the estimate) once the user has \
             agreed to the spend",
            est.total_usd
        )),
        _ => Ok(()),
    }
}

// ---- execution -----------------------------------------------------------------------------------

/// What a call returned.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub markdown: String,
    pub pages: u32,
    pub latency_ms: u64,
    pub cost_usd: Option<f64>,
    pub provider_job_id: Option<String>,
}

/// A call failed in a way that must stop the whole run (a spent gateway budget).
pub const STOP_KIND: &str = "budget_exceeded";

#[derive(Debug, Clone)]
pub struct CallError {
    pub kind: String,
    pub message: String,
    pub job_id: Option<String>,
}

impl From<core::Error> for CallError {
    fn from(e: core::Error) -> Self {
        let kind = serde_json::to_value(e.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        Self { kind, message: crate::mcp::error_text(&e), job_id: e.job_id }
    }
}

#[derive(Debug, Clone)]
pub struct CallInput {
    pub path: PathBuf,
    pub model: String,
    pub pages: Option<String>,
    pub timeout_secs: f64,
    pub max_retries: u32,
}

pub type Caller = Arc<dyn Fn(CallInput) -> BoxFuture<'static, Result<Outcome, CallError>> + Send + Sync>;

/// The caller for a payment route.
pub fn caller(via: &Via) -> Caller {
    match via.clone() {
        Via::Keys => Arc::new(|input: CallInput| Box::pin(call_direct(input))),
        Via::Gateway { url, key } => {
            let client = reqwest::Client::new();
            Arc::new(move |input: CallInput| Box::pin(call_gateway(client.clone(), url.clone(), key.clone(), input)))
        }
    }
}

async fn call_direct(input: CallInput) -> Result<Outcome, CallError> {
    let mut req = DocumentRequest::from_path(&input.path)
        .model(&input.model)
        .timeout_secs(input.timeout_secs)
        .max_retries(input.max_retries)
        .output(OutputFormat::Markdown);
    if let Some(p) = &input.pages {
        req = req.pages(p.clone());
    }
    if let Some(opts) = crate::bench::cache_busting_options(&input.model) {
        req = req.provider_options(opts);
    }
    let r = core::parse(req).await?;
    Ok(Outcome {
        markdown: r.markdown,
        pages: r.usage.pages,
        latency_ms: r.latency_ms,
        cost_usd: r.cost_usd,
        provider_job_id: r.provider_job_id,
    })
}

async fn call_gateway(
    client: reqwest::Client,
    url: String,
    key: String,
    input: CallInput,
) -> Result<Outcome, CallError> {
    let started = std::time::Instant::now();
    let bytes = tokio::fs::read(&input.path).await.map_err(|e| CallError {
        kind: "input".into(),
        message: format!("cannot read document: {e}"),
        job_id: None,
    })?;
    let filename =
        input.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
    let mut form = reqwest::multipart::Form::new()
        .text("model", input.model.clone())
        .text("output", "markdown")
        .text("timeout", format!("{}", input.timeout_secs))
        .text("max_retries", input.max_retries.to_string())
        .part("file", reqwest::multipart::Part::bytes(bytes).file_name(filename));
    if let Some(p) = &input.pages {
        form = form.text("pages", p.clone());
    }
    let resp = client
        .post(format!("{url}/v1/parse"))
        .bearer_auth(&key)
        .multipart(form)
        .timeout(std::time::Duration::from_secs_f64(input.timeout_secs + 60.0))
        .send()
        .await
        .map_err(|e| CallError {
            kind: if e.is_timeout() { "timeout".into() } else { "network".into() },
            // reqwest errors carry the URL, never the bearer token.
            message: format!("gateway request failed: {e}"),
            job_id: None,
        })?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let err = &body["error"];
        let kind = err["type"].as_str().unwrap_or("provider").trim_end_matches("_error").to_string();
        let message = err["message"].as_str().unwrap_or("gateway error").to_string();
        return Err(CallError {
            kind,
            message: format!("gateway {}: {message}", status.as_u16()),
            job_id: err["job_id"].as_str().map(str::to_string),
        });
    }
    let r: core::ParseResponse = serde_json::from_value(body).map_err(|e| CallError {
        kind: "provider".into(),
        message: format!("unexpected gateway response: {e}"),
        job_id: None,
    })?;
    Ok(Outcome {
        markdown: r.markdown,
        pages: r.usage.pages,
        latency_ms: if r.latency_ms > 0 { r.latency_ms } else { started.elapsed().as_millis() as u64 },
        cost_usd: r.cost_usd,
        provider_job_id: r.provider_job_id,
    })
}

/// Live progress of a run, shared with whoever reports it.
#[derive(Debug, Default)]
pub struct Progress {
    pub inner: Mutex<ProgressInner>,
    pub cancel: AtomicBool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ProgressInner {
    pub total_calls: usize,
    pub done: usize,
    pub failed: usize,
    pub spent_usd: f64,
    pub per_model: BTreeMap<String, ModelProgress>,
    /// The last few errors, newest last.
    pub recent_errors: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ModelProgress {
    pub done: usize,
    pub failed: usize,
    pub total: usize,
    pub spent_usd: f64,
}

impl Progress {
    pub fn snapshot(&self) -> ProgressInner {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Settings for [`execute`].
#[derive(Clone)]
pub struct ExecOptions {
    pub concurrency: usize,
    pub timeout_secs: f64,
    pub max_retries: u32,
}

/// Make every call still missing in `prep`, append each record to `results.jsonl`, save outputs,
/// and write the final status into `run.json`.
pub async fn execute(prep: Prepared, caller: Caller, opts: ExecOptions, progress: Arc<Progress>) -> RunManifest {
    let Prepared { dir, mut manifest, dataset_root, done, spent } = prep;
    let docs = manifest.docs.clone();
    {
        let mut p = progress.inner.lock().unwrap_or_else(|p| p.into_inner());
        p.total_calls = docs.len() * manifest.models.len();
        p.done = done.len();
        p.spent_usd = spent;
        for m in &manifest.models {
            let already = done.iter().filter(|(dm, _)| dm == m).count();
            p.per_model
                .insert(m.clone(), ModelProgress { done: already, failed: 0, total: docs.len(), spent_usd: 0.0 });
        }
    }
    let log = Arc::new(Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(results_path(&dir)).ok()));
    let stop_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let max_cost = manifest.max_cost_usd;
    let pages = manifest.pages.clone();

    let per_model = manifest.models.iter().map(|model| {
        let jobs: Vec<DatasetDoc> =
            docs.iter().filter(|d| !done.contains(&(model.clone(), d.id.clone()))).cloned().collect();
        let (model, dir, root, caller, log, progress, stop_reason, pages, opts) = (
            model.clone(),
            dir.clone(),
            dataset_root.clone(),
            caller.clone(),
            log.clone(),
            progress.clone(),
            stop_reason.clone(),
            pages.clone(),
            opts.clone(),
        );
        async move {
            stream::iter(jobs)
                .map(|doc| {
                    let (model, dir, root, caller, log, progress, stop_reason, pages, opts) = (
                        model.clone(),
                        dir.clone(),
                        root.clone(),
                        caller.clone(),
                        log.clone(),
                        progress.clone(),
                        stop_reason.clone(),
                        pages.clone(),
                        opts.clone(),
                    );
                    async move {
                        if progress.cancel.load(Ordering::Relaxed)
                            || stop_reason.lock().unwrap_or_else(|p| p.into_inner()).is_some()
                        {
                            return;
                        }
                        let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                        let input = CallInput {
                            path: root.join(&doc.id),
                            model: model.clone(),
                            pages,
                            timeout_secs: opts.timeout_secs,
                            max_retries: opts.max_retries,
                        };
                        let started = std::time::Instant::now();
                        let record = match caller(input).await {
                            Ok(o) => {
                                let path = output_path(&dir, &model, &doc.id);
                                if let Some(parent) = path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                let saved = std::fs::write(&path, &o.markdown);
                                CallRecord {
                                    model: model.clone(),
                                    doc: doc.id.clone(),
                                    ok: saved.is_ok(),
                                    pages: o.pages,
                                    latency_ms: o.latency_ms,
                                    cost_usd: o.cost_usd,
                                    chars: o.markdown.chars().count(),
                                    empty: o.markdown.trim().is_empty(),
                                    error: saved.err().map(|e| format!("could not save output: {e}")),
                                    error_kind: None,
                                    provider_job_id: o.provider_job_id,
                                    started_at,
                                }
                            }
                            Err(e) => {
                                if e.kind == STOP_KIND {
                                    *stop_reason.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.message.clone());
                                }
                                CallRecord {
                                    model: model.clone(),
                                    doc: doc.id.clone(),
                                    ok: false,
                                    latency_ms: started.elapsed().as_millis() as u64,
                                    error: Some(e.message),
                                    error_kind: Some(e.kind),
                                    provider_job_id: e.job_id,
                                    started_at,
                                    ..CallRecord::default()
                                }
                            }
                        };
                        if let Ok(mut guard) = log.lock() {
                            if let Some(f) = guard.as_mut() {
                                if let Ok(line) = serde_json::to_string(&record) {
                                    let _ = writeln!(f, "{line}");
                                }
                            }
                        }
                        let mut p = progress.inner.lock().unwrap_or_else(|p| p.into_inner());
                        let cost = record.cost_usd.unwrap_or(0.0);
                        p.spent_usd += cost;
                        p.done += 1;
                        if !record.ok {
                            p.failed += 1;
                            let msg = format!(
                                "{} on {}: {}",
                                record.model,
                                record.doc,
                                record.error.clone().unwrap_or_default()
                            );
                            p.recent_errors.push(msg);
                            let n = p.recent_errors.len();
                            if n > 5 {
                                p.recent_errors.drain(..n - 5);
                            }
                        }
                        if let Some(mp) = p.per_model.get_mut(&record.model) {
                            mp.done += 1;
                            mp.spent_usd += cost;
                            if !record.ok {
                                mp.failed += 1;
                            }
                        }
                        if let Some(max) = max_cost {
                            if p.spent_usd >= max {
                                let mut s = stop_reason.lock().unwrap_or_else(|p| p.into_inner());
                                if s.is_none() {
                                    *s = Some(format!("spend reached max_cost_usd ${max:.2}"));
                                }
                            }
                        }
                    }
                })
                .buffer_unordered(opts.concurrency.max(1))
                .collect::<Vec<()>>()
                .await;
        }
    });
    futures::future::join_all(per_model).await;

    let stop = stop_reason.lock().unwrap_or_else(|p| p.into_inner()).clone();
    manifest.status = if progress.cancel.load(Ordering::Relaxed) {
        RunStatus::Cancelled
    } else if stop.is_some() {
        RunStatus::BudgetStopped
    } else {
        RunStatus::Complete
    };
    manifest.note = stop;
    manifest.finished_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    if let Err(e) = write_manifest(&dir, &manifest) {
        tracing::warn!(error = %e, "eval: could not write the final run.json");
    }
    manifest
}

/// Runs whose `run.json` still says `running` but whose process is gone are reported as
/// interrupted (the caller knows which runs it is executing).
pub fn effective_status(m: &RunManifest, live: bool) -> RunStatus {
    if m.status == RunStatus::Running && !live {
        RunStatus::Interrupted
    } else {
        m.status
    }
}

/// Counts from `results.jsonl` (for runs not executing in this process).
pub fn counts_from_results(dir: &Path, m: &RunManifest) -> ProgressInner {
    let results = load_results(dir);
    let mut p = ProgressInner { total_calls: m.docs.len() * m.models.len(), ..Default::default() };
    let mut per: HashMap<&str, ModelProgress> = HashMap::new();
    for r in results.values() {
        let mp = per.entry(r.model.as_str()).or_default();
        mp.done += 1;
        p.done += 1;
        let c = r.cost_usd.unwrap_or(0.0);
        mp.spent_usd += c;
        p.spent_usd += c;
        if !r.ok {
            mp.failed += 1;
            p.failed += 1;
        }
    }
    for m_name in &m.models {
        let mut mp = per.remove(m_name.as_str()).unwrap_or_default();
        mp.total = m.docs.len();
        p.per_model.insert(m_name.clone(), mp);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::dataset::{tempdir, tiny_pdf};

    fn spec(dir: &Path, models: &[&str]) -> RunSpec {
        RunSpec {
            dataset: dir.to_path_buf(),
            models: models.iter().map(|s| s.to_string()).collect(),
            pages: None,
            limit: None,
            filter: None,
            max_cost_usd: Some(100.0),
            out_dir: None,
            requirements: None,
        }
    }

    fn dataset(n: usize) -> PathBuf {
        let dir = tempdir("run");
        for i in 0..n {
            std::fs::write(dir.join(format!("doc{i}.pdf")), tiny_pdf(2)).unwrap();
        }
        dir
    }

    /// A fake provider: echoes the file name, fails `doc1` for `bad/*` style models.
    fn fake(fail_doc: Option<&'static str>, stop_on: Option<&'static str>) -> Caller {
        Arc::new(move |input: CallInput| {
            Box::pin(async move {
                let name = input.path.file_name().unwrap().to_string_lossy().into_owned();
                if stop_on.is_some_and(|s| name.starts_with(s)) {
                    return Err(CallError { kind: STOP_KIND.into(), message: "budget spent".into(), job_id: None });
                }
                if fail_doc.is_some_and(|f| name.starts_with(f)) {
                    return Err(CallError {
                        kind: "provider".into(),
                        message: "boom".into(),
                        job_id: Some("j1".into()),
                    });
                }
                Ok(Outcome {
                    markdown: format!("# {name}\n\nmodel {}", input.model),
                    pages: 2,
                    latency_ms: 5,
                    cost_usd: Some(0.02),
                    provider_job_id: None,
                })
            })
        })
    }

    fn opts() -> ExecOptions {
        ExecOptions { concurrency: 2, timeout_secs: 5.0, max_retries: 0 }
    }

    #[test]
    fn plan_prices_every_call_and_refuses_an_unagreed_spend() {
        let dir = dataset(3);
        let (_, m) = plan(&spec(&dir, &["reducto", "llamaparse/agentic", "tesseract"]), &Via::Keys).unwrap();
        assert_eq!(m.models, ["reducto/standard", "llamaparse/agentic", "tesseract/default"]);
        assert_eq!(m.estimate.calls, 9);
        assert_eq!(m.estimate.per_model["reducto/standard"].pages, 6);
        assert!(m.estimate.total_usd > 0.0);
        assert!(check_budget(&m.estimate, None).unwrap_err().contains("pass max_cost_usd"));
        assert!(check_budget(&m.estimate, Some(0.001)).unwrap_err().contains("above max_cost_usd"));
        assert!(check_budget(&m.estimate, Some(100.0)).is_ok());
        // A free local run needs no limit.
        let (_, m) = plan(&spec(&dir, &["tesseract"]), &Via::Keys).unwrap();
        assert!(check_budget(&m.estimate, None).is_ok());
        assert!(plan(&spec(&dir, &["reducto/extract"]), &Via::Keys).is_err());
    }

    #[test]
    fn a_moved_dataset_still_finds_its_documents() {
        // Canonical, so the prefix matches the run folder's own (canonical) path on macOS, where
        // the temp dir is a symlink into /private.
        let dir = std::fs::canonicalize(dataset(1)).unwrap();
        let (_, m) = plan(&spec(&dir, &["tesseract"]), &Via::Keys).unwrap();
        let prep = prepare_new(&spec(&dir, &["tesseract"]), &Via::Keys, m).unwrap();
        let moved = tempdir("run-moved");
        let target = moved.join("copy");
        std::fs::rename(&dir, &target).unwrap();
        let run = target.join(prep.dir.strip_prefix(&dir).unwrap());
        let m = load_manifest(&run).unwrap();
        assert_eq!(Path::new(&m.dataset_root), std::fs::canonicalize(&target).unwrap());
        assert!(Path::new(&m.dataset_root).join("doc0.pdf").is_file());
    }

    #[tokio::test]
    async fn executes_saves_outputs_and_resumes_failures() {
        let dir = dataset(3);
        let s = spec(&dir, &["reducto", "extend"]);
        let (_, m) = plan(&s, &Via::Keys).unwrap();
        let prep = prepare_new(&s, &Via::Keys, m).unwrap();
        let run_dir = prep.dir.clone();
        assert!(run_dir.starts_with(dir.canonicalize().unwrap().join(".puffinparse/runs")));
        let progress = Arc::new(Progress::default());
        let out = execute(prep, fake(Some("doc1"), None), opts(), progress.clone()).await;
        assert_eq!(out.status, RunStatus::Complete);
        let snap = progress.snapshot();
        assert_eq!((snap.done, snap.failed), (6, 2));
        assert!((snap.spent_usd - 0.08).abs() < 1e-9);
        let md = std::fs::read_to_string(output_path(&run_dir, "reducto/standard", "doc0.pdf")).unwrap();
        assert!(md.contains("doc0.pdf"));
        let results = load_results(&run_dir);
        assert_eq!(results.len(), 6);
        assert_eq!(
            results[&("extend/parse_performance".into(), "doc1.pdf".into())].error_kind.as_deref(),
            Some("provider")
        );

        // Resume: only the two failed calls are made again.
        let prep = prepare_resume(&run_dir, &Via::Keys, None).unwrap();
        assert_eq!(prep.remaining_calls(), 2);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted: Caller = {
            let calls = calls.clone();
            let inner = fake(None, None);
            Arc::new(move |i| {
                calls.fetch_add(1, Ordering::Relaxed);
                inner(i)
            })
        };
        let out = execute(prep, counted, opts(), Arc::new(Progress::default())).await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(out.status, RunStatus::Complete);
        assert!(load_results(&run_dir).values().all(|r| r.ok));
        let counts = counts_from_results(&run_dir, &load_manifest(&run_dir).unwrap());
        assert_eq!((counts.done, counts.failed), (6, 0));
    }

    #[tokio::test]
    async fn a_spent_budget_stops_the_run() {
        let dir = dataset(4);
        let s = spec(&dir, &["reducto"]);
        let (_, m) = plan(&s, &Via::Keys).unwrap();
        let prep = prepare_new(&s, &Via::Keys, m).unwrap();
        let mut o = opts();
        o.concurrency = 1;
        let out = execute(prep, fake(None, Some("doc1")), o, Arc::new(Progress::default())).await;
        assert_eq!(out.status, RunStatus::BudgetStopped);
        assert_eq!(out.note.as_deref(), Some("budget spent"));

        // The running total stops new calls too.
        let mut s = spec(&dir, &["reducto"]);
        s.max_cost_usd = Some(0.05);
        let (_, mut m) = plan(&s, &Via::Keys).unwrap();
        m.estimate.total_usd = 0.0; // pretend the list price was lower than the real bill
        let prep = prepare_new(&s, &Via::Keys, m).unwrap();
        let mut o = opts();
        o.concurrency = 1;
        let progress = Arc::new(Progress::default());
        let out = execute(prep, fake(None, None), o, progress.clone()).await;
        assert_eq!(out.status, RunStatus::BudgetStopped);
        assert_eq!(progress.snapshot().done, 3);
    }

    #[tokio::test]
    async fn cancel_stops_new_calls() {
        let dir = dataset(3);
        let s = spec(&dir, &["reducto"]);
        let (_, m) = plan(&s, &Via::Keys).unwrap();
        let prep = prepare_new(&s, &Via::Keys, m).unwrap();
        let progress = Arc::new(Progress::default());
        progress.cancel.store(true, Ordering::Relaxed);
        let out = execute(prep, fake(None, None), opts(), progress.clone()).await;
        assert_eq!(out.status, RunStatus::Cancelled);
        assert_eq!(progress.snapshot().done, 0);
    }

    #[tokio::test]
    async fn the_gateway_caller_posts_the_file_and_maps_budget_errors() {
        use axum::extract::Multipart;
        use axum::routing::post;
        async fn parse(headers: axum::http::HeaderMap, mut mp: Multipart) -> axum::response::Response {
            use axum::response::IntoResponse;
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let mut model = String::new();
            let mut file_len = 0;
            while let Ok(Some(field)) = mp.next_field().await {
                match field.name() {
                    Some("model") => model = field.text().await.unwrap(),
                    Some("file") => file_len = field.bytes().await.unwrap().len(),
                    _ => {}
                }
            }
            if auth != "Bearer gw-key" {
                return (axum::http::StatusCode::UNAUTHORIZED, "no").into_response();
            }
            if model == "extend/parse_performance" {
                let body = serde_json::json!({"error": {"type": "budget_exceeded", "message": "key 'k' has spent $5"}});
                return (axum::http::StatusCode::PAYMENT_REQUIRED, axum::Json(body)).into_response();
            }
            let pages = vec![core::Page {
                page_number: 1,
                width: None,
                height: None,
                markdown: format!("bytes {file_len}"),
                text: String::new(),
                blocks: vec![],
            }];
            let usage = core::Usage { pages: 1, credits: None, provider_cost_usd: None };
            let mut r = core::ParseResponse::from_pages("reducto", &model, pages, usage);
            r.cost_usd = Some(0.015);
            axum::Json(r).into_response()
        }
        let app = axum::Router::new().route("/v1/parse", post(parse));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = dataset(1);
        let via = Via::Gateway { url: format!("http://{addr}"), key: "gw-key".into() };
        assert!(!format!("{via:?}").contains("gw-key"));
        let call = caller(&via);
        let input = |model: &str| CallInput {
            path: dir.join("doc0.pdf"),
            model: model.into(),
            pages: None,
            timeout_secs: 5.0,
            max_retries: 0,
        };
        let ok = call(input("reducto/standard")).await.unwrap();
        assert_eq!(ok.markdown, format!("bytes {}", tiny_pdf(2).len()));
        assert_eq!(ok.cost_usd, Some(0.015));
        let err = call(input("extend/parse_performance")).await.unwrap_err();
        assert_eq!(err.kind, STOP_KIND);
        assert!(err.message.contains("402"));
    }
}
