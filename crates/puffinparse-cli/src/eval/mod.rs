//! `puffinparse eval`: find the best model for *your* documents.
//!
//! The same engine backs the CLI subcommands here and the MCP tools an agent calls
//! (`scan_dataset`, `recommend_models`, `run_dataset`, `analyze_run`, …; see `mcp/eval_tools.rs`):
//!
//! 1. [`dataset`]: a folder of documents is the dataset; no manifest, ground truth optional.
//! 2. [`recommend`]: a shortlist from the user's requirements, the benchmark, prices and each
//!    provider's documented data handling.
//! 3. [`run`]: the shortlist over the dataset, priced before the first call, with the user's own keys
//!    (or a gateway key), resumable.
//! 4. [`analyze`]: agreement between models, failures, latency, real cost, reviews, a report.
//! 5. [`view`]: a localhost viewer with each document next to every model's output.
//! 6. [`anonymize`]: optional local redaction of personal data before any provider sees a page.

pub mod analyze;
pub mod anonymize;
pub mod dataset;
pub mod recommend;
pub mod run;
pub mod view;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// Count the documents, pages and types in a folder (no network).
    Scan {
        dataset: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Shortlist models for your requirements from the benchmark, prices and data handling.
    Recommend(RecommendArgs),
    /// Run models over a folder of documents and save every output (see `eval analyze`).
    Run(RunArgs),
    /// Agreement, failures, latency, cost and reviews of a run; optionally write report.md.
    Analyze {
        /// The run folder (holds run.json).
        run: PathBuf,
        #[arg(long)]
        json: bool,
        /// Write report.md into the run folder.
        #[arg(long)]
        write_report: bool,
    },
    /// Record a verdict for one document and model of a run.
    Review {
        run: PathBuf,
        #[arg(long)]
        doc: String,
        #[arg(long)]
        model: String,
        /// correct | minor_errors | major_errors | unusable
        #[arg(long)]
        verdict: String,
        #[arg(long, default_value = "")]
        notes: String,
    },
    /// Open a local viewer: each document next to every model's output.
    View {
        run: PathBuf,
        /// Port on 127.0.0.1 (default: any free port).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Print the address instead of opening a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Make redacted copies of a dataset locally (Tesseract), before any provider sees it.
    Anonymize(AnonymizeArgs),
}

#[derive(clap::Args, Debug)]
pub struct RecommendArgs {
    /// Pages per month you expect to process (for the monthly cost).
    #[arg(long)]
    monthly_pages: Option<u64>,
    /// Highest acceptable list price, USD per 1,000 pages.
    #[arg(long)]
    max_usd_per_1k: Option<f64>,
    /// low | medium | high: how many benchmark points below the best still count as good enough.
    #[arg(long, default_value = "medium")]
    tolerance: String,
    /// accuracy | balanced | cost | speed
    #[arg(long, default_value = "balanced")]
    priority: String,
    /// Only providers that offer zero data retention (or local engines).
    #[arg(long)]
    zdr: bool,
    /// Only local / self-hosted engines.
    #[arg(long)]
    local_only: bool,
    /// What the documents need: tables, scans, forms, equations, multi_column, charts, dense_text,
    /// handwriting, non_english. Repeatable or comma-separated.
    #[arg(long, value_delimiter = ',')]
    needs: Vec<String>,
    #[arg(long, default_value_t = 5)]
    top: usize,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Folder of documents (or one document).
    dataset: Option<PathBuf>,
    /// Models ("provider/model"); repeatable or comma-separated.
    #[arg(short, long, value_delimiter = ',')]
    models: Vec<String>,
    /// Only these pages of each document, e.g. "1-3".
    #[arg(long)]
    pages: Option<String>,
    /// Only the first N documents.
    #[arg(long)]
    limit: Option<usize>,
    /// Only documents whose path contains this.
    #[arg(long)]
    filter: Option<String>,
    /// Concurrent calls per model.
    #[arg(short, long, default_value_t = 4)]
    concurrency: usize,
    /// Refuse to start above this estimate, and stop when the spend reaches it (USD).
    #[arg(long, value_name = "USD")]
    max_cost: Option<f64>,
    /// Pages per month you expect (recorded so `analyze` can project a monthly cost).
    #[arg(long)]
    monthly_pages: Option<u64>,
    /// Print the plan and estimate, make no call.
    #[arg(long)]
    dry_run: bool,
    /// Finish an earlier run (its folder): only missing or failed calls are made.
    #[arg(long, value_name = "RUN_DIR", conflicts_with = "dataset")]
    resume: Option<PathBuf>,
    /// Pay through a PuffinParse gateway (PUFFINPARSE_GATEWAY_URL + PUFFINPARSE_GATEWAY_KEY)
    /// instead of your own provider keys.
    #[arg(long)]
    gateway: bool,
    /// Parent folder for the run (default: <dataset>/.puffinparse/runs).
    #[arg(long)]
    out_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 300.0)]
    timeout: f64,
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
}

#[derive(clap::Args, Debug)]
pub struct AnonymizeArgs {
    /// Folder of documents (or one document).
    dataset: PathBuf,
    /// Where to write the redacted copies (must not be inside the dataset).
    #[arg(long)]
    out: PathBuf,
    /// Extra words or phrases to black out (names, company names, addresses). Repeatable.
    #[arg(long = "term")]
    terms: Vec<String>,
    /// Kinds of personal data to detect: email, phone, card, iban, ssn, date, id (default: email, phone, card, iban, ssn).
    #[arg(long, value_delimiter = ',')]
    kinds: Vec<String>,
    /// Rendering resolution for PDF pages.
    #[arg(long, default_value_t = 150)]
    dpi: u32,
}

pub async fn run(cmd: EvalCommand) -> Result<()> {
    match cmd {
        EvalCommand::Scan { dataset, json } => {
            let ds = dataset::scan(&dataset).map_err(|e| anyhow!(e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&ds)?);
            } else {
                println!(
                    "{}: {} documents, {} pages{}",
                    ds.root.display(),
                    ds.docs.len(),
                    ds.total_pages(),
                    if ds.unknown_pages() > 0 {
                        format!(" ({} with an unknown page count, counted as 1)", ds.unknown_pages())
                    } else {
                        String::new()
                    }
                );
                for s in &ds.skipped {
                    println!("  skipped {}: {}", s.path, s.reason);
                }
            }
            Ok(())
        }
        EvalCommand::Recommend(a) => {
            let req = recommend::Requirements {
                monthly_pages: a.monthly_pages,
                max_usd_per_1k_pages: a.max_usd_per_1k,
                error_tolerance: serde_json::from_value(serde_json::json!(a.tolerance))
                    .map_err(|_| anyhow!("--tolerance must be low, medium or high"))?,
                priority: serde_json::from_value(serde_json::json!(a.priority))
                    .map_err(|_| anyhow!("--priority must be accuracy, balanced, cost or speed"))?,
                zdr_required: a.zdr,
                local_only: a.local_only,
                needs: a.needs,
            };
            let r = recommend::recommend(&req, a.top.max(1), &|_| true);
            if a.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                for c in &r.candidates {
                    println!(
                        "{}. {:<34} {:>8} {:>10}  {}",
                        c.rank,
                        c.model,
                        c.quality.map(|q| format!("{q:.1}")).unwrap_or_else(|| "untested".into()),
                        c.usd_per_1k_pages.map(|p| format!("${p}/1k")).unwrap_or_else(|| "n/a".into()),
                        c.reasons.join("; ")
                    );
                }
                for n in &r.notes {
                    println!("note: {n}");
                }
            }
            Ok(())
        }
        EvalCommand::Run(a) => run_cmd(a).await,
        EvalCommand::Analyze { run, json, write_report } => {
            let a = analyze::analyze(&run).map_err(|e| anyhow!(e))?;
            if write_report {
                let path = run.join("report.md");
                std::fs::write(&path, analyze::render_report(&a, None))?;
                eprintln!("wrote {}", path.display());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&a)?);
            } else {
                print!("{}", analyze::render_report(&a, None));
            }
            Ok(())
        }
        EvalCommand::Review { run, doc, model, verdict, notes } => {
            let r = analyze::record_review(&run, &doc, &model, &verdict, &notes, "user").map_err(|e| anyhow!(e))?;
            println!("recorded: {} on {} is {}", r.model, r.doc, r.verdict);
            Ok(())
        }
        EvalCommand::View { run, port, no_open } => {
            let v = view::start(&run, port).await.map_err(|e| anyhow!(e))?;
            eprintln!("PuffinParse viewer for {}: {}", run.display(), v.url);
            eprintln!("Serving on 127.0.0.1 only. Press Ctrl-C to stop.");
            if !no_open {
                view::open_browser(&v.url);
            }
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        EvalCommand::Anonymize(a) => {
            let opts = anonymize::Options { terms: a.terms, kinds: a.kinds, dpi: a.dpi };
            let report = anonymize::anonymize(&a.dataset, &a.out, &opts).await.map_err(|e| anyhow!(e))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

async fn run_cmd(a: RunArgs) -> Result<()> {
    let via = if a.gateway { run::Via::gateway_from_env().map_err(|e| anyhow!(e))? } else { run::Via::Keys };
    let opts = run::ExecOptions { concurrency: a.concurrency, timeout_secs: a.timeout, max_retries: a.max_retries };
    let prep = if let Some(dir) = &a.resume {
        run::prepare_resume(dir, &via, a.max_cost).map_err(|e| anyhow!(e))?
    } else {
        let Some(dataset) = a.dataset.clone() else { bail!("name a dataset folder, or --resume a run") };
        let spec = run::RunSpec {
            dataset,
            models: a.models.clone(),
            pages: a.pages.clone(),
            limit: a.limit,
            filter: a.filter.clone(),
            max_cost_usd: a.max_cost,
            out_dir: a.out_dir.clone(),
            requirements: a.monthly_pages.map(|n| serde_json::json!({ "monthly_pages": n })),
        };
        let (_, manifest) = run::plan(&spec, &via).map_err(|e| anyhow!(e))?;
        let e = &manifest.estimate;
        eprintln!(
            "{} documents x {} models = {} calls, estimated ${:.2} at list price{}",
            e.documents,
            manifest.models.len(),
            e.calls,
            e.total_usd,
            if e.unpriced_models.is_empty() {
                String::new()
            } else {
                format!(" (unpriced: {})", e.unpriced_models.join(", "))
            }
        );
        for (m, me) in &e.per_model {
            eprintln!(
                "  {m:<34} {:>6} pages  {}",
                me.pages,
                me.usd.map(|u| format!("${u:.2}")).unwrap_or_else(|| "n/a".into())
            );
        }
        if a.dry_run {
            return Ok(());
        }
        if a.max_cost.is_none() && e.total_usd > 0.0 {
            bail!("pass --max-cost (at least ${:.2}) to start this run", e.total_usd);
        }
        run::prepare_new(&spec, &via, manifest).map_err(|e| anyhow!(e))?
    };
    let dir = prep.dir.clone();
    let total = prep.remaining_calls();
    eprintln!("run {} -> {}", prep.manifest.run_id, dir.display());
    let progress = Arc::new(run::Progress::default());
    let bar = indicatif::ProgressBar::new(total as u64);
    let ticker = {
        let progress = progress.clone();
        let bar = bar.clone();
        let start = progress.snapshot().done;
        tokio::spawn(async move {
            loop {
                let s = progress.snapshot();
                bar.set_position(s.done.saturating_sub(start) as u64);
                bar.set_message(format!("${:.2} spent, {} failed", s.spent_usd, s.failed));
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
    };
    let caller = run::caller(&via);
    let manifest = run::execute(prep, caller, opts, progress.clone()).await;
    ticker.abort();
    bar.finish_and_clear();
    let s = progress.snapshot();
    eprintln!("{:?}: {} calls, {} failed, ${:.2} spent", manifest.status, s.done, s.failed, s.spent_usd);
    if let Some(n) = &manifest.note {
        eprintln!("stopped: {n}");
    }
    eprintln!("next: puffinparse eval analyze {0}   |   puffinparse eval view {0}", dir.display());
    Ok(())
}
