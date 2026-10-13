//! The "best model for my dataset" tools: `scan_dataset`, `recommend_models`, `run_dataset`,
//! `run_status`, `cancel_run`, `analyze_run`, `get_output`, `record_review`, `open_viewer`,
//! `anonymize_dataset`, and the `find_best_model` prompt that walks an agent through them.
//!
//! The engine is `crate::eval`; this file only validates arguments, applies the server's
//! allow-list and `--root`, and shapes results. Runs execute in the background inside the server
//! process (a run can take far longer than a client's tool timeout); `run_status` reports them.
//! Like the other tools, none accepts a key or a URL: provider keys and the gateway come from the
//! server's environment only.

use super::tools::{Args, Config, ToolOutput};
use super::Features;
use crate::eval::{analyze, anonymize, dataset, recommend, run, view};
use puffinparse_core::{self as core, Mode};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const NAMES: &[&str] = &[
    "scan_dataset",
    "recommend_models",
    "run_dataset",
    "run_status",
    "cancel_run",
    "analyze_run",
    "get_output",
    "record_review",
    "open_viewer",
    "anonymize_dataset",
];

const DEFAULT_OUTPUT_CHARS: usize = 8_000;
const MAX_OUTPUT_CHARS: usize = 100_000;

/// Runs executing in this process and viewers it serves, by canonical run folder.
#[derive(Debug, Default)]
pub struct EvalState {
    runs: Mutex<HashMap<PathBuf, Arc<run::Progress>>>,
    viewers: Mutex<HashMap<PathBuf, String>>,
}

impl EvalState {
    fn live(&self, dir: &Path) -> Option<Arc<run::Progress>> {
        self.runs.lock().unwrap_or_else(|p| p.into_inner()).get(dir).cloned()
    }
}

pub async fn call(cfg: &Config, name: &str, args: &Map<String, Value>) -> Option<Result<ToolOutput, String>> {
    Some(match name {
        "scan_dataset" => scan_dataset(cfg, args),
        "recommend_models" => recommend_models(cfg, args),
        "run_dataset" => run_dataset(cfg, args),
        "run_status" => run_status(cfg, args).await,
        "cancel_run" => cancel_run(cfg, args),
        "analyze_run" => analyze_run(cfg, args).await,
        "get_output" => get_output(cfg, args),
        "record_review" => record_review(cfg, args),
        "open_viewer" => open_viewer(cfg, args).await,
        "anonymize_dataset" => anonymize_dataset(cfg, args).await,
        _ => return None,
    })
}

fn ok(structured: Value, text: String) -> Result<ToolOutput, String> {
    Ok(ToolOutput::ok(vec![text], structured))
}

// ---- paths ---------------------------------------------------------------------------------------

/// A local path the server may read: canonical, and inside `--root` when one is set.
fn local_path(cfg: &Config, raw: &str) -> Result<PathBuf, String> {
    if raw.contains("://") {
        return Err("pass a local folder path, not a URL".into());
    }
    let p = std::fs::canonicalize(raw).map_err(|e| format!("cannot read {raw}: {e}"))?;
    check_root(cfg, &p)?;
    Ok(p)
}

/// A path that may not exist yet (an output folder): its closest existing ancestor must be inside
/// `--root`.
fn writable_path(cfg: &Config, raw: &str) -> Result<PathBuf, String> {
    if raw.contains("://") {
        return Err("pass a local folder path, not a URL".into());
    }
    let p = PathBuf::from(raw);
    let p = if p.is_absolute() { p } else { std::env::current_dir().map_err(|e| e.to_string())?.join(p) };
    let mut existing = p.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or("invalid path")?;
    }
    let canon = std::fs::canonicalize(existing).map_err(|e| e.to_string())?;
    check_root(cfg, &canon)?;
    Ok(canon.join(p.strip_prefix(existing).unwrap_or(Path::new(""))))
}

fn check_root(cfg: &Config, p: &Path) -> Result<(), String> {
    match &cfg.root {
        Some(root) if !p.starts_with(root) => Err(format!(
            "'{}' is outside the directory this server may read ({}); the server was started with --root.",
            p.display(),
            root.display()
        )),
        _ => Ok(()),
    }
}

fn run_dir(cfg: &Config, a: &Args) -> Result<PathBuf, String> {
    let raw = a.required_string("run")?;
    let dir = run::resolve_run_dir(&raw, None)?;
    local_path(cfg, &dir.to_string_lossy())
}

fn string_list(a: &Args, k: &str) -> Result<Vec<String>, String> {
    match a.get(k) {
        None => Ok(Vec::new()),
        Some(Value::Array(v)) => v
            .iter()
            .map(|x| {
                x.as_str().map(|s| s.trim().to_string()).ok_or_else(|| format!("argument '{k}' must contain strings"))
            })
            .collect(),
        Some(Value::String(s)) => Ok(s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()),
        Some(_) => Err(format!("argument '{k}' must be an array of strings")),
    }
}

fn number(a: &Args, k: &str) -> Result<Option<f64>, String> {
    match a.get(k) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| *x >= 0.0)
            .map(Some)
            .ok_or_else(|| format!("argument '{k}' must be a non-negative number")),
    }
}

// ---- scan ----------------------------------------------------------------------------------------

fn scan_dataset(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["path"])?;
    let path = local_path(cfg, &a.required_string("path")?)?;
    let ds = dataset::scan(&path)?;
    let mut by_type: HashMap<&str, (usize, u32)> = HashMap::new();
    for d in &ds.docs {
        let e = by_type.entry(d.kind.as_str()).or_default();
        e.0 += 1;
        e.1 += d.pages.unwrap_or(1);
    }
    let mut largest: Vec<&dataset::DatasetDoc> = ds.docs.iter().collect();
    largest.sort_by_key(|d| std::cmp::Reverse(d.pages.unwrap_or(0)));
    let runs = existing_runs(&ds.root);
    let structured = json!({
        "root": ds.root,
        "documents": ds.docs.len(),
        "total_pages": ds.total_pages(),
        "unknown_page_documents": ds.unknown_pages(),
        "by_type": by_type.iter().map(|(k, (n, p))| (k.to_string(), json!({ "documents": n, "pages": p }))).collect::<Map<_, _>>(),
        "with_ground_truth": ds.docs.iter().filter(|d| d.truth.is_some()).count(),
        "largest": largest.iter().take(5).map(|d| json!({ "id": d.id, "pages": d.pages })).collect::<Vec<_>>(),
        "sample": ds.docs.iter().take(20).map(|d| d.id.clone()).collect::<Vec<_>>(),
        "skipped": ds.skipped.iter().take(20).map(|s| json!({ "path": s.path, "reason": s.reason })).collect::<Vec<_>>(),
        "skipped_count": ds.skipped.len(),
        "existing_runs": runs,
        "next": "Ask the user only what this does not answer (monthly volume, budget per 1,000 pages, error \
                 tolerance, whether documents may leave their machine or need zero data retention, what matters \
                 in them), then call recommend_models. Offer anonymize_dataset if the documents hold personal data.",
    });
    let text = format!(
        "{} documents, {} pages in {}{}. {}",
        ds.docs.len(),
        ds.total_pages(),
        ds.root.display(),
        if ds.unknown_pages() > 0 {
            format!(" ({} with unknown page counts, counted as 1)", ds.unknown_pages())
        } else {
            String::new()
        },
        structured
    );
    ok(structured, text)
}

fn existing_runs(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(run::runs_dir(root)) {
        for e in rd.filter_map(|e| e.ok()) {
            if let Ok(m) = run::load_manifest(&e.path()) {
                out.push(json!({ "run": e.path(), "run_id": m.run_id, "models": m.models, "status": m.status, "created_at": m.created_at }));
            }
        }
    }
    out
}

// ---- recommend -----------------------------------------------------------------------------------

fn recommend_models(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(
        args,
        &[
            "monthly_pages",
            "max_usd_per_1k_pages",
            "error_tolerance",
            "priority",
            "zdr_required",
            "local_only",
            "needs",
            "top_n",
        ],
    )?;
    let parse_enum =
        |k: &str, default: &str| -> Result<Value, String> { Ok(json!(a.string(k)?.unwrap_or_else(|| default.into()))) };
    let req = recommend::Requirements {
        monthly_pages: number(&a, "monthly_pages")?.map(|n| n as u64),
        max_usd_per_1k_pages: number(&a, "max_usd_per_1k_pages")?,
        error_tolerance: serde_json::from_value(parse_enum("error_tolerance", "medium")?)
            .map_err(|_| "error_tolerance must be low, medium or high".to_string())?,
        priority: serde_json::from_value(parse_enum("priority", "balanced")?)
            .map_err(|_| "priority must be accuracy, balanced, cost or speed".to_string())?,
        zdr_required: a.bool("zdr_required")?.unwrap_or(false),
        local_only: a.bool("local_only")?.unwrap_or(false),
        needs: string_list(&a, "needs")?,
    };
    let top = a.count("top_n", 5, 12)?;
    let r = recommend::recommend(&req, top, &|m| cfg.allow.allows(m));
    let mut table = String::from(
        "| # | model | quality | $/1k pages | monthly | ZDR | status | key |\n|---|---|---|---|---|---|---|---|\n",
    );
    for c in &r.candidates {
        table.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            c.rank,
            c.model,
            c.quality.map(|q| format!("{q:.1}")).unwrap_or_else(|| "not benchmarked".into()),
            c.usd_per_1k_pages.map(|p| format!("${p}")).unwrap_or_else(|| "n/a".into()),
            c.monthly_usd.map(|p| format!("${p}")).unwrap_or_else(|| "–".into()),
            c.data_handling.zdr,
            c.verification,
            if c.key_ready { "ready" } else { c.env_var.unwrap_or("missing") }
        ));
    }
    let structured = serde_json::to_value(&r).map_err(|e| e.to_string())?;
    ok(
        structured.clone(),
        format!(
            "{table}\nPresent these with their reasons, data handling and caveats, and ask the user which to run on \
             their documents.\n\n{structured}"
        ),
    )
}

// ---- run -----------------------------------------------------------------------------------------

fn run_dataset(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(
        args,
        &[
            "dataset",
            "models",
            "pages",
            "limit",
            "filter",
            "max_cost_usd",
            "dry_run",
            "via",
            "requirements",
            "concurrency",
            "resume",
        ],
    )?;
    let via = match a.string("via")?.as_deref() {
        None | Some("keys") => run::Via::Keys,
        Some("gateway") => run::Via::gateway_from_env()?,
        Some(o) => return Err(format!("via must be \"keys\" or \"gateway\", not \"{o}\"")),
    };
    let concurrency = a.count("concurrency", 4, 16)?;
    let max_cost = number(&a, "max_cost_usd")?;
    let opts = run::ExecOptions { concurrency, timeout_secs: cfg.timeout_secs, max_retries: cfg.max_retries };

    if let Some(resume) = a.string("resume")? {
        let dir = local_path(cfg, &run::resolve_run_dir(&resume, None)?.to_string_lossy())?;
        if cfg.eval.live(&dir).is_some_and(|p| p.snapshot().done < p.snapshot().total_calls) {
            return Err("this run is still executing; call run_status".into());
        }
        let prep = run::prepare_resume(&dir, &via, max_cost)?;
        check_models(cfg, &prep.manifest.models, &via)?;
        let remaining = prep.remaining_calls();
        let id = prep.manifest.run_id.clone();
        spawn(cfg, prep, via, opts);
        let structured = json!({ "run": dir, "run_id": id, "resumed": true, "calls": remaining });
        return ok(
            structured.clone(),
            format!("Resumed run {id}: {remaining} calls to make. Poll run_status. {structured}"),
        );
    }

    let dataset = local_path(cfg, &a.required_string("dataset")?)?;
    let models = string_list(&a, "models")?;
    let mut resolved = Vec::new();
    for m in &models {
        resolved.push(super::tools::resolve_model(cfg, m, Mode::Parse)?);
    }
    let requirements = match a.get("requirements") {
        None => None,
        Some(Value::Object(o)) => Some(Value::Object(o.clone())),
        Some(_) => return Err("argument 'requirements' must be an object".into()),
    };
    let spec = run::RunSpec {
        dataset,
        models: resolved,
        pages: a.string("pages")?,
        limit: a.get("limit").map(|_| a.count("limit", 1, dataset::MAX_DOCUMENTS)).transpose()?,
        filter: a.string("filter")?,
        max_cost_usd: max_cost,
        out_dir: None,
        requirements,
    };
    let (_, manifest) = run::plan(&spec, &via)?;
    let missing = missing_keys(&manifest.models, &via);
    // Tests hand the mock providers their keys through `Config::endpoints`.
    #[cfg(test)]
    let missing: Vec<String> = missing.into_iter().filter(|_| cfg.endpoints.is_empty()).collect();
    let down = local_engines_down(&manifest.models, &via);
    let est = manifest.estimate.clone();
    let plan_json = json!({
        "dataset": manifest.dataset_root,
        "documents": est.documents,
        "models": manifest.models,
        "calls": est.calls,
        "pages_per_model": est.per_model.values().next().map(|m| m.pages),
        "estimate_usd": est.total_usd,
        "per_model": est.per_model,
        "unpriced_models": est.unpriced_models,
        "unknown_page_documents": est.unknown_page_documents,
        "via": via.name(),
        "missing_keys": missing,
        "engines_not_running": down,
    });
    if a.bool("dry_run")?.unwrap_or(false) {
        let text = format!(
            "Plan: {} documents x {} models = {} calls, estimated ${:.2} at list price. Nothing was sent. Show the \
             user this estimate and start the run with max_cost_usd once they agree.{}{} {plan_json}",
            est.documents,
            manifest.models.len(),
            est.calls,
            est.total_usd,
            if missing.is_empty() { String::new() } else { format!(" Missing keys: {}.", missing.join(", ")) },
            if down.is_empty() { String::new() } else { format!(" Not running: {}.", down.join("; ")) }
        );
        return ok(json!({ "dry_run": true, "plan": plan_json }), text);
    }
    if !missing.is_empty() {
        return Err(format!(
            "set {} in the environment this MCP server starts from (its config's env block), then restart it. Never \
             paste a key into the chat.",
            missing.join(", ")
        ));
    }
    if !down.is_empty() {
        return Err(format!("local engines not reachable, start them or drop them from models: {}", down.join("; ")));
    }
    let prep = run::prepare_new(&spec, &via, manifest)?;
    let dir = prep.dir.clone();
    let id = prep.manifest.run_id.clone();
    spawn(cfg, prep, via, opts);
    let structured = json!({ "run": dir, "run_id": id, "plan": plan_json });
    ok(
        structured.clone(),
        format!(
            "Started run {id} in the background ({} calls, estimated ${:.2}). Call run_status with run=\"{}\" to follow \
             it; outputs are saved as they arrive. {structured}",
            est.calls,
            est.total_usd,
            dir.display()
        ),
    )
}

fn missing_keys(models: &[String], via: &run::Via) -> Vec<String> {
    if matches!(via, run::Via::Gateway { .. }) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for m in models {
        let provider = m.split('/').next().unwrap_or(m);
        if let Some(p) = core::model::provider_info(provider) {
            let set = std::env::var(p.env_var).map(|v| !v.trim().is_empty()).unwrap_or(false);
            if p.key_required() && !set && !out.contains(&p.env_var.to_string()) {
                out.push(p.env_var.to_string());
            }
        }
    }
    out
}

/// Local engines among `models` that cannot be used right now (a gateway runs them on its side).
fn local_engines_down(models: &[String], via: &run::Via) -> Vec<String> {
    if matches!(via, run::Via::Gateway { .. }) || cfg!(test) {
        return Vec::new();
    }
    models
        .iter()
        .filter_map(|m| {
            let (provider, model) = m.split_once('/').unwrap_or((m.as_str(), ""));
            match crate::eval::local::status(provider, model) {
                Some(Err(e)) => Some(format!("{m}: {e}")),
                _ => None,
            }
        })
        .collect()
}

fn check_models(cfg: &Config, models: &[String], via: &run::Via) -> Result<(), String> {
    for m in models {
        super::tools::resolve_model(cfg, m, Mode::Parse)?;
    }
    let missing = missing_keys(models, via);
    if !missing.is_empty() {
        return Err(format!("set {} where this MCP server starts, then restart it", missing.join(", ")));
    }
    let down = local_engines_down(models, via);
    if down.is_empty() {
        Ok(())
    } else {
        Err(format!("local engines not reachable, start them or drop them from models: {}", down.join("; ")))
    }
}

fn spawn(cfg: &Config, prep: run::Prepared, via: run::Via, opts: run::ExecOptions) {
    let progress = Arc::new(run::Progress::default());
    cfg.eval.runs.lock().unwrap_or_else(|p| p.into_inner()).insert(prep.dir.clone(), progress.clone());
    let caller = caller_for(cfg, &via);
    tokio::spawn(async move {
        let m = run::execute(prep, caller, opts, progress).await;
        tracing::info!(run = %m.run_id, status = ?m.status, "puffinparse mcp: run finished");
    });
}

#[cfg(not(test))]
fn caller_for(_cfg: &Config, via: &run::Via) -> run::Caller {
    run::caller(via)
}

/// Tests point the direct route at the loopback mock providers in `Config::endpoints`.
#[cfg(test)]
fn caller_for(cfg: &Config, via: &run::Via) -> run::Caller {
    if !matches!(via, run::Via::Keys) || cfg.endpoints.is_empty() {
        return run::caller(via);
    }
    let endpoints = cfg.endpoints.clone();
    Arc::new(move |input: run::CallInput| {
        let endpoints = endpoints.clone();
        Box::pin(async move {
            let provider = input.model.split('/').next().unwrap_or("").to_string();
            let mut req = core::DocumentRequest::from_path(&input.path).model(&input.model).max_retries(0);
            if let Some((base, key)) = endpoints.get(&input.model).or_else(|| endpoints.get(&provider)) {
                req = req.base_url(base.clone()).api_key(key.clone());
            }
            let r = core::parse(req).await?;
            Ok(run::Outcome {
                markdown: r.markdown,
                pages: r.usage.pages,
                latency_ms: r.latency_ms,
                cost_usd: r.cost_usd,
                provider_job_id: r.provider_job_id,
            })
        })
    })
}

/// How long `run_status` waits for a running run by default, and at most (seconds). Short enough
/// for clients' tool-call timeouts, long enough that an agent never needs to sleep between checks.
const STATUS_WAIT_DEFAULT: usize = 30;
const STATUS_WAIT_MAX: usize = 50;

async fn run_status(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["run", "wait_seconds"])?;
    let dir = run_dir(cfg, &a)?;
    let wait = a.count("wait_seconds", STATUS_WAIT_DEFAULT, STATUS_WAIT_MAX)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait as u64);
    let mut manifest = run::load_manifest(&dir)?;
    let mut live = cfg.eval.live(&dir);
    while live.is_some() && manifest.status == run::RunStatus::Running && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        manifest = run::load_manifest(&dir)?;
        live = cfg.eval.live(&dir);
    }
    let executing = live.is_some() && manifest.status == run::RunStatus::Running;
    let progress = match (&live, executing) {
        (Some(p), true) => p.snapshot(),
        _ => run::counts_from_results(&dir, &manifest),
    };
    let status = run::effective_status(&manifest, executing);
    let structured = json!({
        "run": dir,
        "run_id": manifest.run_id,
        "status": status,
        "note": manifest.note,
        "progress": progress,
        "estimate_usd": manifest.estimate.total_usd,
        "max_cost_usd": manifest.max_cost_usd,
    });
    let next = match status {
        run::RunStatus::Running => "Still running; call run_status again (it waits for the run, no need to sleep first).",
        run::RunStatus::Complete => "Done. Call analyze_run.",
        run::RunStatus::Interrupted => "The server stopped while this run was going. Resume it with run_dataset(resume=...).",
        run::RunStatus::BudgetStopped => "Stopped at the spending limit. analyze_run works on what finished; resume with a higher max_cost_usd only if the user agrees.",
        run::RunStatus::Cancelled => "Cancelled. analyze_run works on what finished.",
    };
    ok(
        structured.clone(),
        format!(
            "{:?}: {} of {} calls finished ({} succeeded, {} failed), ${:.2} spent. {next} {structured}",
            status,
            progress.done,
            progress.total_calls,
            progress.done.saturating_sub(progress.failed),
            progress.failed,
            progress.spent_usd
        ),
    )
}

fn cancel_run(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["run"])?;
    let dir = run_dir(cfg, &a)?;
    match cfg.eval.live(&dir) {
        Some(p) => {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            ok(
                json!({ "run": dir, "cancelled": true }),
                "Cancelling: calls in flight finish, no new ones start.".into(),
            )
        }
        None => Err("this run is not executing in this server".into()),
    }
}

// ---- analysis and review -------------------------------------------------------------------------

async fn analyze_run(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["run", "write_report", "conclusion", "max_docs"])?;
    let dir = run_dir(cfg, &a)?;
    let write = a.bool("write_report")?.unwrap_or(false);
    let conclusion = a.string("conclusion")?;
    let max_docs = a.count("max_docs", 20, 500)?;
    let live = cfg.eval.live(&dir).is_some();
    let d = dir.clone();
    let analysis = tokio::task::spawn_blocking(move || -> Result<analyze::Analysis, String> {
        let manifest = run::load_manifest(&d)?;
        Ok(analyze::analyze_with(&d, &manifest, &run::load_results(&d), &analyze::load_reviews(&d), live))
    })
    .await
    .map_err(|e| e.to_string())??;
    let report = analyze::render_report(&analysis, conclusion.as_deref());
    let mut report_path = None;
    if write {
        let p = dir.join("report.md");
        std::fs::write(&p, &report).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
        report_path = Some(p);
    }
    let root = PathBuf::from(&analysis.dataset_root);
    let queue: Vec<Value> = analysis
        .review_queue
        .iter()
        .filter_map(|id| analysis.per_doc.iter().find(|d| &d.id == id))
        .map(|d| {
            json!({ "doc": d.id, "file": root.join(&d.id), "disagreement": d.disagreement,
                    "numbers_disagreement": d.numbers_disagreement, "outliers": d.outliers, "failed": d.failed,
                    "empty": d.empty, "agreement": d.agreement })
        })
        .collect();
    let next = if queue.is_empty() {
        "Every document worth checking has a verdict. If you have not yet, call analyze_run with write_report=true \
         and your conclusion, then open_viewer so the user can check visually."
    } else {
        "For each document in review_queue: read the original file yourself, call get_output, compare each model's \
         output with the page, and record_review a verdict per model. Then call analyze_run again with \
         write_report=true and your conclusion, and open_viewer so the user can check visually."
    };
    let mut docs: Vec<&analyze::DocReport> = analysis.per_doc.iter().collect();
    docs.sort_by(|x, y| y.disagreement.unwrap_or(0.0).total_cmp(&x.disagreement.unwrap_or(0.0)));
    let structured = json!({
        "run": dir,
        "run_id": analysis.run_id,
        "status": analysis.status,
        "documents": analysis.documents,
        "monthly_pages": analysis.monthly_pages,
        "per_model": analysis.per_model,
        "review_queue": queue,
        "per_doc": docs.into_iter().take(max_docs).collect::<Vec<_>>(),
        "notes": analysis.notes,
        "report": report_path,
        "next": next,
    });
    ok(structured.clone(), format!("{report}\n\n{structured}"))
}

fn get_output(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["run", "doc", "models", "max_chars"])?;
    let dir = run_dir(cfg, &a)?;
    let doc = a.required_string("doc")?;
    let manifest = run::load_manifest(&dir)?;
    let entry =
        manifest.docs.iter().find(|d| d.id == doc).ok_or_else(|| format!("document '{doc}' is not in this run"))?;
    let wanted = string_list(&a, "models")?;
    let models: Vec<String> = if wanted.is_empty() {
        manifest.models.clone()
    } else {
        wanted
            .iter()
            .map(|m| core::ModelRef::parse_for(m, Mode::Parse).map(|r| r.qualified()).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?
    };
    let max = a.count("max_chars", DEFAULT_OUTPUT_CHARS, MAX_OUTPUT_CHARS)?;
    let results = run::load_results(&dir);
    let mut text = format!(
        "Original: {} (read it yourself to compare)\n",
        Path::new(&manifest.dataset_root).join(&entry.id).display()
    );
    let mut outs = Vec::new();
    for m in &models {
        if !manifest.models.contains(m) {
            return Err(format!("model '{m}' is not in this run ({})", manifest.models.join(", ")));
        }
        match results.get(&(m.clone(), doc.clone())) {
            Some(r) if r.ok => {
                let body = std::fs::read_to_string(run::output_path(&dir, m, &doc)).unwrap_or_default();
                let total = body.chars().count();
                let shown: String = body.chars().take(max).collect();
                text.push_str(&format!(
                    "\n### {m}\n\n{}{}\n",
                    if shown.is_empty() { "(empty output)" } else { &shown },
                    if total > max { format!("\n\n[first {max} of {total} characters]") } else { String::new() }
                ));
                outs.push(
                    json!({ "model": m, "ok": true, "markdown": shown, "chars": total, "truncated": total > max }),
                );
            }
            Some(r) => {
                text.push_str(&format!("\n### {m}\n\nFailed: {}\n", r.error.clone().unwrap_or_default()));
                outs.push(json!({ "model": m, "ok": false, "error": r.error, "error_kind": r.error_kind }));
            }
            None => {
                text.push_str(&format!("\n### {m}\n\nNot run yet.\n"));
                outs.push(json!({ "model": m, "ok": false, "error": "not run yet" }));
            }
        }
    }
    let structured = json!({
        "doc": doc,
        "file": Path::new(&manifest.dataset_root).join(&entry.id),
        "pages": entry.pages,
        "outputs": outs,
    });
    ok(structured, text)
}

fn record_review(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["run", "doc", "model", "verdict", "notes"])?;
    let dir = run_dir(cfg, &a)?;
    let r = analyze::record_review(
        &dir,
        &a.required_string("doc")?,
        &a.required_string("model")?,
        &a.required_string("verdict")?,
        &a.string("notes")?.unwrap_or_default(),
        "agent",
    )?;
    let structured = serde_json::to_value(&r).map_err(|e| e.to_string())?;
    ok(structured, format!("Recorded: {} on {} is {}.", r.model, r.doc, r.verdict.replace('_', " ")))
}

async fn open_viewer(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["run", "open_browser"])?;
    let dir = run_dir(cfg, &a)?;
    let existing = cfg.eval.viewers.lock().unwrap_or_else(|p| p.into_inner()).get(&dir).cloned();
    let url = match existing {
        Some(u) => u,
        None => {
            let v = view::start(&dir, 0).await?;
            cfg.eval.viewers.lock().unwrap_or_else(|p| p.into_inner()).insert(dir.clone(), v.url.clone());
            v.url
        }
    };
    if a.bool("open_browser")?.unwrap_or(false) {
        view::open_browser(&url);
    }
    ok(
        json!({ "url": url, "run": dir }),
        format!(
            "Viewer for this run: {url} (on this machine only, for as long as this MCP server runs). Give the user the \
             link; it shows each document next to every model's output, and they can record their own verdicts. From a \
             terminal: puffinparse eval view {}",
            dir.display()
        ),
    )
}

async fn anonymize_dataset(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["dataset", "out", "terms", "kinds", "dpi"])?;
    let dataset = local_path(cfg, &a.required_string("dataset")?)?;
    let out = writable_path(cfg, &a.required_string("out")?)?;
    let opts = anonymize::Options {
        terms: string_list(&a, "terms")?,
        kinds: string_list(&a, "kinds")?,
        dpi: a.count("dpi", 150, 400)? as u32,
    };
    let report = anonymize::anonymize(&dataset, &out, &opts).await?;
    let structured = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    ok(
        structured.clone(),
        format!(
            "Wrote {} redacted copies to {} ({} redactions). Look at a few redacted pages before running models on \
             the copy, and use the copy as the dataset. {structured}",
            report.redacted_documents,
            report.output,
            report.redactions.values().sum::<usize>()
        ),
    )
}

// ---- definitions ---------------------------------------------------------------------------------

pub(crate) fn definitions(features: Features) -> Vec<Value> {
    let local =
        json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false });
    let writes_local =
        json!({ "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false });
    let billed =
        json!({ "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true });
    let any_object = json!({ "type": "object" });
    let run_prop =
        json!({ "type": "string", "description": "The run folder returned by run_dataset (it holds run.json)." });
    let t = |name: &str, title: &str, description: &str, input: Value, ann: &Value| {
        super::tools::tool(features, name, title, description.to_string(), input, any_object.clone(), ann.clone())
    };
    vec![
        t(
            "scan_dataset",
            "Scan a folder of documents",
            "Step 1 of finding the best model for the user's own documents. Counts the documents (PDF, images, office \
             files) in a local folder: pages, types, the largest ones, any .truth.md references, and earlier runs. No \
             network, nothing is sent anywhere. The prompt find_best_model describes the whole workflow.",
            json!({ "type": "object", "properties": {
                "path": { "type": "string", "description": "Local folder of representative documents (absolute path), or one document." }
            }, "required": ["path"] }),
            &local,
        ),
        t(
            "recommend_models",
            "Shortlist models for the user's needs",
            "Step 2. Ranks parse models for the user's requirements using PuffinParse's open benchmark (accuracy overall \
             and by document type), list prices, and each provider's documented data handling (zero data retention, \
             retention, training), with reasons and caveats per model and whether its key is set. Ask the user only \
             what you need first; every argument is optional. No network.",
            json!({ "type": "object", "properties": {
                "monthly_pages": { "type": "integer", "description": "Expected pages per month (for monthly cost)." },
                "max_usd_per_1k_pages": { "type": "number", "description": "Highest acceptable list price per 1,000 pages." },
                "error_tolerance": { "type": "string", "enum": ["low", "medium", "high"],
                    "description": "How far below the best benchmark score is still acceptable: low 2, medium 5, high 10 points." },
                "priority": { "type": "string", "enum": ["accuracy", "balanced", "cost", "speed"],
                    "description": "balanced (default): cheapest among those within tolerance of the best." },
                "zdr_required": { "type": "boolean", "description": "Only providers that document zero data retention, and local engines." },
                "local_only": { "type": "boolean", "description": "Documents must not leave the machine: local engines only." },
                "needs": { "type": "array", "items": { "type": "string", "enum": recommend::NEEDS },
                    "description": "What the documents contain that matters." },
                "top_n": { "type": "integer", "minimum": 1, "maximum": 12, "description": "How many to return (default 5)." }
            } }),
            &local,
        ),
        t(
            "run_dataset",
            "Run models over the documents",
            "Step 3. Runs the chosen models over every document in the folder and saves each output. Always call it with \
             dry_run=true first and show the user the estimate; then start it with max_cost_usd (at least the estimate) \
             once they agree. With via=\"keys\" (default) each document goes to the provider of each model using the key \
             in this server's environment, billed to the user. Runs in the background: poll run_status. resume finishes \
             an earlier run's missing or failed calls. For a first pass on long documents use pages (e.g. \"1-3\") or limit.",
            json!({ "type": "object", "properties": {
                "dataset": { "type": "string", "description": "Local folder of documents (absolute path)." },
                "models": { "type": "array", "items": { "type": "string" }, "description": "Parse models, e.g. [\"reducto/standard\", \"llamaparse/cost_effective\"]." },
                "pages": { "type": "string", "description": "Only these pages of each document, e.g. \"1-3\"." },
                "limit": { "type": "integer", "minimum": 1, "description": "Only the first N documents." },
                "filter": { "type": "string", "description": "Only documents whose path contains this." },
                "max_cost_usd": { "type": "number", "description": "Required for a priced run: refuse to start above it, stop when spend reaches it." },
                "dry_run": { "type": "boolean", "description": "Plan and price only; nothing is sent." },
                "via": { "type": "string", "enum": ["keys", "gateway"], "description": "keys: the user's provider keys (default). gateway: a PuffinParse gateway key from the server's environment, whose budget pays." },
                "requirements": { "type": "object", "description": "The requirements given to recommend_models (monthly_pages, …), kept with the run for the analysis." },
                "concurrency": { "type": "integer", "minimum": 1, "maximum": 16, "description": "Calls in flight per model (default 4)." },
                "resume": { "type": "string", "description": "A run folder to finish instead of starting a new run." }
            } }),
            &billed,
        ),
        t(
            "run_status",
            "Check a run",
            "Progress of a run: calls finished and failed per model, spend so far, recent errors, and what to do next. \
             While the run is going it waits (up to wait_seconds, default 30) and returns as soon as the run ends, so \
             call it again instead of sleeping between checks.",
            json!({ "type": "object", "properties": {
                "run": run_prop,
                "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 50, "description": "Wait up to this long for a running run to finish (default 30; 0 returns at once)." }
            }, "required": ["run"] }),
            &local,
        ),
        t(
            "cancel_run",
            "Cancel a run",
            "Stop starting new calls in a run this server is executing; finished outputs are kept.",
            json!({ "type": "object", "properties": { "run": run_prop }, "required": ["run"] }),
            &writes_local,
        ),
        t(
            "analyze_run",
            "Analyze a run",
            "Step 4. Without ground truth: how much the models agree on each document (words and numbers), outliers, \
             failures, empty outputs, latency, real cost per 1,000 pages and at the user's monthly volume, recorded \
             reviews, and a review_queue of documents to verify by hand. Scores against .truth.md references when the \
             dataset has them. write_report saves report.md in the run folder, with your conclusion on top.",
            json!({ "type": "object", "properties": {
                "run": run_prop,
                "write_report": { "type": "boolean" },
                "conclusion": { "type": "string", "description": "Your recommendation, in Markdown, for the top of report.md." },
                "max_docs": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Per-document rows to return (default 20, most disagreement first)." }
            }, "required": ["run"] }),
            &writes_local,
        ),
        t(
            "get_output",
            "Read the models' outputs for a document",
            "Each model's markdown for one document of a run (or the error), plus the original file's path so you can \
             read the page yourself and judge which output is right.",
            json!({ "type": "object", "properties": {
                "run": run_prop,
                "doc": { "type": "string", "description": "Document id (its path inside the dataset folder)." },
                "models": { "type": "array", "items": { "type": "string" }, "description": "Default: every model in the run." },
                "max_chars": { "type": "integer", "minimum": 1, "maximum": MAX_OUTPUT_CHARS }
            }, "required": ["run", "doc"] }),
            &local,
        ),
        t(
            "record_review",
            "Record a verdict",
            "Save your verdict on one model's output for one document after comparing it with the original: correct, \
             minor_errors, major_errors or unusable, with a short note on what was wrong. Reviews feed analyze_run, the \
             report and the viewer.",
            json!({ "type": "object", "properties": {
                "run": run_prop,
                "doc": { "type": "string" },
                "model": { "type": "string" },
                "verdict": { "type": "string", "enum": analyze::VERDICTS },
                "notes": { "type": "string" }
            }, "required": ["run", "doc", "model", "verdict"] }),
            &writes_local,
        ),
        t(
            "open_viewer",
            "Open the visual viewer",
            "Start a local viewer (127.0.0.1 only) for a run and return its link: each original document next to every \
             model's output, rendered, raw or as a word diff, with agreement and reviews; the user can record verdicts \
             there too.",
            json!({ "type": "object", "properties": {
                "run": run_prop,
                "open_browser": { "type": "boolean", "description": "Also open it in the default browser (default false)." }
            }, "required": ["run"] }),
            &local,
        ),
        t(
            "anonymize_dataset",
            "Anonymize documents locally",
            "Optional, before any run: make redacted copies of the documents on this machine. Pages are rendered and \
             read with the local Tesseract engine, and emails, phone numbers, card numbers, IBANs and SSNs (plus dates \
             or ids if asked, and any terms you pass such as names and companies) are blacked out in new image-only \
             PDFs. Nothing is sent anywhere. Needs Tesseract and poppler installed. Check a few redacted pages, then use \
             the output folder as the dataset.",
            json!({ "type": "object", "properties": {
                "dataset": { "type": "string", "description": "Local folder of documents." },
                "out": { "type": "string", "description": "New, empty folder outside the dataset for the redacted copies." },
                "terms": { "type": "array", "items": { "type": "string" }, "description": "Names, companies, addresses or other phrases to black out." },
                "kinds": { "type": "array", "items": { "type": "string", "enum": anonymize::ALL_KINDS },
                    "description": "Patterns to detect (default email, phone, card, iban, ssn)." },
                "dpi": { "type": "integer", "minimum": 72, "maximum": 400, "description": "Rendering resolution (default 150)." }
            }, "required": ["dataset", "out"] }),
            &writes_local,
        ),
    ]
}

// ---- the prompt ----------------------------------------------------------------------------------

pub const PROMPT_NAME: &str = "find_best_model";

const WORKFLOW: &str = "Help me find the best document-parsing model for my own documents with the PuffinParse tools.

1. Call scan_dataset on my folder{DATASET}. Tell me in two lines what is in it.{REQUIREMENTS}
2. Ask me, in one short message, only what you still need: pages per month, budget per 1,000 pages, how many \
errors I can live with, whether documents may leave my machine or need zero data retention, and what matters in \
them (tables, scans, forms, handwriting, ...). If they may hold personal data, offer to anonymize them first with \
anonymize_dataset (then check a few redacted pages and use the redacted folder).
3. Call recommend_models with my answers and show me the top 5 as a short table: why each, price and monthly cost, \
data handling, and whether its key is set. Ask which ones to run.
4. Ask how to pay: my own provider keys (tell me which variables to set in this MCP server's config; never ask me \
to paste a key into the chat), or a PuffinParse gateway key if I have one.
5. Call run_dataset with dry_run=true, show me the estimate, and start it with max_cost_usd only after I say yes. \
For a large folder, suggest a first pass with pages=\"1-3\" or a limit.
6. Call run_status until it is done. Each call waits for the run (up to 30 seconds), so do not sleep or watch files in between.
7. Call analyze_run. For the documents in review_queue (at least five, or all if fewer), read the original file \
yourself, call get_output, compare each model with the page and record_review a verdict with a short note.
8. Call analyze_run with write_report=true and your conclusion, then tell me: the model to use and the runner-up, \
why (errors you saw, agreement, failures), cost per month at my volume, latency, data handling, and anything to \
watch. Call open_viewer and give me the link so I can check the results myself.";

pub fn prompt_definitions() -> Vec<Value> {
    vec![json!({
        "name": PROMPT_NAME,
        "title": "Find the best model for my documents",
        "description": "Walks the agent through scanning your documents, asking a few questions, shortlisting models, \
                        running them with your keys, verifying the results and recommending one.",
        "arguments": [
            { "name": "dataset", "description": "Folder of representative documents (optional).", "required": false },
            { "name": "requirements", "description": "What you already know, e.g. \"20,000 pages a month, under $10 per 1,000 pages, tables and scans, no ZDR needed\" (optional).", "required": false }
        ],
    })]
}

pub fn get_prompt(params: &Map<String, Value>) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).ok_or("prompts/get: \"name\" is required")?;
    if name != PROMPT_NAME {
        return Err(format!("Unknown prompt: {name}"));
    }
    let arg = |k: &str| {
        params.get("arguments").and_then(|a| a.get(k)).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    };
    let dataset = arg("dataset").map(|s| format!(" ({s})")).unwrap_or_default();
    let requirements = arg("requirements")
        .map(|s| format!(" What I already know about my needs: {s}. Use it and skip those questions."))
        .unwrap_or_default();
    Ok(json!({
        "description": "Find the best document-parsing model for your documents",
        "messages": [{ "role": "user", "content": { "type": "text", "text": WORKFLOW.replace("{DATASET}", &dataset).replace("{REQUIREMENTS}", &requirements) } }],
    }))
}
