//! Analysing a run without ground truth, then with the agent's (or the user's) own checks.
//!
//! A user's documents rarely come with reference transcripts, so the analysis leans on three
//! signals, in increasing order of trust:
//!
//! 1. **Agreement between models.** Per document, every pair of successful outputs is compared as a
//!    bag of normalised words (F1) and as a bag of numbers (amounts, dates, ids: the tokens where a
//!    misread costs most). A model far below the others on a document is an *outlier* there; a
//!    document where the models disagree most is where a human or the agent should look first.
//!    Agreement is not accuracy: models can agree on the same mistake, and one correct model can
//!    disagree with three wrong ones.
//! 2. **Reviews.** The agent (reading the original page itself) or the user in the viewer records a
//!    verdict per document and model: `correct`, `minor_errors`, `major_errors` or `unusable`.
//! 3. **Ground truth**, when the dataset has `<file>.truth.md` files: the benchmark's scorer.
//!
//! Plus the plain operational facts: failures, empty outputs, latency, real cost per page and the
//! monthly cost at the user's volume.

use super::run::{self, CallRecord, RunManifest};
use puffinparse_core::bench::{normalize, score, NormalizeOptions};
use puffinparse_core::{self as core, Mode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const VERDICTS: &[&str] = &["correct", "minor_errors", "major_errors", "unusable"];

/// A model whose agreement on a document is this far below the median of the models there is
/// flagged as an outlier on it.
const OUTLIER_GAP: f64 = 0.15;
/// Documents put in the review queue.
const REVIEW_QUEUE: usize = 10;
/// Character-level truth scoring is quadratic; longer documents are scored on words only.
const MAX_CHARS_FOR_CHAR_SCORE: usize = 30_000;

// ---- reviews -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Review {
    pub doc: String,
    pub model: String,
    pub verdict: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// `agent` or `user`.
    pub by: String,
    pub at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ReviewsFile {
    reviews: Vec<Review>,
}

fn reviews_path(dir: &Path) -> std::path::PathBuf {
    dir.join("reviews.json")
}

/// Every review, the last one per (document, model) winning.
pub fn load_reviews(dir: &Path) -> BTreeMap<(String, String), Review> {
    let raw = std::fs::read_to_string(reviews_path(dir)).unwrap_or_default();
    let file: ReviewsFile = serde_json::from_str(&raw).unwrap_or_default();
    file.reviews.into_iter().map(|r| ((r.doc.clone(), r.model.clone()), r)).collect()
}

/// Record a verdict for one document and model of a run.
pub fn record_review(
    dir: &Path,
    doc: &str,
    model: &str,
    verdict: &str,
    notes: &str,
    by: &str,
) -> Result<Review, String> {
    if !VERDICTS.contains(&verdict) {
        return Err(format!("verdict must be one of {}", VERDICTS.join(", ")));
    }
    let manifest = run::load_manifest(dir)?;
    if !manifest.docs.iter().any(|d| d.id == doc) {
        return Err(format!("document '{doc}' is not in run {}", manifest.run_id));
    }
    let model = core::ModelRef::parse_for(model, Mode::Parse).map_err(|e| e.to_string())?.qualified();
    if !manifest.models.contains(&model) {
        return Err(format!("model '{model}' is not in run {} ({})", manifest.run_id, manifest.models.join(", ")));
    }
    let review = Review {
        doc: doc.to_string(),
        model,
        verdict: verdict.to_string(),
        notes: notes.chars().take(2_000).collect(),
        by: by.to_string(),
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    // One writer at a time: the viewer's handlers run concurrently, and the agent (MCP) and the user
    // (viewer, started by the same process) record verdicts on the same run. A unique temp name keeps
    // a writer from renaming another's file.
    static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITE.lock().unwrap_or_else(|p| p.into_inner());
    let mut all = load_reviews(dir);
    all.insert((review.doc.clone(), review.model.clone()), review.clone());
    let file = ReviewsFile { reviews: all.into_values().collect() };
    let path = reviews_path(dir);
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&file).map_err(|e| e.to_string())? + "\n")
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(review)
}

// ---- analysis ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewTally {
    pub reviewed: usize,
    pub correct: usize,
    pub minor_errors: usize,
    pub major_errors: usize,
    pub unusable: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TruthScore {
    pub documents: usize,
    /// Mean character similarity (0–1), over documents short enough to score by character.
    pub char_similarity: Option<f64>,
    /// Mean word F1 (0–1) against the reference.
    pub word_f1: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelReport {
    pub model: String,
    pub verification: &'static str,
    pub documents: usize,
    pub ok: usize,
    pub failed: usize,
    pub empty: usize,
    pub missing: usize,
    pub pages: u32,
    pub cost_usd: f64,
    /// Real spend per 1,000 pages in this run.
    pub usd_per_1k_pages: Option<f64>,
    /// List price per 1,000 pages.
    pub list_usd_per_1k_pages: Option<f64>,
    /// At the user's monthly volume (from the run's requirements), at this run's real cost per page.
    pub monthly_usd: Option<f64>,
    pub latency_p50_ms: Option<u64>,
    pub latency_p95_ms: Option<u64>,
    /// Mean word agreement with the other models (0–1), over documents at least two models parsed.
    pub agreement: Option<f64>,
    /// Mean agreement on numbers alone.
    pub numbers_agreement: Option<f64>,
    /// Documents where this model was the outlier.
    pub outlier_docs: usize,
    pub reviews: ReviewTally,
    pub truth: Option<TruthScore>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocReport {
    pub id: String,
    pub pages: Option<u32>,
    /// 1 − mean pairwise word agreement (0 = every model produced the same words).
    pub disagreement: Option<f64>,
    pub numbers_disagreement: Option<f64>,
    pub outliers: Vec<String>,
    pub failed: Vec<String>,
    pub empty: Vec<String>,
    /// Per model: characters of output (`None` when the call failed or is missing).
    pub chars: BTreeMap<String, Option<usize>>,
    /// Per model: agreement with the others on this document.
    pub agreement: BTreeMap<String, f64>,
    pub reviews: BTreeMap<String, Review>,
    pub has_truth: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Analysis {
    pub run_id: String,
    /// The run folder.
    pub run_dir: String,
    pub status: run::RunStatus,
    pub dataset_root: String,
    pub created_at: String,
    pub via: String,
    pub pages_selection: Option<String>,
    pub models: Vec<String>,
    pub documents: usize,
    pub monthly_pages: Option<u64>,
    pub per_model: Vec<ModelReport>,
    pub per_doc: Vec<DocReport>,
    /// Documents to check by hand first: failures, then the largest disagreements, unreviewed only.
    pub review_queue: Vec<String>,
    pub notes: Vec<String>,
}

/// Tokens for agreement: normalised words, and the subset that carry digits.
struct Tokens {
    words: HashMap<String, usize>,
    numbers: HashMap<String, usize>,
}

fn tokens(markdown: &str) -> Tokens {
    let norm = normalize(markdown, NormalizeOptions::default());
    let mut words = HashMap::new();
    let mut numbers = HashMap::new();
    for w in norm.split_whitespace() {
        *words.entry(w.to_string()).or_insert(0) += 1;
        if w.chars().any(|c| c.is_ascii_digit()) {
            let n: String = w.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
            if !n.is_empty() {
                *numbers.entry(n).or_insert(0) += 1;
            }
        }
    }
    Tokens { words, numbers }
}

/// Multiset F1 of two bags; `None` when both are empty.
fn bag_f1(a: &HashMap<String, usize>, b: &HashMap<String, usize>) -> Option<f64> {
    let (na, nb): (usize, usize) = (a.values().sum(), b.values().sum());
    if na == 0 && nb == 0 {
        return None;
    }
    if na == 0 || nb == 0 {
        return Some(0.0);
    }
    let common: usize = a.iter().map(|(k, v)| (*v).min(b.get(k).copied().unwrap_or(0))).sum();
    Some(2.0 * common as f64 / (na + nb) as f64)
}

fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}

fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

fn percentile(xs: &mut [u64], p: f64) -> Option<u64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_unstable();
    let idx = ((xs.len() as f64 - 1.0) * p).round() as usize;
    Some(xs[idx.min(xs.len() - 1)])
}

fn round(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

pub fn analyze(dir: &Path) -> Result<Analysis, String> {
    let manifest = run::load_manifest(dir)?;
    let results = run::load_results(dir);
    let reviews = load_reviews(dir);
    Ok(analyze_with(dir, &manifest, &results, &reviews, true))
}

fn read_output(dir: &Path, r: &CallRecord) -> Option<String> {
    if !r.ok {
        return None;
    }
    std::fs::read_to_string(run::output_path(dir, &r.model, &r.doc)).ok()
}

pub fn analyze_with(
    dir: &Path,
    manifest: &RunManifest,
    results: &BTreeMap<(String, String), CallRecord>,
    reviews: &BTreeMap<(String, String), Review>,
    live_status: bool,
) -> Analysis {
    let models = &manifest.models;
    let monthly_pages =
        manifest.requirements.as_ref().and_then(|r| r.get("monthly_pages")).and_then(serde_json::Value::as_u64);
    let root = Path::new(&manifest.dataset_root);

    let mut agreement_sum: HashMap<&str, Vec<f64>> = HashMap::new();
    let mut numbers_sum: HashMap<&str, Vec<f64>> = HashMap::new();
    let mut outlier_count: HashMap<&str, usize> = HashMap::new();
    let mut truth_scores: HashMap<&str, (Vec<f64>, Vec<f64>)> = HashMap::new();
    let mut per_doc = Vec::new();

    for doc in &manifest.docs {
        let mut toks: Vec<(&str, Tokens)> = Vec::new();
        let mut chars = BTreeMap::new();
        let mut failed = Vec::new();
        let mut empty = Vec::new();
        let truth = doc.truth.as_ref().and_then(|t| std::fs::read_to_string(root.join(t)).ok());
        for m in models {
            let rec = results.get(&(m.clone(), doc.id.clone()));
            match rec {
                Some(r) if r.ok => {
                    chars.insert(m.clone(), Some(r.chars));
                    if r.empty {
                        empty.push(m.clone());
                    }
                    if let Some(text) = read_output(dir, r) {
                        if let Some(t) = &truth {
                            let e = truth_scores.entry(m.as_str()).or_default();
                            let wf1 = bag_f1(&tokens(&text).words, &tokens(t).words).unwrap_or(1.0);
                            e.1.push(wf1);
                            if text.len().max(t.len()) <= MAX_CHARS_FOR_CHAR_SCORE {
                                e.0.push(score(&text, t, NormalizeOptions::default()).char_similarity);
                            }
                        }
                        toks.push((m.as_str(), tokens(&text)));
                    }
                }
                Some(_) => {
                    chars.insert(m.clone(), None);
                    failed.push(m.clone());
                }
                None => {
                    chars.insert(m.clone(), None);
                }
            }
        }
        let mut doc_agreement = BTreeMap::new();
        let mut pair_words = Vec::new();
        let mut pair_numbers = Vec::new();
        let mut own: HashMap<&str, (Vec<f64>, Vec<f64>)> = HashMap::new();
        for i in 0..toks.len() {
            for j in (i + 1)..toks.len() {
                if let Some(w) = bag_f1(&toks[i].1.words, &toks[j].1.words) {
                    pair_words.push(w);
                    own.entry(toks[i].0).or_default().0.push(w);
                    own.entry(toks[j].0).or_default().0.push(w);
                }
                if let Some(n) = bag_f1(&toks[i].1.numbers, &toks[j].1.numbers) {
                    pair_numbers.push(n);
                    own.entry(toks[i].0).or_default().1.push(n);
                    own.entry(toks[j].0).or_default().1.push(n);
                }
            }
        }
        for (m, (w, n)) in &own {
            if let Some(a) = mean(w) {
                doc_agreement.insert(m.to_string(), round(a, 3));
                agreement_sum.entry(m).or_default().push(a);
            }
            if let Some(a) = mean(n) {
                numbers_sum.entry(m).or_default().push(a);
            }
        }
        let mut outliers = Vec::new();
        if doc_agreement.len() >= 3 {
            let vals: Vec<f64> = doc_agreement.values().copied().collect();
            let med = median(&vals).unwrap_or(1.0);
            for (m, a) in &doc_agreement {
                if *a < med - OUTLIER_GAP {
                    outliers.push(m.clone());
                    *outlier_count
                        .entry(models.iter().find(|x| *x == m).map(String::as_str).unwrap_or(""))
                        .or_insert(0) += 1;
                }
            }
        }
        let doc_reviews: BTreeMap<String, Review> =
            reviews.iter().filter(|((d, _), _)| d == &doc.id).map(|((_, m), r)| (m.clone(), r.clone())).collect();
        per_doc.push(DocReport {
            id: doc.id.clone(),
            pages: doc.pages,
            disagreement: mean(&pair_words).map(|a| round(1.0 - a, 3)),
            numbers_disagreement: mean(&pair_numbers).map(|a| round(1.0 - a, 3)),
            outliers,
            failed,
            empty,
            chars,
            agreement: doc_agreement,
            reviews: doc_reviews,
            has_truth: truth.is_some(),
        });
    }

    let mut per_model = Vec::new();
    for m in models {
        let recs: Vec<&CallRecord> = results.values().filter(|r| &r.model == m).collect();
        let ok: Vec<&&CallRecord> = recs.iter().filter(|r| r.ok).collect();
        let pages: u32 = ok.iter().map(|r| r.pages).sum();
        let cost: f64 = recs.iter().filter_map(|r| r.cost_usd).sum();
        let mut lat: Vec<u64> = ok.iter().map(|r| r.latency_ms).collect();
        let per_1k =
            (pages > 0 && ok.iter().any(|r| r.cost_usd.is_some())).then(|| round(cost / pages as f64 * 1000.0, 2));
        let list = core::pricing::price_per_page(m, Mode::Parse).map(|p| round(p * 1000.0, 2));
        let unit = per_1k.or(list);
        let mut tally = ReviewTally::default();
        for r in reviews.values().filter(|r| &r.model == m) {
            tally.reviewed += 1;
            match r.verdict.as_str() {
                "correct" => tally.correct += 1,
                "minor_errors" => tally.minor_errors += 1,
                "major_errors" => tally.major_errors += 1,
                _ => tally.unusable += 1,
            }
        }
        let truth = truth_scores.get(m.as_str()).filter(|(_, w)| !w.is_empty()).map(|(c, w)| TruthScore {
            documents: w.len(),
            char_similarity: mean(c).map(|x| round(x, 3)),
            word_f1: round(mean(w).unwrap_or(0.0), 3),
        });
        per_model.push(ModelReport {
            model: m.clone(),
            verification: super::recommend::verification(m.split('/').next().unwrap_or("")),
            documents: manifest.docs.len(),
            ok: ok.len(),
            failed: recs.len() - ok.len(),
            empty: ok.iter().filter(|r| r.empty).count(),
            missing: manifest.docs.len() - recs.len(),
            pages,
            cost_usd: round(cost, 4),
            usd_per_1k_pages: per_1k,
            list_usd_per_1k_pages: list,
            monthly_usd: match (monthly_pages, unit) {
                (Some(n), Some(u)) => Some(round(n as f64 * u / 1000.0, 2)),
                _ => None,
            },
            latency_p50_ms: percentile(&mut lat, 0.5),
            latency_p95_ms: percentile(&mut lat, 0.95),
            agreement: agreement_sum.get(m.as_str()).and_then(|v| mean(v)).map(|a| round(a, 3)),
            numbers_agreement: numbers_sum.get(m.as_str()).and_then(|v| mean(v)).map(|a| round(a, 3)),
            outlier_docs: outlier_count.get(m.as_str()).copied().unwrap_or(0),
            reviews: tally,
            truth,
        });
    }

    let mut queue: Vec<&DocReport> = per_doc.iter().filter(|d| d.reviews.is_empty()).collect();
    queue.sort_by(|a, b| {
        let fa = !a.failed.is_empty() || !a.empty.is_empty();
        let fb = !b.failed.is_empty() || !b.empty.is_empty();
        fb.cmp(&fa).then(b.disagreement.unwrap_or(0.0).total_cmp(&a.disagreement.unwrap_or(0.0)))
    });
    let review_queue = queue.iter().take(REVIEW_QUEUE).map(|d| d.id.clone()).collect();

    let mut notes = vec![
        "Agreement compares each model's words (and numbers) with the other models' on the same document. It finds \
         where to look; it is not accuracy. Check the documents in review_queue against the original pages and \
         record a verdict for each model."
            .to_string(),
    ];
    if models.len() < 3 {
        notes.push("With fewer than three models there is no majority, so no model is flagged as an outlier.".into());
    }
    if per_doc.iter().any(|d| d.has_truth) {
        notes.push("Documents with a .truth.md reference are also scored against it (truth).".into());
    }
    if monthly_pages.is_none() {
        notes.push("Pass monthly_pages in the run's requirements to project a monthly cost.".into());
    }
    Analysis {
        run_id: manifest.run_id.clone(),
        run_dir: dir.to_string_lossy().into_owned(),
        status: run::effective_status(manifest, live_status),
        dataset_root: manifest.dataset_root.clone(),
        created_at: manifest.created_at.clone(),
        via: manifest.via.clone(),
        pages_selection: manifest.pages.clone(),
        models: models.clone(),
        documents: manifest.docs.len(),
        monthly_pages,
        per_model,
        per_doc,
        review_queue,
        notes,
    }
}

// ---- report --------------------------------------------------------------------------------------

fn pct(x: Option<f64>) -> String {
    x.map(|v| format!("{:.1}%", v * 100.0)).unwrap_or_else(|| "–".into())
}

fn usd(x: Option<f64>) -> String {
    x.map(|v| format!("${v:.2}")).unwrap_or_else(|| "–".into())
}

fn secs(ms: Option<u64>) -> String {
    ms.map(|m| format!("{:.1} s", m as f64 / 1000.0)).unwrap_or_else(|| "–".into())
}

/// The analysis as Markdown. `conclusion` is the agent's own recommendation, put on top.
pub fn render_report(a: &Analysis, conclusion: Option<&str>) -> String {
    let mut s = format!(
        "# Model comparison on your documents\n\nRun `{}` · {} documents · {} models · {} · paid with {}{}\n\n",
        a.run_id,
        a.documents,
        a.models.len(),
        a.created_at,
        if a.via == "gateway" { "a gateway key" } else { "your own provider keys" },
        a.pages_selection.as_ref().map(|p| format!(" · pages {p} of each document")).unwrap_or_default()
    );
    if let Some(c) = conclusion.filter(|c| !c.trim().is_empty()) {
        s.push_str("## Recommendation\n\n");
        s.push_str(c.trim());
        s.push_str("\n\n");
    }
    s.push_str("## Results\n\n");
    let has_truth = a.per_model.iter().any(|m| m.truth.is_some());
    s.push_str("| Model | OK | Failed | Agreement | Numbers | Reviewed correct |");
    if has_truth {
        s.push_str(" Truth (words) |");
    }
    s.push_str(" p50 | p95 | $/1k pages | Monthly | Status |\n|---|---:|---:|---:|---:|---:|");
    if has_truth {
        s.push_str("---:|");
    }
    s.push_str("---:|---:|---:|---:|---|\n");
    for m in &a.per_model {
        let reviewed =
            if m.reviews.reviewed > 0 { format!("{}/{}", m.reviews.correct, m.reviews.reviewed) } else { "–".into() };
        s.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |",
            m.model,
            m.ok,
            m.failed + m.missing,
            pct(m.agreement),
            pct(m.numbers_agreement),
            reviewed
        ));
        if has_truth {
            s.push_str(&format!(" {} |", pct(m.truth.as_ref().map(|t| t.word_f1))));
        }
        s.push_str(&format!(
            " {} | {} | {} | {} | {} |\n",
            secs(m.latency_p50_ms),
            secs(m.latency_p95_ms),
            usd(m.usd_per_1k_pages.or(m.list_usd_per_1k_pages)),
            usd(m.monthly_usd),
            m.verification
        ));
    }
    if let Some(n) = a.monthly_pages {
        s.push_str(&format!("\nMonthly cost at {n} pages per month, at this run's cost per page.\n"));
    }
    let reviewed: Vec<&DocReport> = a.per_doc.iter().filter(|d| !d.reviews.is_empty()).collect();
    if !reviewed.is_empty() {
        s.push_str("\n## Checked by hand\n\n| Document | Model | Verdict | Notes |\n|---|---|---|---|\n");
        for d in reviewed {
            for (m, r) in &d.reviews {
                s.push_str(&format!(
                    "| `{}` | `{m}` | {} | {} |\n",
                    d.id,
                    r.verdict.replace('_', " "),
                    r.notes.replace('|', "\\|").replace('\n', " ")
                ));
            }
        }
    }
    let mut hard: Vec<&DocReport> = a.per_doc.iter().filter(|d| d.disagreement.is_some()).collect();
    hard.sort_by(|x, y| y.disagreement.unwrap_or(0.0).total_cmp(&x.disagreement.unwrap_or(0.0)));
    if !hard.is_empty() {
        s.push_str("\n## Where the models disagree most\n\n| Document | Disagreement | Numbers | Outliers | Failed |\n|---|---:|---:|---|---|\n");
        for d in hard.iter().take(10) {
            s.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                d.id,
                pct(d.disagreement),
                pct(d.numbers_disagreement),
                d.outliers.join(", "),
                d.failed.join(", ")
            ));
        }
    }
    s.push_str(
        "\n## How to read this\n\n- **Agreement**: share of words a model has in common with the other models on the same \
         document, averaged. **Numbers**: the same for tokens with digits (amounts, dates, ids). High agreement means \
         the models read the same thing; it is not proof that any of them read it right.\n- **Reviewed correct**: \
         documents checked against the original page and judged correct, out of those checked.\n- Cost is what the \
         providers reported, or list price where they report none.\n",
    );
    s.push_str(&format!("\nInspect every document and output: `puffinparse eval view {}`\n", a.run_dir));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::dataset::{tempdir, tiny_pdf};
    use crate::eval::run::{CallRecord, RunSpec, Via};

    fn setup(outputs: &[(&str, &str, Option<&str>)]) -> std::path::PathBuf {
        let dir = tempdir("analyze");
        for d in ["a.pdf", "b.pdf"] {
            std::fs::write(dir.join(d), tiny_pdf(1)).unwrap();
        }
        std::fs::write(dir.join("a.pdf.truth.md"), "Invoice 1234 total 56.78 paid").unwrap();
        let spec = RunSpec {
            dataset: dir.clone(),
            models: vec!["reducto".into(), "extend".into(), "llamaparse".into()],
            pages: None,
            limit: None,
            filter: None,
            max_cost_usd: Some(10.0),
            out_dir: None,
            requirements: Some(serde_json::json!({ "monthly_pages": 10000 })),
        };
        let (_, m) = run::plan(&spec, &Via::Keys).unwrap();
        let prep = run::prepare_new(&spec, &Via::Keys, m).unwrap();
        let mut log = String::new();
        for (model, doc, text) in outputs {
            let q = core::ModelRef::parse_for(model, Mode::Parse).unwrap().qualified();
            let rec = match text {
                Some(t) => {
                    let p = run::output_path(&prep.dir, &q, doc);
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    std::fs::write(&p, t).unwrap();
                    CallRecord {
                        model: q,
                        doc: doc.to_string(),
                        ok: true,
                        pages: 1,
                        latency_ms: 1000,
                        cost_usd: Some(0.01),
                        chars: t.len(),
                        empty: t.trim().is_empty(),
                        ..Default::default()
                    }
                }
                None => CallRecord {
                    model: q,
                    doc: doc.to_string(),
                    ok: false,
                    error: Some("boom".into()),
                    error_kind: Some("provider".into()),
                    ..Default::default()
                },
            };
            log.push_str(&serde_json::to_string(&rec).unwrap());
            log.push('\n');
        }
        std::fs::write(run::results_path(&prep.dir), log).unwrap();
        prep.dir
    }

    #[test]
    fn agreement_flags_the_outlier_and_queues_the_hardest_documents() {
        let dir = setup(&[
            ("reducto", "a.pdf", Some("Invoice 1234 total 56.78 paid")),
            ("extend", "a.pdf", Some("Invoice 1234 total 56.78 paid")),
            ("llamaparse", "a.pdf", Some("lnvoice l234 tota1 5678 pald")),
            ("reducto", "b.pdf", Some("hello world")),
            ("extend", "b.pdf", None),
            ("llamaparse", "b.pdf", Some("hello world")),
        ]);
        let a = analyze(&dir).unwrap();
        let doc_a = a.per_doc.iter().find(|d| d.id == "a.pdf").unwrap();
        assert_eq!(doc_a.outliers, ["llamaparse/cost_effective"]);
        assert!(doc_a.has_truth);
        let lp = a.per_model.iter().find(|m| m.model == "llamaparse/cost_effective").unwrap();
        assert_eq!(lp.outlier_docs, 1);
        assert!(lp.truth.as_ref().unwrap().word_f1 < 0.5);
        let rd = a.per_model.iter().find(|m| m.model == "reducto/standard").unwrap();
        assert_eq!(rd.truth.as_ref().unwrap().word_f1, 1.0);
        assert_eq!(rd.truth.as_ref().unwrap().char_similarity, Some(1.0));
        assert_eq!(rd.usd_per_1k_pages, Some(10.0));
        assert_eq!(rd.monthly_usd, Some(100.0));
        let ex = a.per_model.iter().find(|m| m.model == "extend/parse_performance").unwrap();
        assert_eq!((ex.ok, ex.failed), (1, 1));
        // The failed document first, then the most disagreed-on one.
        assert_eq!(a.review_queue, ["b.pdf", "a.pdf"]);

        // A review takes a document out of the queue and shows in the tally and the report.
        record_review(&dir, "a.pdf", "llamaparse", "major_errors", "misreads digits", "agent").unwrap();
        assert!(record_review(&dir, "a.pdf", "llamaparse", "great", "", "agent").is_err());
        assert!(record_review(&dir, "zzz.pdf", "llamaparse", "correct", "", "agent").is_err());
        assert!(record_review(&dir, "a.pdf", "mistral", "correct", "", "agent").is_err());
        let a = analyze(&dir).unwrap();
        assert_eq!(a.review_queue, ["b.pdf"]);
        let lp = a.per_model.iter().find(|m| m.model == "llamaparse/cost_effective").unwrap();
        assert_eq!(lp.reviews.major_errors, 1);
        let report = render_report(&a, Some("Use reducto/standard."));
        assert!(report.contains("## Recommendation\n\nUse reducto/standard."));
        assert!(report.contains("misreads digits"));
        assert!(report.contains("| `reducto/standard` | 2 | 0 |"));
    }

    #[test]
    fn bag_f1_handles_empty_sides() {
        let t = |s: &str| tokens(s).words;
        assert_eq!(bag_f1(&t(""), &t("")), None);
        assert_eq!(bag_f1(&t("a b"), &t("")), Some(0.0));
        assert_eq!(bag_f1(&t("a b c d"), &t("a b x y")), Some(0.5));
        assert_eq!(tokens("Total: $56.78 on 2026-10-12").numbers.len(), 2);
    }
}
